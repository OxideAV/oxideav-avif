//! The `avif` registry encoder / decoder against the image-gateway
//! contract (umbrella `IMAGE_CRATE_API.md`, round-476 rulings):
//!
//! * the advisory `quality` option (0..=100) governs the AV1 quantiser
//!   — bytes shrink and PSNR falls monotonically, 100 / absent is
//!   lossless, an explicit `q` wins;
//! * `CodecCapabilities::accepted_pixel_formats` lists every layout the
//!   encoder truly accepts, each encodes and decodes back under the
//!   coded label, anything else is a typed `Unsupported`;
//! * a packed RGB source at a lossy quality lands on a Y′CbCr coding;
//! * the decoder's worker budget (`threads` option / the execution
//!   context) is byte-identical to the serial decode.
//!
//! Everything goes through `oxideav_core` registry calls with
//! `params.options`, the way the gateway drives the codec.

#![cfg(feature = "registry")]

use oxideav_avif::{AvifDecoder, ENCODER_PIXEL_FORMATS};
use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{
    BlobKind, CodecId, CodecOptions, CodecParameters, Decoder, Encoder, Error, ExecutionContext,
    Frame, MetadataBlob, Packet, PixelFormat, RuntimeContext, TimeBase,
};

const AVIF: &str = oxideav_avif::CODEC_ID_STR;

fn registry() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_avif::register(&mut ctx);
    ctx
}

/// Textured 8-bit sample at `(x, y)` for plane `c`: a slow gradient
/// plus a sinusoidal weave, so a lossy encode has something to
/// quantise and the PSNR ladder is meaningful at 16 × 16.
fn texel(x: usize, y: usize, c: usize, w: usize, h: usize) -> u8 {
    let gx = (x * 160) / w.max(1);
    let gy = (y * 60) / h.max(1);
    let wave = ((x as f64 * 0.9 + c as f64).sin() * 22.0 + (y as f64 * 1.3).cos() * 18.0) as i64;
    (gx as i64 + gy as i64 + wave + 40 + c as i64 * 25).clamp(0, 255) as u8
}

/// Plane geometry of a layout: `(width_divisor_log2, height_divisor_log2)`
/// per plane and the bytes per sample.
fn geometry(fmt: PixelFormat) -> (Vec<(u32, u32)>, usize, usize) {
    use PixelFormat::*;
    // (plane shifts, bytes per sample, channels per sample)
    match fmt {
        Yuv420P | YuvJ420P => (vec![(0, 0), (1, 1), (1, 1)], 1, 1),
        Yuv422P | YuvJ422P => (vec![(0, 0), (1, 0), (1, 0)], 1, 1),
        Yuv444P | YuvJ444P => (vec![(0, 0), (0, 0), (0, 0)], 1, 1),
        Gray8 => (vec![(0, 0)], 1, 1),
        Yuv420P10Le | Yuv420P12Le => (vec![(0, 0), (1, 1), (1, 1)], 2, 1),
        Yuv422P10Le | Yuv422P12Le => (vec![(0, 0), (1, 0), (1, 0)], 2, 1),
        Yuv444P10Le | Yuv444P12Le => (vec![(0, 0), (0, 0), (0, 0)], 2, 1),
        Gray10Le | Gray12Le => (vec![(0, 0)], 2, 1),
        Rgb24 | Bgr24 => (vec![(0, 0)], 1, 3),
        Rgba | Bgra => (vec![(0, 0)], 1, 4),
        Gbrp8 => (vec![(0, 0); 3], 1, 1),
        Gbrap8 => (vec![(0, 0); 4], 1, 1),
        Gbrp10Le | Gbrp12Le => (vec![(0, 0); 3], 2, 1),
        Gbrap10Le | Gbrap12Le => (vec![(0, 0); 4], 2, 1),
        Yuva420P => (vec![(0, 0), (1, 1), (1, 1), (0, 0)], 1, 1),
        Ya8 => (vec![(0, 0)], 1, 2),
        other => panic!("no test geometry for {other:?}"),
    }
}

fn depth_of(fmt: PixelFormat) -> u32 {
    let name = format!("{fmt:?}");
    if name.contains("10Le") {
        10
    } else if name.contains("12Le") {
        12
    } else {
        8
    }
}

/// A textured frame in `fmt` at `w × h`.
fn frame(fmt: PixelFormat, w: u32, h: u32) -> Frame {
    let (planes, bps, channels) = geometry(fmt);
    let depth = depth_of(fmt);
    let mut out = Vec::new();
    for (pi, (sx, sy)) in planes.iter().enumerate() {
        let (pw, ph) = ((w >> sx) as usize, (h >> sy) as usize);
        let stride = pw * channels * bps;
        let mut data = Vec::with_capacity(stride * ph);
        for y in 0..ph {
            for x in 0..pw {
                for c in 0..channels {
                    let v8 = texel(x, y, pi + c, pw, ph);
                    // Alpha channels (4th packed byte / 4th plane / Ya
                    // second byte): mostly opaque with a soft edge.
                    let is_alpha = (channels == 4 && c == 3)
                        || (channels == 2 && c == 1)
                        || (channels == 1 && pi == 3);
                    let v8 = if is_alpha { 255 - v8 / 3 } else { v8 };
                    if bps == 1 {
                        data.push(v8);
                    } else {
                        let v = (u32::from(v8) << (depth - 8)) | (u32::from(v8) >> (16 - depth));
                        data.extend_from_slice(&(v as u16).to_le_bytes());
                    }
                }
            }
        }
        out.push(VideoPlane { stride, data });
    }
    Frame::Video(VideoFrame {
        pts: Some(0),
        planes: out,
    })
}

