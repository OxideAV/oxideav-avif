//! One encode or one decode per process, through the contract API or
//! the registry path, so the wall clock printed here and the peak RSS
//! of the process (`/usr/bin/time -l` on macOS, `-v` on Linux) belong
//! to that one operation. The process holds the source buffer (encode)
//! or the file (decode) and nothing else before the clock starts.
//!
//! ```text
//! cargo run --release --example path_bench -- encode \
//!     --api contract|registry --raw photo.rgb24 --size 4032x3024 \
//!     --fmt rgb24|rgba|yuv420p [--lossless] [--quality 60] [--threads 8] \
//!     [--frames N [--all-intra]] [--out out.avif]
//! cargo run --release --example path_bench -- decode \
//!     --api contract|registry --in out.avif [--threads 8] [--rgba]
//! ```
//!
//! `--frames N` (contract encode of `yuv420p`) codes an `avis` image
//! sequence of `N` frames through `encode_all`: the picture panned two
//! columns per frame, 40 ms each — KEY + P groups, or KEY frames only
//! with `--all-intra`.
//!
//! `--rgba` (decode) also converts the decoded picture to packed RGBA
//! (`to_rgba8_threads` on the same budget), the buffer a contract
//! caller usually wants.

use std::time::Instant;

use oxideav_avif::{
    AvifImage, AvifPixelFormat, AvifPlane, CODEC_ID_STR, DecodeOptions, EncodeOptions,
};
use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, ExecutionContext, Frame, Packet, PixelFormat, TimeBase,
};

