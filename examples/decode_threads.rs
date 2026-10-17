//! Decode-budget bench: a synthetic 12 MP (4000 × 3000) 4:2:0 still is
//! coded as a multi-tile AV1 item (oxideav-av1's still encoder, tiles
//! derived from the thread count), wrapped in an AVIF, then decoded
//! through the registry decoder serial and under an
//! `ExecutionContext` budget. Prints the wall clock of each and checks
//! the outputs are byte-identical. A second leg codes the same picture
//! as a HEIF `grid` of single-tile items (`encode_still_grid`, 4 × 3
//! tiles) — the budget is then spent across the grid's tiles.
//!
//! ```text
//! cargo run --release --example decode_threads -- [threads] [width] [height]
//! ```

use std::time::Instant;

use oxideav_av1::encoder::still::{encode_still_yuv, StillOptions};
use oxideav_av1::encoder::yuv_frame::{ChromaFormat, YuvFrame};
use oxideav_av1::Av1CodecConfig;
use oxideav_avif::{encode_still_grid, EncodeOptions, StillChroma, StillImage};
use oxideav_core::{CodecId, CodecParameters, ExecutionContext, Frame, Packet, TimeBase};

fn plane(w: u32, h: u32, seed: u32) -> Vec<u16> {
    (0..u64::from(w) * u64::from(h))
        .map(|i| {
            let x = (i % u64::from(w)) as f64;
            let y = (i / u64::from(w)) as f64;
            let s = f64::from(seed);
            let v = 128.0
                + 60.0 * ((x * 0.013 + s).sin() * (y * 0.011 - s).cos())
                + 30.0 * ((x * 0.21 + y * 0.17).sin())
                + ((i.wrapping_mul(2654435761) >> 29) & 7) as f64;
            v.clamp(0.0, 255.0) as u16
        })
        .collect()
}

fn decode(bytes: &[u8], threads: usize) -> (Vec<u8>, f64) {
    let params = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let mut dec = oxideav_avif::make_decoder(&params).expect("decoder");
    if threads > 1 {
        dec.set_execution_context(&ExecutionContext::with_threads(threads));
    }
    let t = Instant::now();
    dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), bytes.to_vec()))
        .expect("decode");
    let frame = match dec.receive_frame().expect("frame") {
        Frame::Video(v) => v,
        other => panic!("unexpected {other:?}"),
    };
    let secs = t.elapsed().as_secs_f64();
    let mut out = Vec::new();
    for p in &frame.planes[..frame.image_plane_count()] {
        out.extend_from_slice(&p.data);
    }
    (out, secs)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let threads: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let width: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(4000);
    let height: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3000);

    let input = YuvFrame {
        width,
        height,
        bit_depth: 8,
        format: ChromaFormat::Yuv420,
        y: plane(width, height, 1),
        u: plane(width / 2, height / 2, 2),
        v: plane(width / 2, height / 2, 3),
    };
    let t = Instant::now();
    let coded = encode_still_yuv(&input, &StillOptions::new(120).threads(threads)).expect("encode");
    let av1c = Av1CodecConfig::from_sequence_header(&coded.seq).to_bytes();
    let file = oxideav_avif::encode_still_av1(
        width,
        height,
        coded.temporal_unit_bytes,
        av1c,
        vec![8, 8, 8],
    )
    .expect("mux");
    eprintln!(
        "{width}x{height} 4:2:0 still: {} bytes, encoded in {:.2} s on {threads} threads",
        file.len(),
        t.elapsed().as_secs_f64()
    );

    let (serial, t1) = decode(&file, 1);
    let (parallel, tn) = decode(&file, threads);
    assert_eq!(serial, parallel, "byte-identical at any budget");
    eprintln!(
        "decode: serial {t1:.3} s, {threads} workers {tn:.3} s ({:.2}x), identical bytes",
        t1 / tn
    );

    // The registry encoder on the same picture (quality 50): serial vs
    // the budget — identical bytes, the wall clock is the point.
    let registry_frame = Frame::Video(oxideav_core::frame::VideoFrame {
        pts: Some(0),
        planes: [
            (&input.y, width),
            (&input.u, width / 2),
            (&input.v, width / 2),
        ]
        .iter()
        .map(|(p, w)| oxideav_core::frame::VideoPlane {
            stride: *w as usize,
            data: p.iter().map(|&v| v as u8).collect(),
        })
        .collect(),
    });
    let mut p = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
    p.width = Some(width);
    p.height = Some(height);
    p.pixel_format = Some(oxideav_core::PixelFormat::Yuv420P);
    p.options = oxideav_core::CodecOptions::new().set("quality", "50");
    let encode_registry = |budget: usize| -> (Vec<u8>, f64) {
        let mut enc = oxideav_avif::make_encoder(&p).expect("encoder");
        if budget > 1 {
            enc.set_execution_context(&ExecutionContext::with_threads(budget));
        }
        let t = Instant::now();
        enc.send_frame(&registry_frame).expect("send");
        let pkt = enc.receive_packet().expect("packet");
        (pkt.data, t.elapsed().as_secs_f64())
    };
    let (e1, te1) = encode_registry(1);
    let (en, ten) = encode_registry(threads);
    assert_eq!(e1, en, "registry encoder: byte-identical at any budget");
    eprintln!(
        "registry encode (quality 50): {} bytes, serial {te1:.2} s, {threads} workers {ten:.2} s ({:.2}x), identical bytes",
        e1.len(),
        te1 / ten
    );

    // The same picture as a 4 × 3 grid of single-tile items.
    let t = Instant::now();
    let still = StillImage::yuv(
        width,
        height,
        8,
        StillChroma::Yuv420,
        input.y.clone(),
        input.u.clone(),
        input.v.clone(),
    )
    .expect("still");
    let grid = encode_still_grid(&still, &EncodeOptions::new(120, 0, false), 4, 3).expect("grid");
    eprintln!(
        "{width}x{height} as a 4x3 grid: {} bytes, encoded in {:.2} s (serial encoder)",
        grid.len(),
        t.elapsed().as_secs_f64()
    );
    let (serial, t1) = decode(&grid, 1);
    let (parallel, tn) = decode(&grid, threads);
    assert_eq!(serial, parallel, "grid: byte-identical at any budget");
    eprintln!(
        "grid decode: serial {t1:.3} s, {threads} workers {tn:.3} s ({:.2}x), identical bytes",
        t1 / tn
    );
}