fn params(fmt: PixelFormat, w: u32, h: u32, options: CodecOptions) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(AVIF));
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = Some(fmt);
    p.options = options;
    p
}

/// Encode one frame through the registry; the file bytes plus the
/// encoder's output parameters.
fn encode(ctx: &RuntimeContext, p: &CodecParameters, f: &Frame) -> (Vec<u8>, CodecParameters) {
    let mut enc = ctx.codecs.first_encoder(p).expect("registry make_encoder");
    enc.send_frame(f).expect("send_frame");
    let pkt = enc.receive_packet().expect("receive_packet");
    (pkt.data, enc.output_params().clone())
}

/// Decode a file through the registry decoder built from `p`.
fn decode(ctx: &RuntimeContext, p: &CodecParameters, bytes: &[u8]) -> (VideoFrame, PixelFormat) {
    let mut dec = ctx.codecs.first_decoder(p).expect("registry make_decoder");
    dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), bytes.to_vec()))
        .expect("send_packet");
    let vf = match dec.receive_frame().expect("receive_frame") {
        Frame::Video(v) => v,
        other => panic!("expected a video frame, got {other:?}"),
    };
    let label = dec
        .output_pixel_format()
        .expect("the decoder labels what it emitted");
    (vf, label)
}

fn planes_of(f: &Frame) -> &[VideoPlane] {
    match f {
        Frame::Video(v) => &v.planes[..v.image_plane_count()],
        _ => unreachable!(),
    }
}

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
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// The picture as tightly packed RGB bytes, through the standalone
/// decoder's exact conversion — the common ground for comparing an
/// identity-matrix (lossless) and a Y′CbCr (lossy) coding of the same
/// RGB source.
fn rgb_of(bytes: &[u8]) -> Vec<u8> {
    oxideav_avif::decode(bytes).expect("decode").to_rgb8()
}

/// The `matrix_coefficients` of the primary's `colr` `nclx`.
fn matrix_of(bytes: &[u8]) -> Option<u16> {
    match oxideav_avif::inspect(bytes).expect("inspect").colour {
        Some(oxideav_avif::Colr::Nclx {
            matrix_coefficients,
            ..
        }) => Some(matrix_coefficients),
        _ => None,
    }
}

fn tight(vf: &VideoFrame) -> Vec<u8> {
    let mut out = Vec::new();
    for p in &vf.planes[..vf.image_plane_count()] {
        out.extend_from_slice(&p.data);
    }
    out
}

// ─────────────────────────── capabilities ───────────────────────────

#[test]
fn capabilities_declare_layouts_flags_and_quality() {
    let ctx = registry();
    let id = CodecId::new(AVIF);
    let imp = ctx
        .codecs
        .implementations(&id)
        .iter()
        .find(|i| i.make_encoder.is_some())
        .expect("avif encoder implementation")
        .clone();
    assert_eq!(
        imp.caps.accepted_pixel_formats,
        ENCODER_PIXEL_FORMATS.to_vec(),
        "declared layouts are the encoder's accepted set"
    );
    assert_eq!(ENCODER_PIXEL_FORMATS.len(), 27);
    assert!(imp.caps.lossy && imp.caps.lossless, "both flags truthful");
    let schema = ctx
        .codecs
        .encoder_options_schema(&id)
        .expect("encoder options schema");
    let names: Vec<&str> = schema.iter().map(|f| f.name).collect();
    assert_eq!(
        names,
        [
            "quality",
            "q",
            "alpha_q",
            "chroma",
            "premultiplied",
            "speed",
            "threads",
            "grid"
        ]
    );
    let schema = ctx
        .codecs
        .decoder_options_schema(&id)
        .expect("decoder options schema");
    let names: Vec<&str> = schema.iter().map(|f| f.name).collect();
    assert!(names.contains(&"threads") && names.contains(&"gain_map"));
}

// ───────────────────────────── quality ──────────────────────────────

#[test]
fn quality_ladder_shrinks_bytes_and_psnr_on_yuv_and_rgb() {
    let ctx = registry();
    for fmt in [
        PixelFormat::Yuv420P,
        PixelFormat::Yuv444P,
        PixelFormat::Rgb24,
    ] {
        let f = frame(fmt, 16, 16);
        let q = |o: CodecOptions| encode(&ctx, &params(fmt, 16, 16, o), &f).0;
        let none = q(CodecOptions::new());
        let q100 = q(CodecOptions::new().set("quality", "100"));
        let q60 = q(CodecOptions::new().set("quality", "60"));
        let q50 = q(CodecOptions::new().set("quality", "50"));
        let q10 = q(CodecOptions::new().set("quality", "10"));
        assert_eq!(
            none, q60,
            "{fmt:?}: absent quality is the production default (60)"
        );
        assert_eq!(oxideav_avif::DEFAULT_QUALITY, 60);
        assert!(
            q100.len() > q50.len() && q50.len() > q10.len(),
            "{fmt:?}: bytes must shrink with quality: 100 → {} / 50 → {} / 10 → {}",
            q100.len(),
            q50.len(),
            q10.len()
        );
        // PSNR against the source, in the source's own channels.
        let (src, p100, p50, p10): (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) =
            if fmt == PixelFormat::Rgb24 {
                (
                    planes_of(&f)[0].data.clone(),
                    rgb_of(&q100),
                    rgb_of(&q50),
                    rgb_of(&q10),
                )
            } else {
                let dp = params(fmt, 16, 16, CodecOptions::new());
                let src: Vec<u8> = planes_of(&f).iter().flat_map(|p| p.data.clone()).collect();
                (
                    src,
                    tight(&decode(&ctx, &dp, &q100).0),
                    tight(&decode(&ctx, &dp, &q50).0),
                    tight(&decode(&ctx, &dp, &q10).0),
                )
            };
        let (d100, d50, d10) = (psnr8(&src, &p100), psnr8(&src, &p50), psnr8(&src, &p10));
        assert!(d100.is_infinite(), "{fmt:?}: quality 100 is lossless");
        assert!(
            d50 > d10 && d10 > 15.0,
            "{fmt:?}: PSNR must fall with quality: 50 → {d50:.2} dB / 10 → {d10:.2} dB"
        );
        eprintln!(
            "{fmt:?}: quality 100 {} B (exact) / 50 {} B {d50:.2} dB / 10 {} B {d10:.2} dB",
            q100.len(),
            q50.len(),
            q10.len()
        );
    }
}