#[derive(Default)]
struct Args {
    op: String,
    api: String,
    raw: String,
    input: String,
    out: Option<String>,
    width: u32,
    height: u32,
    fmt: String,
    lossless: bool,
    quality: Option<u8>,
    threads: usize,
    rgba: bool,
    frames: usize,
    all_intra: bool,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        op: it.next().expect("encode | decode"),
        api: "contract".into(),
        fmt: "rgb24".into(),
        threads: 1,
        ..Args::default()
    };
    while let Some(k) = it.next() {
        match k.as_str() {
            "--api" => a.api = it.next().expect("--api contract|registry"),
            "--raw" => a.raw = it.next().expect("--raw PATH"),
            "--in" => a.input = it.next().expect("--in PATH"),
            "--out" => a.out = it.next(),
            "--size" => {
                let s = it.next().expect("--size WxH");
                let (w, h) = s.split_once('x').expect("WxH");
                a.width = w.parse().expect("width");
                a.height = h.parse().expect("height");
            }
            "--fmt" => a.fmt = it.next().expect("--fmt"),
            "--lossless" => a.lossless = true,
            "--quality" => a.quality = Some(it.next().expect("--quality N").parse().expect("N")),
            "--threads" => a.threads = it.next().expect("--threads N").parse().expect("N"),
            "--rgba" => a.rgba = true,
            "--frames" => a.frames = it.next().expect("--frames N").parse().expect("N"),
            "--all-intra" => a.all_intra = true,
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

fn yuv420_planes(raw: Vec<u8>, w: usize, h: usize) -> [(usize, Vec<u8>); 3] {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (y, c) = (w * h, cw * ch);
    assert_eq!(raw.len(), y + 2 * c, "raw size is not {w}x{h} 4:2:0");
    [
        (w, raw[..y].to_vec()),
        (cw, raw[y..y + c].to_vec()),
        (cw, raw[y + c..].to_vec()),
    ]
}

fn encode(a: &Args) {
    let raw = std::fs::read(&a.raw).expect("read --raw");
    let (w, h) = (a.width as usize, a.height as usize);
    let (secs, bytes) = if a.api == "contract" {
        let mut opts = EncodeOptions::default()
            .with_quality(a.quality.unwrap_or(oxideav_avif::DEFAULT_QUALITY))
            .with_threads(a.threads);
        if a.lossless {
            opts = opts.with_lossless(true);
        }
        match a.fmt.as_str() {
            "rgb24" => {
                let t = Instant::now();
                let out =
                    oxideav_avif::encode_rgb8(a.width, a.height, &raw, &opts).expect("encode");
                (t.elapsed().as_secs_f64(), out)
            }
            "rgba" => {
                let t = Instant::now();
                let out =
                    oxideav_avif::encode_rgba8(a.width, a.height, &raw, &opts).expect("encode");
                (t.elapsed().as_secs_f64(), out)
            }
            "yuv420p" if a.frames > 1 => {
                // Frame `t`: every plane panned (cyclically) by 2 luma
                // columns per frame.
                let planes = yuv420_planes(raw, w, h);
                let frames: Vec<oxideav_avif::Frame> = (0..a.frames)
                    .map(|t| {
                        let panned = planes
                            .iter()
                            .enumerate()
                            .map(|(p, (stride, data))| {
                                let shift = (if p == 0 { 2 * t } else { t }) % stride;
                                let mut out = Vec::with_capacity(data.len());
                                for row in data.chunks_exact(*stride) {
                                    out.extend_from_slice(&row[shift..]);
                                    out.extend_from_slice(&row[..shift]);
                                }
                                AvifPlane::new(*stride, out)
                            })
                            .collect();
                        let img =
                            AvifImage::new(a.width, a.height, AvifPixelFormat::Yuv420P, panned)
                                .expect("image");
                        oxideav_avif::Frame::new(img, Some(std::time::Duration::from_millis(40)))
                    })
                    .collect();
                let t = Instant::now();
                let opts = opts.with_all_intra(a.all_intra);
                let out = oxideav_avif::encode_all(&frames, &opts).expect("encode_all");
                (t.elapsed().as_secs_f64(), out)
            }
            "yuv420p" => {
                let planes = yuv420_planes(raw, w, h)
                    .into_iter()
                    .map(|(stride, data)| AvifPlane::new(stride, data))
                    .collect();
                let img = AvifImage::new(a.width, a.height, AvifPixelFormat::Yuv420P, planes)
                    .expect("image");
                let t = Instant::now();
                let out = oxideav_avif::encode(&img, &opts).expect("encode");
                (t.elapsed().as_secs_f64(), out)
            }
            other => panic!("unknown --fmt {other}"),
        }
    } else {
        let (fmt, planes) = match a.fmt.as_str() {
            "rgb24" => (
                PixelFormat::Rgb24,
                vec![VideoPlane {
                    stride: w * 3,
                    data: raw,
                }],
            ),
            "rgba" => (
                PixelFormat::Rgba,
                vec![VideoPlane {
                    stride: w * 4,
                    data: raw,
                }],
            ),
            "yuv420p" => (
                PixelFormat::Yuv420P,
                yuv420_planes(raw, w, h)
                    .into_iter()
                    .map(|(stride, data)| VideoPlane { stride, data })
                    .collect(),
            ),
            other => panic!("unknown --fmt {other}"),
        };
        let frame = Frame::Video(VideoFrame {
            pts: Some(0),
            planes,
        });
        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
        params.width = Some(a.width);
        params.height = Some(a.height);
        params.pixel_format = Some(fmt);
        let mut o = CodecOptions::new();
        if let Some(q) = a.quality {
            o = o.set("quality", q.to_string());
        }
        if a.lossless {
            o = o.set("lossless", "true");
        }
        params.options = o;
        let mut enc = oxideav_avif::make_encoder(&params).expect("make_encoder");
        enc.set_execution_context(&ExecutionContext::with_threads(a.threads));
        let t = Instant::now();
        enc.send_frame(&frame).expect("send_frame");
        let pkt = enc.receive_packet().expect("receive_packet");
        (t.elapsed().as_secs_f64(), pkt.data)
    };
    println!(
        "encode {} {}x{} {} {} threads={}: {:.3} s, {} bytes",
        a.api,
        a.width,
        a.height,
        a.fmt,
        if a.lossless { "lossless" } else { "lossy" },
        a.threads,
        secs,
        bytes.len()
    );
    if let Some(out) = &a.out {
        std::fs::write(out, &bytes).expect("write --out");
    }
}

fn decode(a: &Args) {
    let file = std::fs::read(&a.input).expect("read --in");
    let (secs, what, sum) = if a.api == "contract" {
        let opts = DecodeOptions::default().with_threads(Some(a.threads));
        let t = Instant::now();
        let img = oxideav_avif::decode_with(&file, &opts).expect("decode");
        let (what, sum) = if a.rgba {
            let rgba = img.to_rgba8_threads(a.threads);
            (
                format!("{}x{} rgba8", img.width, img.height),
                rgba.iter().map(|&b| u64::from(b)).sum::<u64>(),
            )
        } else {
            (
                format!("{}x{} {:?}", img.width, img.height, img.format),
                img.planes[0].data.iter().map(|&b| u64::from(b)).sum(),
            )
        };
        (t.elapsed().as_secs_f64(), what, sum)
    } else {
        let mut dec =
            oxideav_avif::make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR)))
                .expect("make_decoder");
        dec.set_execution_context(&ExecutionContext::with_threads(a.threads));
        let t = Instant::now();
        dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), file))
            .expect("send_packet");
        let vf = match dec.receive_frame().expect("frame") {
            Frame::Video(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        let secs = t.elapsed().as_secs_f64();
        let p = dec
            .output_params()
            .expect("output parameters after a frame");
        (
            secs,
            format!(
                "{}x{} {:?}",
                p.width.unwrap_or(0),
                p.height.unwrap_or(0),
                p.pixel_format
            ),
            vf.planes[0].data.iter().map(|&b| u64::from(b)).sum(),
        )
    };
    println!(
        "decode {} threads={}: {:.3} s, {what}, checksum {sum}",
        a.api, a.threads, secs
    );
}

fn main() {
    let a = parse_args();
    match a.op.as_str() {
        "encode" => encode(&a),
        "decode" => decode(&a),
        other => panic!("unknown operation {other} (encode | decode)"),
    }
}
