//! Encode-budget bench for the registry encoder: a raw planar
//! `yuv420p` / packed `rgb24` picture is sent through the framework
//! `avif` encoder under an `ExecutionContext` budget, and the wall
//! clock, the file size and (with `--psnr`) the luma PSNR of the
//! decoded file against the source are printed. Peak RSS comes from
//! the process (`/usr/bin/time -l` on macOS, `-v` on Linux); the bench
//! itself allocates nothing beyond the source picture before the
//! encode starts, so the process peak is the encoder's.
//!
//! ```text
//! cargo run --release --example encode_budget -- \
//!     --raw photo.yuv420p --size 4032x3024 [--fmt yuv420p|rgb24] \
//!     [--threads N] [--opt quality=60] [--opt grid=512] [--out out.avif] [--psnr]
//! ```

use std::time::Instant;

use oxideav_avif::CODEC_ID_STR;
use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, ExecutionContext, Frame, Packet, PixelFormat, TimeBase,
};

struct Args {
    raw: String,
    width: u32,
    height: u32,
    fmt: PixelFormat,
    threads: usize,
    options: Vec<(String, String)>,
    out: Option<String>,
    psnr: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        raw: String::new(),
        width: 0,
        height: 0,
        fmt: PixelFormat::Yuv420P,
        threads: 1,
        options: Vec::new(),
        out: None,
        psnr: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--raw" => a.raw = it.next().expect("--raw PATH"),
            "--size" => {
                let s = it.next().expect("--size WxH");
                let (w, h) = s.split_once('x').expect("WxH");
                a.width = w.parse().expect("width");
                a.height = h.parse().expect("height");
            }
            "--fmt" => {
                a.fmt = match it.next().expect("--fmt").as_str() {
                    "yuv420p" => PixelFormat::Yuv420P,
                    "yuvj420p" => PixelFormat::YuvJ420P,
                    "rgb24" => PixelFormat::Rgb24,
                    "rgba" => PixelFormat::Rgba,
                    "yuv420p10le" => PixelFormat::Yuv420P10Le,
                    other => panic!("unknown --fmt {other}"),
                }
            }
            "--threads" => a.threads = it.next().expect("--threads N").parse().expect("N"),
            "--opt" => {
                let s = it.next().expect("--opt key=value");
                let (k, v) = s.split_once('=').expect("key=value");
                a.options.push((k.to_string(), v.to_string()));
            }
            "--out" => a.out = it.next(),
            "--psnr" => a.psnr = true,
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(
        !a.raw.is_empty() && a.width > 0 && a.height > 0,
        "need --raw and --size"
    );
    a
}

/// Split the raw file into the frame's planes per the layout.
fn frame_of(raw: Vec<u8>, w: usize, h: usize, fmt: PixelFormat) -> VideoFrame {
    let planes = match fmt {
        PixelFormat::Yuv420P | PixelFormat::YuvJ420P => {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let y = w * h;
            let c = cw * ch;
            assert_eq!(raw.len(), y + 2 * c, "raw size is not {w}x{h} 4:2:0");
            vec![
                VideoPlane {
                    stride: w,
                    data: raw[..y].to_vec(),
                },
                VideoPlane {
                    stride: cw,
                    data: raw[y..y + c].to_vec(),
                },
                VideoPlane {
                    stride: cw,
                    data: raw[y + c..].to_vec(),
                },
            ]
        }
        PixelFormat::Yuv420P10Le => {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let y = w * h * 2;
            let c = cw * ch * 2;
            assert_eq!(raw.len(), y + 2 * c, "raw size is not {w}x{h} 4:2:0 10-bit");
            vec![
                VideoPlane {
                    stride: w * 2,
                    data: raw[..y].to_vec(),
                },
                VideoPlane {
                    stride: cw * 2,
                    data: raw[y..y + c].to_vec(),
                },
                VideoPlane {
                    stride: cw * 2,
                    data: raw[y + c..].to_vec(),
                },
            ]
        }
        PixelFormat::Rgb24 => {
            assert_eq!(raw.len(), w * h * 3, "raw size is not {w}x{h} rgb24");
            vec![VideoPlane {
                stride: w * 3,
                data: raw,
            }]
        }
        PixelFormat::Rgba => {
            assert_eq!(raw.len(), w * h * 4, "raw size is not {w}x{h} rgba");
            vec![VideoPlane {
                stride: w * 4,
                data: raw,
            }]
        }
        other => panic!("unsupported layout {other:?}"),
    };
    VideoFrame {
        pts: Some(0),
        planes,
    }
}

/// Luma PSNR (dB) between two 8-bit planes.
fn psnr8(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = f64::from(x) - f64::from(y);
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// BT.601 full-range luma of a packed RGB row set (what a lossy
/// packed-RGB encode is converted with by default).
fn luma_of_rgb(rgb: &[u8]) -> Vec<u8> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .map(|p| {
            let (r, g, b) = (f64::from(p[0]), f64::from(p[1]), f64::from(p[2]));
            (0.299 * r + 0.587 * g + 0.114 * b)
                .round()
                .clamp(0.0, 255.0) as u8
        })
        .collect()
}

fn main() {
    let a = parse_args();
    let raw = std::fs::read(&a.raw).expect("read --raw");
    let (w, h) = (a.width as usize, a.height as usize);
    let source_luma: Vec<u8> = if a.psnr {
        match a.fmt {
            PixelFormat::Rgb24 => luma_of_rgb(&raw),
            PixelFormat::Yuv420P | PixelFormat::YuvJ420P => raw[..w * h].to_vec(),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let frame = Frame::Video(frame_of(raw, w, h, a.fmt));

    let mut params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    params.width = Some(a.width);
    params.height = Some(a.height);
    params.pixel_format = Some(a.fmt);
    let mut opts = CodecOptions::new();
    for (k, v) in &a.options {
        opts = opts.set(k, v);
    }
    params.options = opts;

    let mut enc = oxideav_avif::make_encoder(&params).expect("make_encoder");
    enc.set_execution_context(&ExecutionContext::with_threads(a.threads));
    let t = Instant::now();
    enc.send_frame(&frame).expect("send_frame");
    let pkt = enc.receive_packet().expect("receive_packet");
    let secs = t.elapsed().as_secs_f64();
    let label = enc.output_params().pixel_format;
    println!(
        "encode {}x{} {:?} threads={} opts={:?}: {:.3} s, {} bytes, coded as {:?}",
        a.width,
        a.height,
        a.fmt,
        a.threads,
        a.options,
        secs,
        pkt.data.len(),
        label
    );
    if let Some(out) = &a.out {
        std::fs::write(out, &pkt.data).expect("write --out");
    }
    drop(frame);
    if a.psnr && !source_luma.is_empty() {
        let mut dec =
            oxideav_avif::make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR)))
                .expect("make_decoder");
        dec.set_execution_context(&ExecutionContext::with_threads(a.threads));
        let t = Instant::now();
        dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), pkt.data.clone()))
            .expect("decode");
        let decoded = match dec.receive_frame().expect("frame") {
            Frame::Video(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        let dsecs = t.elapsed().as_secs_f64();
        let y = &decoded.planes[0];
        let mut luma = Vec::with_capacity(w * h);
        for row in 0..h {
            luma.extend_from_slice(&y.data[row * y.stride..row * y.stride + w]);
        }
        println!(
            "decode {:.3} s as {:?}; luma PSNR {:.2} dB",
            dsecs,
            dec.output_pixel_format(),
            psnr8(&source_luma, &luma)
        );
    }
}