#[test]
fn explicit_q_wins_over_quality_and_alpha_follows() {
    let ctx = registry();
    let fmt = PixelFormat::Yuv420P;
    let f = frame(fmt, 16, 16);
    let q = |o: CodecOptions| encode(&ctx, &params(fmt, 16, 16, o), &f).0;
    let lossless = q(CodecOptions::new().set("quality", "100"));
    // quality 10 with an explicit q = 0: the lossless bytes.
    assert_eq!(
        q(CodecOptions::new().set("quality", "10").set("q", "0")),
        lossless
    );
    // quality 100 with q = 128: exactly the quality-50 coding
    // (128 is the documented mapping of 50).
    assert_eq!(oxideav_avif::quality_to_base_q_idx(50), 128);
    assert_eq!(oxideav_avif::quality_to_base_q_idx(90), 26);
    assert_eq!(oxideav_avif::quality_to_base_q_idx(10), 230);
    assert_eq!(
        q(CodecOptions::new().set("quality", "100").set("q", "128")),
        q(CodecOptions::new().set("quality", "50"))
    );
    // Alpha follows `quality` unless `alpha_q` is given: a lossy
    // quality with a lossless alpha_q is larger than quality alone.
    let fa = frame(PixelFormat::Yuva420P, 16, 16);
    let qa = |o: CodecOptions| encode(&ctx, &params(PixelFormat::Yuva420P, 16, 16, o), &fa).0;
    let both = qa(CodecOptions::new().set("quality", "30"));
    let alpha_exact = qa(CodecOptions::new().set("quality", "30").set("alpha_q", "0"));
    assert!(
        alpha_exact.len() > both.len(),
        "alpha coded losslessly ({}) must cost more than at quality 30 ({})",
        alpha_exact.len(),
        both.len()
    );
    // And the alpha plane really is exact then.
    let dp = params(PixelFormat::Yuva420P, 16, 16, CodecOptions::new());
    let (vf, label) = decode(&ctx, &dp, &alpha_exact);
    assert_eq!(label, PixelFormat::Yuva420P);
    assert_eq!(vf.planes[3].data, planes_of(&fa)[3].data, "alpha exact");
    // With nothing given: colour at the production default, alpha
    // lossless — the same bytes as saying so.
    let defaults = qa(CodecOptions::new());
    assert_eq!(
        defaults,
        qa(CodecOptions::new().set("quality", "60").set("alpha_q", "0"))
    );
    let (vf, _) = decode(&ctx, &dp, &defaults);
    assert_eq!(
        vf.planes[3].data,
        planes_of(&fa)[3].data,
        "default alpha exact"
    );
}

#[test]
fn registry_encoder_refuses_unknown_or_out_of_range_options() {
    let ctx = registry();
    let bad = |o: CodecOptions| {
        ctx.codecs
            .first_encoder(&params(PixelFormat::Yuv420P, 16, 16, o))
            .err()
    };
    assert!(matches!(
        bad(CodecOptions::new().set("qualty", "50")),
        Some(Error::InvalidData(_))
    ));
    assert!(matches!(
        bad(CodecOptions::new().set("quality", "101")),
        Some(Error::InvalidData(_))
    ));
    assert!(matches!(
        bad(CodecOptions::new().set("q", "256")),
        Some(Error::InvalidData(_))
    ));
    assert!(matches!(
        bad(CodecOptions::new().set("chroma", "411")),
        Some(Error::InvalidData(_))
    ));
    assert!(matches!(
        bad(CodecOptions::new().set("speed", "fastest")),
        Some(Error::InvalidData(_))
    ));
    // The historical lenient constructor keeps ignoring them.
    let enc = oxideav_avif::AvifEncoder::with_params(params(
        PixelFormat::Yuv420P,
        16,
        16,
        CodecOptions::new().set("qualty", "50").set("q", "999"),
    ));
    assert_eq!(
        enc.encode_options().base_q_idx,
        oxideav_avif::quality_to_base_q_idx(oxideav_avif::DEFAULT_QUALITY)
    );
}

// ───────────────────────────── layouts ──────────────────────────────

/// The label the crate's decoder reports for a lossless encode of
/// each declared layout: planar RGB for the identity coding of packed
/// RGB; the full-range `YuvJ*` label for an 8-bit `Yuv*` input (its
/// label implies no range, the item is written without `colr`, and
/// MIAF reads that as full range — the samples are stored as given);
/// the input label otherwise.
fn lossless_label(fmt: PixelFormat) -> PixelFormat {
    use PixelFormat::*;
    match fmt {
        Rgb24 | Bgr24 => Gbrp8,
        Rgba | Bgra => Gbrap8,
        Yuv420P => YuvJ420P,
        Yuv422P => YuvJ422P,
        Yuv444P => YuvJ444P,
        other => other,
    }
}

#[test]
fn every_declared_layout_encodes_and_decodes_under_its_coded_label() {
    let ctx = registry();
    for &fmt in ENCODER_PIXEL_FORMATS {
        let f = frame(fmt, 16, 16);
        let p = params(fmt, 16, 16, CodecOptions::new().set("quality", "100"));
        let (bytes, out) = encode(&ctx, &p, &f);
        let expect = lossless_label(fmt);
        assert_eq!(
            out.pixel_format,
            Some(expect),
            "{fmt:?}: output params carry the coded label"
        );
        let dp = params(fmt, 16, 16, CodecOptions::new());
        let (vf, label) = decode(&ctx, &dp, &bytes);
        assert_eq!(label, expect, "{fmt:?}: decoded label");
        let src = planes_of(&f);
        match fmt {
            PixelFormat::Rgb24 | PixelFormat::Bgr24 | PixelFormat::Rgba | PixelFormat::Bgra => {
                // Identity coding: the G / B / R (/ A) planes are the
                // packed channels, byte-exact.
                let (bpp, at) = match fmt {
                    PixelFormat::Rgb24 => (3, [1usize, 2, 0]),
                    PixelFormat::Bgr24 => (3, [1, 0, 2]),
                    PixelFormat::Rgba => (4, [1, 2, 0]),
                    _ => (4, [1, 0, 2]),
                };
                for (pi, &off) in at.iter().enumerate() {
                    let want: Vec<u8> = src[0].data.chunks_exact(bpp).map(|px| px[off]).collect();
                    assert_eq!(vf.planes[pi].data, want, "{fmt:?} plane {pi}");
                }
                if bpp == 4 {
                    let want: Vec<u8> = src[0].data.chunks_exact(4).map(|px| px[3]).collect();
                    assert_eq!(vf.planes[3].data, want, "{fmt:?} alpha");
                }
            }
            _ => {
                assert_eq!(vf.image_plane_count(), src.len(), "{fmt:?} plane count");
                for (i, (a, b)) in vf.planes.iter().zip(src).enumerate() {
                    assert_eq!(a.data, b.data, "{fmt:?} plane {i} exact");
                }
            }
        }
    }
}

#[test]
fn undeclared_layout_is_a_typed_unsupported_naming_it() {
    let ctx = registry();
    for fmt in [
        PixelFormat::Pal8,
        PixelFormat::Rgb48Le,
        PixelFormat::Yuv440P,
    ] {
        assert!(!ENCODER_PIXEL_FORMATS.contains(&fmt));
        let mut enc = ctx
            .codecs
            .first_encoder(&params(fmt, 16, 16, CodecOptions::new()))
            .expect("construction does not touch pixels");
        let f = frame(PixelFormat::Yuv444P, 16, 16);
        match enc.send_frame(&f) {
            Err(Error::Unsupported(msg)) => {
                assert!(msg.contains(&format!("{fmt:?}")), "{msg}")
            }
            other => panic!("{fmt:?}: expected Unsupported, got {other:?}"),
        }
    }
}

// ─────────────────────── lossy RGB → Y′CbCr ────────────────────────

#[test]
fn lossy_packed_rgb_lands_on_ycbcr_lossless_keeps_identity() {
    let ctx = registry();
    let f = frame(PixelFormat::Rgb24, 16, 16);
    let enc = |o: CodecOptions, w: u32, h: u32, f: &Frame| {
        encode(&ctx, &params(PixelFormat::Rgb24, w, h, o), f)
    };
    // Lossless: identity matrix, planar RGB label, exact.
    let (lossless, out) = enc(CodecOptions::new().set("quality", "100"), 16, 16, &f);
    assert_eq!(out.pixel_format, Some(PixelFormat::Gbrp8));
    assert_eq!(rgb_of(&lossless), planes_of(&f)[0].data);
    assert_eq!(matrix_of(&lossless), Some(0));

    // Lossy: full-range 4:2:0 Y′CbCr under the MIAF default matrix.
    let (lossy, out) = enc(CodecOptions::new().set("quality", "60"), 16, 16, &f);
    assert_eq!(out.pixel_format, Some(PixelFormat::YuvJ420P));
    assert_eq!(matrix_of(&lossy), Some(6));
    assert!(lossy.len() < lossless.len());
    let db = psnr8(&rgb_of(&lossy), &planes_of(&f)[0].data);
    assert!(db > 25.0, "quality 60 RGB round trip {db:.2} dB");
    let dp = params(PixelFormat::Rgb24, 16, 16, CodecOptions::new());
    assert_eq!(decode(&ctx, &dp, &lossy).1, PixelFormat::YuvJ420P);

    // `chroma=444` keeps full chroma; odd extents promote to 4:4:4.
    let (c444, out) = enc(
        CodecOptions::new()
            .set("quality", "60")
            .set("chroma", "444"),
        16,
        16,
        &f,
    );
    assert_eq!(out.pixel_format, Some(PixelFormat::YuvJ444P));
    assert_eq!(decode(&ctx, &dp, &c444).1, PixelFormat::YuvJ444P);
    let f15 = frame(PixelFormat::Rgb24, 15, 15);
    let (odd, out) = enc(CodecOptions::new().set("quality", "60"), 15, 15, &f15);
    assert_eq!(out.pixel_format, Some(PixelFormat::YuvJ444P));
    assert_eq!(
        decode(
            &ctx,
            &params(PixelFormat::Rgb24, 15, 15, CodecOptions::new()),
            &odd
        )
        .1,
        PixelFormat::YuvJ444P
    );

    // RGBA keeps its alpha as the auxiliary on the lossy path too.
    let fa = frame(PixelFormat::Rgba, 16, 16);
    let (rgba, out) = encode(
        &ctx,
        &params(
            PixelFormat::Rgba,
            16,
            16,
            CodecOptions::new().set("quality", "60"),
        ),
        &fa,
    );
    assert_eq!(out.pixel_format, Some(PixelFormat::Yuva420P));
    assert!(oxideav_avif::inspect(&rgba).expect("inspect").has_alpha);
}

// ─────────────────────────── thread budget ───────────────────────────

/// The registry encoder codes the same bytes at any worker budget
/// (the tile layout follows the picture size), serially by default,
/// on the execution context's budget or the explicit `threads`
/// option; a 1024 × 256 picture is laid out as 8 tiles.
#[test]
fn encoder_thread_budget_leaves_the_bytes_unchanged() {
    let ctx = registry();
    let (w, h) = (1024u32, 256u32);
    let f = frame(PixelFormat::Yuv420P, w, h);
    let p = params(
        PixelFormat::Yuv420P,
        w,
        h,
        CodecOptions::new().set("quality", "70"),
    );
    let (serial, _) = encode(&ctx, &p, &f);
    let mut enc = ctx.codecs.first_encoder(&p).expect("encoder");
    enc.set_execution_context(&ExecutionContext::with_threads(4));
    enc.send_frame(&f).expect("send");
    let budget = enc.receive_packet().expect("packet").data;
    assert_eq!(budget, serial, "budget 4");
    let (explicit, _) = encode(
        &ctx,
        &params(
            PixelFormat::Yuv420P,
            w,
            h,
            CodecOptions::new().set("quality", "70").set("threads", "3"),
        ),
        &f,
    );
    assert_eq!(explicit, serial, "threads=3");
    let e = oxideav_avif::AvifEncoder::try_with_params(params(
        PixelFormat::Yuv420P,
        w,
        h,
        CodecOptions::new().set("threads", "3"),
    ))
    .expect("encoder");
    assert_eq!(e.encode_options().threads, 3);
    let mut e = oxideav_avif::AvifEncoder::try_with_params(p).expect("encoder");
    assert_eq!(e.encode_options().threads, 1);
    e.set_execution_context(&ExecutionContext::with_threads(6));
    assert_eq!(e.encode_options().threads, 6);
    // The picture decodes back and is labelled as coded.
    let dp = params(PixelFormat::Yuv420P, w, h, CodecOptions::new());
    assert_eq!(decode(&ctx, &dp, &serial).1, PixelFormat::YuvJ420P);
}

const FIXTURES: &[(&str, &[u8])] = &[
    (
        "kimono_rotate90",
        include_bytes!("fixtures/kimono_rotate90.avif"),
    ),
    ("bbb_alpha", include_bytes!("fixtures/bbb_alpha.avif")),
    (
        "yuv420_10bit_full",
        include_bytes!("fixtures/yuv420_10bit_full.avif"),
    ),
    (
        "alpha_video (avis)",
        include_bytes!("fixtures/alpha_video.avif"),
    ),
    ("gainmap_tool", include_bytes!("fixtures/gainmap_tool.avif")),
];

fn decode_all_frames(dec: &mut dyn Decoder, bytes: &[u8]) -> Vec<Vec<u8>> {
    dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), bytes.to_vec()))
        .expect("send_packet");
    let mut out = Vec::new();
    while let Ok(Frame::Video(v)) = dec.receive_frame() {
        out.push(tight(&v));
    }
    assert!(!out.is_empty());
    out
}

#[test]
fn decoder_thread_budget_is_byte_identical_to_serial() {
    let ctx = registry();
    let p = params(PixelFormat::Yuv420P, 0, 0, CodecOptions::new());
    for (name, bytes) in FIXTURES {
        let mut serial = ctx.codecs.first_decoder(&p).expect("decoder");
        let want = decode_all_frames(serial.as_mut(), bytes);
        // The execution context's budget.
        let mut granted = ctx.codecs.first_decoder(&p).expect("decoder");
        granted.set_execution_context(&ExecutionContext::with_threads(4));
        assert_eq!(
            decode_all_frames(granted.as_mut(), bytes),
            want,
            "{name}: budget 4"
        );
        // The explicit `threads` codec option.
        let mut opt = ctx
            .codecs
            .first_decoder(&params(
                PixelFormat::Yuv420P,
                0,
                0,
                CodecOptions::new().set("threads", "3"),
            ))
            .expect("decoder");
        assert_eq!(
            decode_all_frames(opt.as_mut(), bytes),
            want,
            "{name}: threads=3"
        );
    }
}

/// A `grid` primary (2 × 2 tiles of 64 × 64 — the MIAF tile floor —
/// lossless with an alpha grid, lossy without) decodes identically
/// serial and on a budget — the budget is spent across the tiles.
#[test]
fn grid_tiles_decode_identically_under_a_budget() {
    use oxideav_avif::{encode_still_grid, EncodeOptions, StillChroma, StillImage};
    let (w, h) = (128u32, 128u32);
    let plane = |c: usize, pw: u32, ph: u32| -> Vec<u16> {
        (0..pw as usize * ph as usize)
            .map(|i| {
                u16::from(texel(
                    i % pw as usize,
                    i / pw as usize,
                    c,
                    pw as usize,
                    ph as usize,
                ))
            })
            .collect()
    };
    let ctx = registry();
    for (alpha, q) in [(true, 0u8), (false, 100)] {
        let mut still = StillImage::yuv(
            w,
            h,
            8,
            StillChroma::Yuv420,
            plane(0, w, h),
            plane(1, w / 2, h / 2),
            plane(2, w / 2, h / 2),
        )
        .expect("still");
        if alpha {
            still = still.with_alpha(plane(3, w, h)).expect("alpha");
        }
        let file = encode_still_grid(&still, &EncodeOptions::new(q, q, false), 2, 2).expect("grid");
        let info = oxideav_avif::inspect(&file).expect("inspect");
        assert!(info.has_grid(), "a grid primary");
        let p = params(PixelFormat::Yuv420P, 0, 0, CodecOptions::new());
        let mut serial = ctx.codecs.first_decoder(&p).expect("decoder");
        let want = decode_all_frames(serial.as_mut(), &file);
        for budget in [3usize, 8] {
            let mut d = ctx.codecs.first_decoder(&p).expect("decoder");
            d.set_execution_context(&ExecutionContext::with_threads(budget));
            assert_eq!(
                decode_all_frames(d.as_mut(), &file),
                want,
                "alpha {alpha} q {q}: budget {budget}"
            );
            assert_eq!(
                d.output_pixel_format(),
                Some(if alpha {
                    PixelFormat::Yuva420P
                } else {
                    PixelFormat::YuvJ420P
                })
            );
        }
        if q == 0 {
            // Lossless: the tiles stitch back to the source exactly.
            let got = &want[0];
            let n = (w * h) as usize;
            let y: Vec<u8> = still.y.iter().map(|&v| v as u8).collect();
            assert_eq!(
                &got[..n],
                &y[..],
                "alpha {alpha}: luma exact through the grid"
            );
        }
    }
}

#[test]
fn decoder_options_parse_strictly_and_threads_resolve() {
    let p = |o: CodecOptions| params(PixelFormat::Yuv420P, 0, 0, o);
    // Serial by default; the execution context fills the budget in.
    let mut d = AvifDecoder::with_params(&p(CodecOptions::new())).expect("defaults");
    assert_eq!(d.threads(), 1);
    d.set_execution_context(&ExecutionContext::with_threads(8));
    assert_eq!(d.threads(), 8);
    // An explicit option wins over the context.
    let mut d =
        AvifDecoder::with_params(&p(CodecOptions::new().set("threads", "2"))).expect("threads");
    d.set_execution_context(&ExecutionContext::with_threads(8));
    assert_eq!(d.threads(), 2);
    assert_eq!(d.options().threads, Some(2));
    // `threads=0` follows the context.
    let d = AvifDecoder::with_params(&p(CodecOptions::new().set("threads", "0"))).expect("zero");
    assert_eq!(d.options().threads, None);
    // The other knobs land on DecodeOptions.
    let d = AvifDecoder::with_params(&p(CodecOptions::new()
        .set("gain_map", "apply")
        .set("reference_white", "100")
        .set("max_width", "0")
        .set("max_pixels", "4096")
        .set("strict", "true")
        .set("layer", "1")))
    .expect("knobs");
    let o = d.options();
    assert!(o.tone_mapped && o.strict);
    assert_eq!(o.reference_white_nits, 100.0);
    assert_eq!(o.max_width, None);
    assert_eq!(o.max_pixels, Some(4096));
    assert_eq!(o.layer, Some(1));
    // Unknown keys and bad values are refused.
    assert!(matches!(
        AvifDecoder::with_params(&p(CodecOptions::new().set("gain_map", "maybe"))),
        Err(Error::InvalidData(_))
    ));
    assert!(matches!(
        AvifDecoder::with_params(&p(CodecOptions::new().set("thread", "2"))),
        Err(Error::InvalidData(_))
    ));
    assert!(matches!(
        AvifDecoder::with_params(&p(CodecOptions::new().set("reference_white", "-1"))),
        Err(Error::InvalidData(_))
    ));
}

/// A minimal TIFF-structured Exif block (little-endian header, one
/// empty IFD) — the `BlobKind::EXIF` payload convention.
fn exif_block() -> Vec<u8> {
    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v
}

/// Metadata through the registry (image-gateway contract, r477 blob
/// wave): stream blobs on the parameters, frame blobs on the record,
/// the frame's kind winning; ICC lands as the `prof` `colr`, Exif as
/// the `Exif` item (offset header written by the encoder, stripped by
/// the decoder), XMP as the `mime` item; kinds AVIF cannot carry are
/// dropped; the decoder puts the three back on its frame and the
/// contract image's `metadata` is the same source of truth.
#[test]
fn blobs_ride_the_registry_frame_over_stream() {
    let ctx = registry();
    let icc_stream = b"stream-side ICC profile bytes".to_vec();
    let icc_frame = b"frame-side ICC profile bytes (wins)".to_vec();
    let exif = exif_block();
    let xmp = b"<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?>\
<x:xmpmeta xmlns:x='adobe:ns:meta/'/><?xpacket end='w'?>"
        .to_vec();
    let mut p = params(
        PixelFormat::Yuv420P,
        16,
        16,
        CodecOptions::new().set("quality", "80"),
    );
    p.blobs = vec![
        MetadataBlob::new(BlobKind::ICC, icc_stream.clone()),
        MetadataBlob::new(BlobKind::EXIF, exif.clone()),
        MetadataBlob::new(BlobKind::IPTC, b"no AVIF carriage".to_vec()),
    ];
    let mut f = frame(PixelFormat::Yuv420P, 16, 16);
    if let Frame::Video(v) = &mut f {
        v.set_blobs(vec![
            MetadataBlob::new(BlobKind::ICC, icc_frame.clone()),
            MetadataBlob::new(BlobKind::XMP, xmp.clone()).with_mime("application/rdf+xml"),
        ]);
    }
    let (bytes, _) = encode(&ctx, &p, &f);

    // The contract view of the written file.
    let img = oxideav_avif::decode(&bytes).expect("decode");
    assert_eq!(
        img.metadata.icc.as_deref(),
        Some(&icc_frame[..]),
        "frame ICC over stream ICC"
    );
    assert_eq!(
        img.metadata.exif.as_deref(),
        Some(&exif[..]),
        "stream Exif stands"
    );
    assert_eq!(img.metadata.xmp.as_deref(), Some(&xmp[..]), "frame XMP");
    let info = oxideav_avif::info(&bytes).expect("info");
    assert!(info.has_exif && info.has_xmp && info.has_icc);

    // The registry decoder puts the same three on its frame.
    let (vf, _) = decode(&ctx, &CodecParameters::video(CodecId::new(AVIF)), &bytes);
    let blobs = vf.blobs();
    assert_eq!(blobs.len(), 3, "ICC / Exif / XMP, nothing else: {blobs:?}");
    assert_eq!(vf.blob(&BlobKind::ICC).expect("icc").data, icc_frame);
    assert_eq!(vf.blob(&BlobKind::EXIF).expect("exif").data, exif);
    let x = vf.blob(&BlobKind::XMP).expect("xmp");
    assert_eq!(x.data, xmp);
    assert_eq!(x.mime.as_deref(), Some("application/rdf+xml"));
    assert!(vf.blob(&BlobKind::IPTC).is_none());
    assert_eq!(img.metadata, oxideav_avif::Metadata::from_blobs(&blobs));
    assert_eq!(img.metadata.to_blobs(), blobs);

    // The frame conversion refines the same way.
    let back = oxideav_avif::AvifImage::from_video_frame(&vf, &p).expect("from_video_frame");
    assert_eq!(back.metadata, img.metadata);
    let mut bare = vf.clone();
    bare.set_blobs(Vec::new());
    let stream_only = oxideav_avif::AvifImage::from_video_frame(&bare, &p).expect("stream only");
    assert_eq!(stream_only.metadata.icc.as_deref(), Some(&icc_stream[..]));
    assert_eq!(stream_only.metadata.exif.as_deref(), Some(&exif[..]));
    assert!(stream_only.metadata.xmp.is_none());

    // Without blobs the file carries no metadata items at all.
    let (plain, _) = encode(
        &ctx,
        &params(PixelFormat::Yuv420P, 16, 16, CodecOptions::new()),
        &frame(PixelFormat::Yuv420P, 16, 16),
    );
    let info = oxideav_avif::info(&plain).expect("info");
    assert!(!info.has_exif && !info.has_xmp && !info.has_icc);
    let (vf, _) = decode(&ctx, &CodecParameters::video(CodecId::new(AVIF)), &plain);
    assert!(vf.blobs().is_empty());
    assert!(plain.len() < bytes.len());
}

// ───────────────────────────── grid policy ──────────────────────────

/// The `grid` policy: `auto` codes a picture above 1 MP as a grid of
/// 512-pixel tiles and a smaller one as a single item, `none` keeps
/// one item up to the coded bound, a tile edge forces that tiling;
/// the pixels decode the same on every shape, the bytes never depend
/// on the worker budget, and the option parses strictly.
#[test]
fn grid_policy_elects_shapes_and_tiles_code_on_the_budget() {
    use oxideav_avif::{
        elect_grid_for, GridTiling, AUTO_GRID_MAX_SINGLE_PIXELS, AUTO_GRID_MAX_TILES,
        AUTO_GRID_TILE, STILL_MAX_CODED_DIM,
    };
    // The election table.
    let elect = |w, h, a, g| elect_grid_for(w, h, a, g).unwrap();
    assert_eq!(elect(1024, 1024, false, GridTiling::Auto), None);
    assert_eq!(elect(1100, 900, false, GridTiling::Auto), None);
    assert_eq!(elect(4032, 3024, false, GridTiling::Auto), Some((8, 6)));
    assert_eq!(elect(1200, 900, false, GridTiling::Auto), Some((3, 2)));
    assert_eq!(elect(4032, 3024, false, GridTiling::Single), None);
    assert_eq!(elect(8000, 100, false, GridTiling::Single), Some((2, 1)));
    assert_eq!(
        elect(4032, 3024, false, GridTiling::MaxTile(1024)),
        Some((4, 3))
    );
    assert_eq!(elect(500, 500, false, GridTiling::MaxTile(512)), None);
    assert_eq!(elect(500, 500, false, GridTiling::MaxTile(8)), Some((8, 8)));
    assert_eq!(elect(0, 10, false, GridTiling::Auto), None);
    assert!(elect_grid_for(5000, 0, false, GridTiling::Single).is_err());
    // The reader-side ipco cap: colour + alpha tiles stay under
    // AUTO_GRID_MAX_TILES by doubling the tile edge (12 MP with alpha
    // = 2 × 12 tiles of 1024; 48 MP = 48 tiles of 1024, 96 with alpha
    // would exceed it so 2048: 4 × 3); an explicit edge is honoured.
    assert_eq!(elect(4032, 3024, true, GridTiling::Auto), Some((4, 3)));
    assert_eq!(elect(8000, 6000, false, GridTiling::Auto), Some((8, 6)));
    assert_eq!(elect(8000, 6000, true, GridTiling::Auto), Some((4, 3)));
    assert_eq!(
        elect(4032, 3024, true, GridTiling::MaxTile(256)),
        Some((16, 12))
    );
    assert_eq!(AUTO_GRID_TILE, 512);
    assert_eq!(AUTO_GRID_MAX_SINGLE_PIXELS, 1 << 20);
    assert_eq!(AUTO_GRID_MAX_TILES, 90);
    assert_eq!(STILL_MAX_CODED_DIM, 4096);

    let ctx = registry();
    let (w, h) = (1200u32, 900u32);
    let f = frame(PixelFormat::Yuv420P, w, h);
    let shape = |opts: CodecOptions, threads: usize| -> (Vec<u8>, Option<(u16, u16)>) {
        let p = params(PixelFormat::Yuv420P, w, h, opts.set("quality", "70"));
        let mut enc = ctx.codecs.first_encoder(&p).expect("encoder");
        enc.set_execution_context(&ExecutionContext::with_threads(threads));
        enc.send_frame(&f).expect("send_frame");
        let bytes = enc.receive_packet().expect("packet").data;
        let tiles = grid_tiles(&bytes);
        (bytes, tiles)
    };
    let (auto1, tiles) = shape(CodecOptions::new(), 1);
    assert_eq!(tiles, Some((3, 2)), "1 MP+ codes as a grid by default");
    let (auto8, _) = shape(CodecOptions::new(), 8);
    assert_eq!(auto1, auto8, "the bytes never depend on the worker budget");
    let (none, tiles) = shape(CodecOptions::new().set("grid", "none"), 8);
    assert_eq!(tiles, None, "grid=none is one item");
    let (edge, tiles) = shape(CodecOptions::new().set("grid", "256"), 4);
    assert_eq!(tiles, Some((5, 4)));
    let (small_single, tiles) = shape(CodecOptions::new().set("grid", "2048"), 2);
    assert_eq!(tiles, None, "a tile edge the picture fits is one item");
    assert_eq!(small_single, none);

    // Every shape decodes to the same picture (within the quantiser)
    // and the same label.
    let dp = CodecParameters::video(CodecId::new(AVIF));
    let (vf_auto, label) = decode(&ctx, &dp, &auto1);
    let (vf_none, label2) = decode(&ctx, &dp, &none);
    let (vf_edge, _) = decode(&ctx, &dp, &edge);
    assert_eq!(label, PixelFormat::YuvJ420P);
    assert_eq!(label, label2);
    let src = planes_of(&f)[0].data.clone();
    for (name, vf) in [("auto", &vf_auto), ("none", &vf_none), ("256", &vf_edge)] {
        let y = tight_plane(vf, 0, w as usize, h as usize);
        let psnr = psnr8(&src, &y);
        assert!(psnr > 30.0, "{name}: luma PSNR {psnr:.2} dB");
    }
    let y_auto = tight_plane(&vf_auto, 0, w as usize, h as usize);
    let y_none = tight_plane(&vf_none, 0, w as usize, h as usize);
    let agree = psnr8(&y_auto, &y_none);
    assert!(
        agree > 30.0,
        "shapes agree within the quantiser: {agree:.2} dB"
    );

    // Strict parse.
    for bad in ["0", "32", "5000", "sometimes", "-1"] {
        let p = params(
            PixelFormat::Yuv420P,
            16,
            16,
            CodecOptions::new().set("grid", bad),
        );
        assert!(
            matches!(oxideav_avif::make_encoder(&p), Err(Error::InvalidData(_))),
            "grid={bad} must be refused"
        );
    }
    let opts = oxideav_avif::AvifEncoderOptions::from_params(&params(
        PixelFormat::Yuv420P,
        16,
        16,
        CodecOptions::new().set("grid", "1024"),
    ))
    .expect("parse");
    assert_eq!(opts.grid, GridTiling::MaxTile(1024));
    assert_eq!(opts.resolve().grid, GridTiling::MaxTile(1024));
    assert_eq!(
        oxideav_avif::AvifEncoderOptions::default().grid,
        GridTiling::Auto
    );
}

/// `(columns, rows)` of a file whose primary is a `grid`, `None` for a
/// coded primary.
fn grid_tiles(bytes: &[u8]) -> Option<(u16, u16)> {
    let info = oxideav_avif::inspect(bytes).expect("inspect");
    if !info.is_grid {
        return None;
    }
    let hdr = oxideav_avif::parse_header(bytes).expect("header");
    let body = hdr
        .item_data(hdr.meta.primary_item_id.expect("primary"))
        .expect("grid body");
    let grid = oxideav_avif::ImageGrid::parse(&body).expect("grid descriptor");
    Some((grid.columns, grid.rows))
}

/// One plane of a frame, tightly packed.
fn tight_plane(vf: &VideoFrame, i: usize, w: usize, h: usize) -> Vec<u8> {
    let p = &vf.planes[i];
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        out.extend_from_slice(&p.data[row * p.stride..row * p.stride + w]);
    }
    out
}
