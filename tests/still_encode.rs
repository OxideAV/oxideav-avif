//! End-to-end tests for the pixel → AVIF still encoder
//! ([`oxideav_avif::still`]): encode through `oxideav_av1`'s KEY-frame
//! encoder + this crate's muxer, then decode back through this crate's
//! own decoder (pixel-exact where lossless, PSNR-gated where lossy),
//! plus container-level audits (mif1 / profile brands / av1C fields)
//! and a black-box acceptance leg against an external AVIF decoder
//! binary when one is installed (skipped silently otherwise).

#![cfg(feature = "registry")]

use oxideav_avif::{
    audit_avif_profile_compliance, audit_mif1, audit_sequence_header_obu, classify_brands,
    encode_still, encode_still_grid, inspect, parse, parse_header, AvifDecoder, Colr, StillChroma,
    StillEncodeOptions, StillImage, StillProperties,
};
use oxideav_core::{CodecId, CodecParameters, Decoder, Frame, Packet, TimeBase};

// ───────────────────────── helpers ─────────────────────────

/// Deterministic non-uniform plane: position-hashed samples bounded by
/// the bit depth.
fn plane(w: u32, h: u32, bit_depth: u8, seed: u32) -> Vec<u16> {
    let mask = (1u32 << bit_depth) - 1;
    (0..w as u64 * h as u64)
        .map(|i| {
            let x = (i as u32 % w).wrapping_mul(2654435761);
            let y = (i as u32 / w).wrapping_mul(40503);
            (((x ^ y ^ seed.wrapping_mul(97)) >> 3) & mask) as u16
        })
        .collect()
}

/// Smooth gradient plane (for the lossy PSNR leg — natural-ish
/// content the lossy path compresses meaningfully).
fn gradient(w: u32, h: u32, bit_depth: u8) -> Vec<u16> {
    let ceil = (1u32 << bit_depth) - 1;
    (0..w as u64 * h as u64)
        .map(|i| {
            let x = i as u32 % w;
            let y = i as u32 / w;
            ((x + y) * ceil / (w + h - 2).max(1)) as u16
        })
        .collect()
}

fn build_image(w: u32, h: u32, bit_depth: u8, chroma: StillChroma) -> StillImage {
    let (sx, sy) = match chroma {
        StillChroma::Yuv420 => (1, 1),
        StillChroma::Yuv422 => (1, 0),
        StillChroma::Yuv444 => (0, 0),
        StillChroma::Monochrome => (0, 0),
    };
    let (y, u, v) = if chroma == StillChroma::Monochrome {
        (plane(w, h, bit_depth, 1), Vec::new(), Vec::new())
    } else {
        (
            plane(w, h, bit_depth, 1),
            plane(w >> sx, h >> sy, bit_depth, 2),
            plane(w >> sx, h >> sy, bit_depth, 3),
        )
    };
    StillImage::yuv(w, h, bit_depth, chroma, y, u, v).expect("build image")
}

/// Decode an AVIF through this crate's own registry decoder; returns
/// the composited `VideoFrame`.
fn decode_own(label: &str, avif: &[u8]) -> oxideav_core::frame::VideoFrame {
    let mut d = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), avif.to_vec());
    d.send_packet(&pkt)
        .unwrap_or_else(|e| panic!("{label}: send_packet failed: {e}"));
    match d.receive_frame() {
        Ok(Frame::Video(v)) => v,
        Ok(other) => panic!("{label}: expected VideoFrame, got {other:?}"),
        Err(e) => panic!("{label}: receive_frame failed: {e}"),
    }
}

/// Decode the primary item's raw AV1 payload through the `oxideav_av1`
/// registry decoder — the raw-payload cross-check leg that validates
/// the composed output against an independent decode of the same
/// bitstream.
fn decode_payload_av1(label: &str, avif: &[u8]) -> oxideav_core::frame::VideoFrame {
    let img = parse(avif).unwrap_or_else(|e| panic!("{label}: parse failed: {e}"));
    let params = CodecParameters::video(CodecId::new("av1"));
    let mut d = oxideav_av1::registry::make_decoder(&params).expect("av1 decoder");
    let pkt = Packet::new(0, TimeBase::new(1, 90_000), img.primary_item_data.to_vec());
    d.send_packet(&pkt)
        .unwrap_or_else(|e| panic!("{label}: av1 send_packet failed: {e}"));
    match d.receive_frame() {
        Ok(Frame::Video(v)) => v,
        Ok(other) => panic!("{label}: expected VideoFrame, got {other:?}"),
        Err(e) => panic!("{label}: av1 receive_frame failed: {e}"),
    }
}

/// Split a little-endian 2-byte-per-sample plane into `u16`s.
fn le_u16(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn narrow(p: &[u16]) -> Vec<u8> {
    p.iter().map(|&s| s as u8).collect()
}

/// PSNR (dB) between two same-length sample slices at `bit_depth`.
fn psnr(a: &[u16], b: &[u16], bit_depth: u8) -> f64 {
    assert_eq!(a.len(), b.len());
    let peak = ((1u32 << bit_depth) - 1) as f64;
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (peak * peak / mse).log10()
    }
}

// ───────────────────── lossless round-trips ─────────────────────

/// Arc 1: 8-bit 4:2:0 lossless — encode, decode back through this
/// crate's own decoder, byte-exact planes; container audits pass.
#[test]
fn yuv420_8bit_lossless_round_trips_exact() {
    let img = build_image(32, 32, 8, StillChroma::Yuv420);
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");

    // Container-level: mif1-compliant, Baseline brand, exactly one
    // Sequence Header OBU in the item payload (av1-avif §2.1).
    assert!(audit_mif1(&avif).expect("audit").is_compliant());
    let info = inspect(&avif).expect("inspect");
    assert_eq!((info.width, info.height), (32, 32));
    assert_eq!(info.max_bit_depth(), 8);
    let parsed = parse(&avif).expect("parse");
    assert!(parsed.compatible_brands.iter().any(|b| b == b"MA1B"));
    let hdr = parse_header(&avif).expect("parse_header");
    let sh_audit = audit_sequence_header_obu(&hdr.meta, &avif);
    assert!(sh_audit.iter().all(|a| a.is_compliant()), "{sh_audit:?}");
    let brands = classify_brands(&hdr.major_brand, &hdr.compatible_brands).expect("brands");
    for rec in audit_avif_profile_compliance(&hdr.meta, &brands) {
        assert!(rec.is_compliant(), "{rec:?}");
    }

    // Pixel-exact round-trip.
    let vf = decode_own("yuv420 lossless", &avif);
    assert_eq!(vf.planes.len(), 3);
    assert_eq!(vf.planes[0].data, narrow(&img.y), "Y");
    assert_eq!(vf.planes[1].data, narrow(&img.u), "U");
    assert_eq!(vf.planes[2].data, narrow(&img.v), "V");
}

/// Arc 2: the full (bit depth × chroma format) matrix — 8/10/12-bit ×
/// 4:2:0 / 4:2:2 / 4:4:4 / monochrome, all lossless, all validated
/// sample-exact. Every leg decodes through this crate's own decoder
/// (the HBD composition layer emits little-endian 2-byte planes); the
/// 10/12-bit legs additionally cross-check the extracted payload
/// through the AV1 registry decoder. Also pins the av1C field mapping
/// and the §8 profile-brand election per pairing.
#[test]
fn depth_format_matrix_round_trips_exact() {
    let chromas = [
        (StillChroma::Yuv420, 3usize),
        (StillChroma::Yuv422, 3),
        (StillChroma::Yuv444, 3),
        (StillChroma::Monochrome, 1),
    ];
    for bit_depth in [8u8, 10, 12] {
        for (chroma, nplanes) in chromas {
            let label = format!("{bit_depth}-bit {chroma:?}");
            let img = build_image(16, 16, bit_depth, chroma);
            let avif = encode_still(&img, &StillEncodeOptions::default())
                .unwrap_or_else(|e| panic!("{label}: encode failed: {e}"));

            // av1C fields mirror the pairing (av1-avif §2.2.1).
            let info = inspect(&avif).expect("inspect");
            assert_eq!(info.max_bit_depth(), bit_depth, "{label}: depth");
            assert_eq!(
                info.is_monochrome(),
                chroma == StillChroma::Monochrome,
                "{label}: mono flag"
            );
            // pixi channel count matches the layout.
            assert_eq!(info.num_channels(), nplanes, "{label}: pixi");

            // Profile brand follows the elected seq_profile (§8):
            // Main → MA1B, High → MA1A, Professional → general only.
            let expect_profile = match (bit_depth, chroma) {
                (12, _) | (_, StillChroma::Yuv422) => 2u8,
                (_, StillChroma::Yuv444) => 1,
                _ => 0,
            };
            let parsed = parse(&avif).expect("parse");
            let has = |b: &[u8; 4]| parsed.compatible_brands.iter().any(|x| x == b);
            match expect_profile {
                0 => assert!(has(b"MA1B") && !has(b"MA1A"), "{label}: brand"),
                1 => assert!(has(b"MA1A") && !has(b"MA1B"), "{label}: brand"),
                _ => assert!(!has(b"MA1A") && !has(b"MA1B"), "{label}: brand"),
            }
            let hdr = parse_header(&avif).expect("parse_header");
            let brands = classify_brands(&hdr.major_brand, &hdr.compatible_brands).expect("brands");
            for rec in audit_avif_profile_compliance(&hdr.meta, &brands) {
                assert!(rec.is_compliant(), "{label}: {rec:?}");
            }

            // Sample-exact decode-back through this crate's own
            // decoder + composition layer, at every depth.
            let vf = decode_own(&label, &avif);
            assert_eq!(vf.planes.len(), nplanes, "{label}: planes");
            if bit_depth == 8 {
                assert_eq!(vf.planes[0].data, narrow(&img.y), "{label}: Y");
                if nplanes == 3 {
                    assert_eq!(vf.planes[1].data, narrow(&img.u), "{label}: U");
                    assert_eq!(vf.planes[2].data, narrow(&img.v), "{label}: V");
                }
            } else {
                assert_eq!(le_u16(&vf.planes[0].data), img.y, "{label}: Y");
                if nplanes == 3 {
                    assert_eq!(le_u16(&vf.planes[1].data), img.u, "{label}: U");
                    assert_eq!(le_u16(&vf.planes[2].data), img.v, "{label}: V");
                }
                // Cross-check: the raw payload through the AV1
                // registry decoder agrees with the composed output.
                let raw = decode_payload_av1(&label, &avif);
                assert_eq!(le_u16(&raw.planes[0].data), img.y, "{label}: raw Y");
            }
        }
    }
}

/// Arbitrary (non-multiple-of-8) extents: the coded frame pads with
/// edge replication, `ispe` documents the coded extents (av1-avif
/// §2.2.2 `shall`), and the emitted top-left-anchored `clap`
/// (av1-avif §2.2.3) crops the decode back to the requested pixels
/// exactly.
#[test]
fn odd_dimensions_pad_and_clap_back_exact() {
    // 4:4:4 (odd × odd), monochrome (odd), 4:2:0 (even but not
    // multiple of 8).
    let cases = [
        (17u32, 11u32, StillChroma::Yuv444),
        (9, 9, StillChroma::Monochrome),
        (18, 10, StillChroma::Yuv420),
    ];
    for (w, h, chroma) in cases {
        let label = format!("{w}x{h} {chroma:?}");
        let img = build_image(w, h, 8, chroma);
        let avif = encode_still(&img, &StillEncodeOptions::default())
            .unwrap_or_else(|e| panic!("{label}: encode failed: {e}"));
        // ispe documents the padded coded extents; clap carries the
        // display crop.
        let info = inspect(&avif).expect("inspect");
        assert_eq!(info.width % 8, 0, "{label}: coded width padded");
        assert_eq!(info.height % 8, 0, "{label}: coded height padded");
        assert!(audit_mif1(&avif).expect("audit").is_compliant());

        let vf = decode_own(&label, &avif);
        // The decoder applies clap — the output is the requested rect.
        assert_eq!(vf.planes[0].stride as u32, w, "{label}: cropped width");
        assert_eq!(
            vf.planes[0].data.len(),
            (w * h) as usize,
            "{label}: cropped size"
        );
        assert_eq!(vf.planes[0].data, narrow(&img.y), "{label}: Y exact");
    }
}

// ───────────────────── RGB(A) identity path ─────────────────────

/// RGB via the H.273 identity matrix in 4:4:4: byte-exact round-trip
/// (Y = G, Cb = B, Cr = R) with the `colr` `nclx` identity triple on
/// the wire.
#[test]
fn rgb8_identity_round_trips_exact() {
    let (w, h) = (23u32, 15u32);
    let rgb: Vec<u8> = (0..w * h * 3).map(|i| ((i * 31) & 0xff) as u8).collect();
    let img = StillImage::rgb8(w, h, &rgb).expect("rgb8");
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");

    // colr signals the identity triple, full range.
    let parsed = parse(&avif).expect("parse");
    match parsed.colr.expect("colr present") {
        Colr::Nclx {
            matrix_coefficients,
            full_range,
            ..
        } => {
            assert_eq!(matrix_coefficients, 0, "identity matrix");
            assert!(full_range, "full range");
        }
        other => panic!("expected nclx, got {other:?}"),
    }
    // 4:4:4 → AV1 High profile → MA1A.
    assert!(parsed.compatible_brands.iter().any(|b| b == b"MA1A"));

    let vf = decode_own("rgb8", &avif);
    assert_eq!(vf.planes.len(), 3);
    let n = (w * h) as usize;
    let mut out = Vec::with_capacity(n * 3);
    for i in 0..n {
        out.push(vf.planes[2].data[i]); // R = Cr
        out.push(vf.planes[0].data[i]); // G = Y
        out.push(vf.planes[1].data[i]); // B = Cb
    }
    assert_eq!(out, rgb, "RGB byte-exact");
}

/// RGBA: the A channel rides as a hidden monochrome auxiliary item
/// (av1-avif §4.1) and composites back into a 4-plane frame,
/// byte-exact end to end.
#[test]
fn rgba8_alpha_round_trips_exact() {
    let (w, h) = (16u32, 16u32);
    let rgba: Vec<u8> = (0..w * h * 4).map(|i| ((i * 17) & 0xff) as u8).collect();
    let img = StillImage::rgba8(w, h, &rgba).expect("rgba8");
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");

    let info = inspect(&avif).expect("inspect");
    assert!(info.has_alpha, "alpha auxiliary present");
    // §4.1: alpha bit depth matches the master.
    for rec in &info.alpha_bit_depth_compliance {
        assert!(rec.is_compliant(), "{rec:?}");
    }

    let vf = decode_own("rgba8", &avif);
    assert_eq!(vf.planes.len(), 4, "YUV + A");
    let n = (w * h) as usize;
    let mut out = Vec::with_capacity(n * 4);
    for i in 0..n {
        out.push(vf.planes[2].data[i]);
        out.push(vf.planes[0].data[i]);
        out.push(vf.planes[1].data[i]);
        out.push(vf.planes[3].data[i]);
    }
    assert_eq!(out, rgba, "RGBA byte-exact");
}

/// Alpha on a 4:2:0 master + the premultiplied signal: the `prem`
/// iref lands on the wire and the composite still round-trips.
#[test]
fn yuv420_alpha_premultiplied_round_trips() {
    let (w, h) = (24u32, 16u32);
    let img = build_image(w, h, 8, StillChroma::Yuv420);
    let alpha = plane(w, h, 8, 9);
    let img = img.with_alpha(alpha.clone()).expect("alpha");
    let opts = StillEncodeOptions {
        premultiplied_alpha: true,
        ..Default::default()
    };
    let avif = encode_still(&img, &opts).expect("encode");

    let hdr = oxideav_avif::parse_header(&avif).expect("parse_header");
    let primary = hdr.meta.primary_item_id.expect("pitm");
    assert!(
        hdr.meta.is_alpha_premultiplied_for(primary),
        "prem iref present"
    );
    let vf = decode_own("420+alpha", &avif);
    assert_eq!(vf.planes.len(), 4);
    assert_eq!(vf.planes[3].data, narrow(&alpha), "alpha exact");
    assert_eq!(vf.planes[0].data, narrow(&img.y), "Y exact");
}

/// The alpha stream must signal §5.5.2 `color_range = 1`: av1-avif
/// §4.1 is explicit — "The color_range field in the Sequence Header
/// OBU shall be set to 1" (and `mono_chrome` shall be 1) for every
/// AV1 Auxiliary Image Item; readers also ignore any `colr` on the
/// alpha item, so the bitstream flag is the only range signal. Walk
/// the alpha item's payload to its Sequence Header OBU and check both
/// parsed fields; the identity-RGB colour item (paired with a
/// full-range `colr`) checks `color_range` too.
#[test]
fn full_range_flag_signalled_where_it_matters() {
    let img = StillImage::rgba8(
        16,
        16,
        &(0..16u32 * 16 * 4)
            .map(|i| ((i * 13) & 0xff) as u8)
            .collect::<Vec<_>>(),
    )
    .expect("rgba8");
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");
    let hdr = oxideav_avif::parse_header(&avif).expect("parse_header");
    let primary = hdr.meta.primary_item_id.expect("pitm");
    let alpha_id = oxideav_avif::find_alpha_item_id(&hdr.meta, primary).expect("alpha id");

    for (label, id) in [("primary", primary), ("alpha", alpha_id)] {
        let loc = hdr.meta.location_by_id(id).expect("iloc");
        let payload = oxideav_avif::item_bytes(&avif, loc).expect("item bytes");
        let mut off = 0usize;
        let mut found = false;
        while off < payload.len() {
            let (desc, consumed) = oxideav_av1::parse_obu(&payload[off..]).expect("obu");
            if desc.obu_type == oxideav_av1::ObuType::SequenceHeader {
                let seq = oxideav_av1::parse_sequence_header(desc.payload).expect("sh");
                assert!(
                    seq.color_config.color_range,
                    "{label}: sequence header must signal full range (av1-avif §4.1 shall)"
                );
                if label == "alpha" {
                    assert!(
                        seq.color_config.mono_chrome,
                        "alpha: mono_chrome shall be 1 (av1-avif §4.1)"
                    );
                }
                found = true;
            }
            off += consumed;
        }
        assert!(found, "{label}: no sequence header OBU");
    }
}

/// 10-bit master + 10-bit alpha: the av1-avif §4.1 same-bit-depth
/// `shall` holds at high bit depth too. The composition layer now
/// covers HBD end to end, so the composited 4-plane frame validates
/// sample-exact through this crate's own decoder; the raw alpha
/// payload additionally cross-checks through the AV1 registry decoder.
#[test]
fn ten_bit_alpha_matches_master_depth_and_round_trips() {
    let (w, h) = (16u32, 16u32);
    let img = build_image(w, h, 10, StillChroma::Yuv420);
    let alpha = plane(w, h, 10, 11);
    let img = img.with_alpha(alpha.clone()).expect("alpha");
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");

    let info = inspect(&avif).expect("inspect");
    assert!(info.has_alpha);
    for rec in &info.alpha_bit_depth_compliance {
        assert!(rec.is_compliant(), "§4.1 same-depth: {rec:?}");
    }

    // Composited decode: Y U V A planes, all sample-exact at 10 bits.
    let vf = decode_own("10-bit 420 + alpha", &avif);
    assert_eq!(vf.planes.len(), 4, "YUV + A");
    assert_eq!(le_u16(&vf.planes[0].data), img.y, "Y");
    assert_eq!(le_u16(&vf.planes[1].data), img.u, "U");
    assert_eq!(le_u16(&vf.planes[2].data), img.v, "V");
    assert_eq!(le_u16(&vf.planes[3].data), alpha, "alpha");

    // Alpha payload sample-exact at 10 bits (resolved via auxl iref)
    // through the raw AV1 registry decoder too.
    let hdr = parse_header(&avif).expect("parse_header");
    let primary = hdr.meta.primary_item_id.expect("pitm");
    let alpha_id = oxideav_avif::find_alpha_item_id(&hdr.meta, primary).expect("alpha id");
    let loc = hdr.meta.location_by_id(alpha_id).expect("iloc");
    let payload = oxideav_avif::item_bytes(&avif, loc).expect("alpha payload");
    let params = CodecParameters::video(CodecId::new("av1"));
    let mut d = oxideav_av1::registry::make_decoder(&params).expect("av1 decoder");
    d.send_packet(&Packet::new(0, TimeBase::new(1, 90_000), payload.to_vec()))
        .expect("alpha decode");
    let af = match d.receive_frame().expect("alpha frame") {
        Frame::Video(v) => v,
        other => panic!("expected VideoFrame, got {other:?}"),
    };
    assert_eq!(af.planes.len(), 1, "monochrome alpha");
    assert_eq!(le_u16(&af.planes[0].data), alpha, "alpha samples exact");
}

/// 12-bit 4:4:4 master + 12-bit alpha, with the premultiplied signal:
/// the `prem` iref lands on the wire and the HBD composite still
/// round-trips sample-exact.
#[test]
fn twelve_bit_alpha_premultiplied_round_trips() {
    let (w, h) = (16u32, 16u32);
    let img = build_image(w, h, 12, StillChroma::Yuv444);
    let alpha = plane(w, h, 12, 21);
    let img = img.with_alpha(alpha.clone()).expect("alpha");
    let opts = StillEncodeOptions {
        premultiplied_alpha: true,
        ..Default::default()
    };
    let avif = encode_still(&img, &opts).expect("encode");

    let hdr = parse_header(&avif).expect("parse_header");
    let primary = hdr.meta.primary_item_id.expect("pitm");
    assert!(
        hdr.meta.is_alpha_premultiplied_for(primary),
        "prem iref present"
    );
    let vf = decode_own("12-bit 444 + prem alpha", &avif);
    assert_eq!(vf.planes.len(), 4);
    assert_eq!(le_u16(&vf.planes[0].data), img.y, "Y exact");
    assert_eq!(le_u16(&vf.planes[3].data), alpha, "alpha exact");
}

/// 10/12-bit monochrome master + same-depth alpha → packed `Ya16Le`
/// output: interleaved 16-bit LE Y A words with the raw coded values,
/// plus the core significant-bits side channel reporting the
/// effective depth on the single packed image plane.
#[test]
fn hbd_monochrome_alpha_composites_packed_ya16le() {
    for depth in [10u8, 12] {
        let label = format!("{depth}-bit mono + alpha");
        let (w, h) = (16u32, 8u32);
        let img = build_image(w, h, depth, StillChroma::Monochrome);
        let alpha = plane(w, h, depth, 33);
        let img = img.with_alpha(alpha.clone()).expect("alpha");
        let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");

        let vf = decode_own(&label, &avif);
        assert_eq!(vf.image_plane_count(), 1, "{label}: one packed plane");
        assert_eq!(
            vf.plane_significant_bits(0),
            Some(depth),
            "{label}: significant-bits side channel"
        );
        let plane0 = &vf.planes[0];
        assert_eq!(plane0.stride, (w as usize) * 4, "{label}: 4 bytes/px");
        let words = le_u16(&plane0.data);
        let n = (w * h) as usize;
        assert_eq!(words.len(), n * 2, "{label}: interleaved words");
        for i in 0..n {
            assert_eq!(words[i * 2], img.y[i], "{label}: Y[{i}]");
            assert_eq!(words[i * 2 + 1], alpha[i], "{label}: A[{i}]");
        }
    }
}

/// HBD grid encode: a 10-bit 4:2:0 canvas split into 2×2 tiles with
/// right/bottom trim reassembles sample-exact through the crate's own
/// HBD grid stitch.
#[test]
fn hbd_grid_encode_round_trips_exact_with_trim() {
    let (w, h) = (150u32, 130u32);
    let img = build_image(w, h, 10, StillChroma::Yuv420);
    let avif = encode_still_grid(&img, &StillEncodeOptions::default(), 2, 2).expect("grid encode");

    let info = inspect(&avif).expect("inspect");
    assert!(info.is_grid, "grid primary");
    assert_eq!(info.max_bit_depth(), 10, "10-bit grid");
    assert_eq!((info.width, info.height), (w, h), "canvas extents");
    assert!(audit_mif1(&avif).expect("audit").is_compliant());

    let vf = decode_own("10-bit grid 2x2", &avif);
    assert_eq!(vf.planes[0].stride as u32, w * 2, "byte stride");
    assert_eq!(le_u16(&vf.planes[0].data), img.y, "Y exact across seams");
    assert_eq!(le_u16(&vf.planes[1].data), img.u, "U exact across seams");
    assert_eq!(le_u16(&vf.planes[2].data), img.v, "V exact across seams");
}

/// HBD arbitrary extents: the coded frame pads, `clap` crops the
/// decode back to the requested pixels — sample-exact at 10 and 12
/// bits (the HBD crop path).
#[test]
fn hbd_odd_dimensions_pad_and_clap_back_exact() {
    let cases = [
        (17u32, 11u32, 10u8, StillChroma::Yuv444),
        (9, 9, 12, StillChroma::Monochrome),
        (18, 10, 12, StillChroma::Yuv420),
    ];
    for (w, h, depth, chroma) in cases {
        let label = format!("{w}x{h} {depth}-bit {chroma:?}");
        let img = build_image(w, h, depth, chroma);
        let avif = encode_still(&img, &StillEncodeOptions::default())
            .unwrap_or_else(|e| panic!("{label}: encode failed: {e}"));
        let vf = decode_own(&label, &avif);
        assert_eq!(vf.planes[0].stride as u32, w * 2, "{label}: byte stride");
        assert_eq!(le_u16(&vf.planes[0].data), img.y, "{label}: Y exact");
    }
}

/// HBD orientation: a 10-bit encode carrying `irot` = 1 decodes with
/// swapped extents and exactly the 90°-CCW-rotated samples (the HBD
/// rotation path moves 2-byte words).
#[test]
fn hbd_orientation_irot_rotates_samples_exact() {
    let (w, h) = (24u32, 16u32);
    let img = build_image(w, h, 10, StillChroma::Yuv444).with_props(StillProperties {
        irot: Some(1),
        ..Default::default()
    });
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");

    let vf = decode_own("10-bit irot", &avif);
    assert_eq!(vf.planes[0].stride as u32, h * 2, "rotated byte stride");
    let got = le_u16(&vf.planes[0].data);
    // Expected: 90° CCW — output pixel (ox, oy) on the rotated h×w
    // canvas sources input (w-1-oy, ox).
    let mut expect = Vec::with_capacity((w * h) as usize);
    for oy in 0..w as usize {
        for ox in 0..h as usize {
            expect.push(img.y[ox * w as usize + (w as usize - 1 - oy)]);
        }
    }
    assert_eq!(got, expect, "rotated samples exact");
}

/// Pass-through container properties: Exif + XMP metadata items land
/// `cdsc`-linked and byte-exact, HDR properties and orientation
/// round-trip through the parser, and the orientation composes with
/// the decode path (irot=1 swaps the displayed extents).
#[test]
fn pass_through_properties_round_trip() {
    let (w, h) = (24u32, 16u32);
    let exif = b"\x00\x00\x00\x00II*\x00still-exif".to_vec();
    let xmp = br#"<?xpacket?><x:xmpmeta/>"#.to_vec();
    let img = build_image(w, h, 8, StillChroma::Yuv444).with_props(StillProperties {
        exif: Some(exif.clone()),
        xmp: Some(xmp.clone()),
        clli: Some(oxideav_avif::Clli {
            max_content_light_level: 1000,
            max_pic_average_light_level: 400,
        }),
        irot: Some(1),
        ..Default::default()
    });
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");

    let info = inspect(&avif).expect("inspect");
    assert!(info.has_descriptive_metadata(), "Exif/XMP present");
    let exif_id = info.exif_item_id.expect("exif item");
    let xmp_id = info.xmp_item_id.expect("xmp item");
    assert_eq!(
        oxideav_avif::item_payload_bytes(&avif, exif_id).expect("exif bytes"),
        exif
    );
    assert_eq!(
        oxideav_avif::item_payload_bytes(&avif, xmp_id).expect("xmp bytes"),
        xmp
    );
    let parsed = parse(&avif).expect("parse");
    let clli = parsed.clli.expect("clli");
    assert_eq!(clli.max_content_light_level, 1000);

    // irot=1 rotates on decode: displayed extents swap.
    let vf = decode_own("props irot", &avif);
    assert_eq!(vf.planes[0].stride as u32, h, "rotated width = h");
    assert_eq!(
        vf.planes[0].data.len() as u32,
        w * h,
        "rotated plane extent"
    );
}

/// Tile election: the fewest tiles per axis that fit the bound, never
/// below the 64-pixel floor, never leaving a fully-trimmed tile; and
/// `encode_still_auto` routes oversize canvases through it. The
/// external AVIF decoder binary accepts the elected grid and recovers
/// the exact planes.
#[test]
fn grid_tiling_election_and_auto_encode() {
    use oxideav_avif::{elect_grid_tiling, encode_still_auto, GRID_MIN_TILE_DIM};
    assert_eq!(elect_grid_tiling(4096, 4096, 4096), Some((1, 1)));
    assert_eq!(elect_grid_tiling(4097, 100, 4096), Some((2, 1)));
    assert_eq!(elect_grid_tiling(9000, 5000, 4096), Some((3, 2)));
    assert_eq!(elect_grid_tiling(150, 130, 80), Some((2, 2)));
    assert_eq!(elect_grid_tiling(0, 10, 4096), None);
    assert_eq!(elect_grid_tiling(100, 100, GRID_MIN_TILE_DIM - 1), None);
    // A bound just above the floor: 100 wide needs 2 columns of 56 →
    // below the floor → no tiling.
    assert_eq!(elect_grid_tiling(100, 64, 64), None);

    // Auto: a 300x70 canvas with a 128 bound would grid; with the real
    // bound it is a single item. Exercise the grid arm directly.
    let (w, h) = (300u32, 70u32);
    let img = build_image(w, h, 8, StillChroma::Yuv420);
    let single = encode_still_auto(&img, &StillEncodeOptions::default()).expect("auto");
    assert!(!inspect(&single).expect("inspect").is_grid);
    let (cols, rows) = elect_grid_tiling(w, h, 128).expect("tiling");
    assert_eq!((cols, rows), (3, 1));
    let grid = encode_still_grid(&img, &StillEncodeOptions::default(), cols, rows).expect("grid");
    let info = inspect(&grid).expect("inspect");
    assert!(info.is_grid);
    let vf = decode_own("elected grid", &grid);
    assert_eq!(vf.planes[0].data, narrow(&img.y));

    // Black box: external AVIF decoder accepts the elected grid.
    let tmp = std::env::temp_dir().join(format!("oxideav-avif-grid-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let in_path = tmp.join("grid.avif");
    let out_path = tmp.join("grid.y4m");
    std::fs::write(&in_path, &grid).expect("write");
    match std::process::Command::new("avifdec")
        .arg(&in_path)
        .arg(&out_path)
        .output()
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("elected grid: external decoder not installed — leg skipped");
        }
        Err(e) => panic!("spawn failed: {e}"),
        Ok(out) => {
            assert!(
                out.status.success(),
                "external decoder rejected the elected grid: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let y4m = std::fs::read(&out_path).expect("read y4m");
            let (gw, gh, _, planes) = parse_y4m(&y4m).expect("parse y4m");
            assert_eq!((gw, gh), (w, h));
            assert_eq!(planes[0], narrow(&img.y), "external Y exact");
            assert_eq!(planes[1], narrow(&img.u), "external U exact");
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

// ───────────────────────── layered items ─────────────────────────

/// Layered (progressive) AVIF: 2 and 3 independently coded spatial
/// layers in one temporal unit. `a1lx` documents the per-layer byte
/// sizes (summing to the item size), `lsel = 0xFFFF` lets the reader
/// finish on the top layer (sample-exact), a pinned `lsel` renders
/// that layer, an `a1op` operating point drops the upper layers, and
/// the external AVIF decoder binary decodes the top layer exactly.
#[test]
fn layered_item_round_trips_and_selects_layers() {
    use oxideav_avif::encode_still_layered;
    let top = build_image(64, 48, 8, StillChroma::Yuv420);
    let mid = build_image(32, 24, 8, StillChroma::Yuv420);
    let base = build_image(16, 16, 8, StillChroma::Yuv420);

    for layers in [vec![&mid, &top], vec![&base, &mid, &top]] {
        let n = layers.len();
        let label = format!("{n}-layer");
        let avif = encode_still_layered(&layers, None, &StillEncodeOptions::default())
            .expect("layered encode");
        let info = inspect(&avif).expect("inspect");
        assert_eq!(
            (info.width, info.height),
            (64, 48),
            "{label}: ispe = top layer"
        );
        let a1lx = info.layered_index.expect("a1lx present");
        assert_eq!(
            a1lx.documented_layers(),
            n - 1,
            "{label}: n-1 documented sizes"
        );
        let parsed = parse(&avif).expect("parse");
        let item_len = parsed.primary_item_data.len() as u32;
        let documented: u32 = a1lx.layer_size.iter().sum();
        assert!(documented < item_len, "{label}: last layer implicit");
        assert!(audit_mif1(&avif).expect("audit").is_compliant());
        assert!(
            info.sequence_header_obu_compliance
                .iter()
                .all(|a| a.is_compliant()),
            "{label}: exactly one sequence header"
        );

        let vf = decode_own(&label, &avif);
        assert_eq!(vf.planes[0].data, narrow(&top.y), "{label}: top Y exact");
        assert_eq!(vf.planes[1].data, narrow(&top.u), "{label}: top U exact");

        // Pin the base layer.
        let pinned = encode_still_layered(&layers, Some(0), &StillEncodeOptions::default())
            .expect("pinned encode");
        let info = inspect(&pinned).expect("inspect");
        let first = layers[0];
        assert_eq!((info.width, info.height), (first.width, first.height));
        let vf = decode_own(&format!("{label} lsel=0"), &pinned);
        assert_eq!(
            vf.planes[0].data,
            narrow(&first.y),
            "{label}: layer 0 Y exact"
        );
        assert_eq!(vf.planes[0].stride as u32, first.width);

        // Operating point 1 = drop the top layer (nested prefix
        // points): the last decoded frame is layer n-2.
        let a1op = oxideav_avif::AvifMuxer::new(
            layers[n - 2].width,
            layers[n - 2].height,
            parsed.primary_item_data.to_vec(),
            parsed.av1c.clone().expect("av1c"),
        )
        .with_operating_point(1)
        .with_layer_selector(0xFFFF)
        .build()
        .expect("a1op mux");
        let info = inspect(&a1op).expect("inspect");
        assert_eq!(info.operating_point.map(|o| o.op_index), Some(1));
        let vf = decode_own(&format!("{label} a1op=1"), &a1op);
        let want = layers[n - 2];
        assert_eq!(
            vf.planes[0].stride as u32, want.width,
            "{label}: op 1 width"
        );
        assert_eq!(vf.planes[0].data, narrow(&want.y), "{label}: op 1 Y exact");

        // Black box: the external AVIF decoder renders the top layer.
        let tmp = std::env::temp_dir().join(format!("oxideav-avif-layered-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("tmp dir");
        let in_path = tmp.join("layered.avif");
        let out_path = tmp.join("layered.y4m");
        std::fs::write(&in_path, &avif).expect("write");
        match std::process::Command::new("avifdec")
            .arg(&in_path)
            .arg(&out_path)
            .output()
        {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("{label}: external decoder not installed — leg skipped");
            }
            Err(e) => panic!("spawn failed: {e}"),
            Ok(out) => {
                assert!(
                    out.status.success(),
                    "{label}: external decoder rejected the layered file: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let y4m = std::fs::read(&out_path).expect("read y4m");
                let (gw, gh, _, planes) = parse_y4m(&y4m).expect("parse y4m");
                assert_eq!((gw, gh), (64, 48), "{label}: external dims");
                assert_eq!(planes[0], narrow(&top.y), "{label}: external Y exact");
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Guards: layer count, mixed layouts, oversize lower layer.
    assert!(encode_still_layered(&[&top], None, &StillEncodeOptions::default()).is_err());
    let hbd = build_image(32, 24, 10, StillChroma::Yuv420);
    assert!(encode_still_layered(&[&hbd, &top], None, &StillEncodeOptions::default()).is_err());
    let big = build_image(80, 48, 8, StillChroma::Yuv420);
    assert!(encode_still_layered(&[&big, &top], None, &StillEncodeOptions::default()).is_err());
    assert!(encode_still_layered(&[&mid, &top], Some(2), &StillEncodeOptions::default()).is_err());
}

// ───────────────────────── lossy leg ─────────────────────────

/// Lossy encode: strictly smaller than the lossless sibling on smooth
/// content and PSNR-gated against the source.
#[test]
fn lossy_encode_is_smaller_and_psnr_gated() {
    let (w, h) = (64u32, 64u32);
    let y = gradient(w, h, 8);
    let u = gradient(w / 2, h / 2, 8);
    let v = plane(w / 2, h / 2, 8, 4);
    let img = StillImage::yuv(w, h, 8, StillChroma::Yuv420, y.clone(), u, v).expect("image");

    let lossless = encode_still(&img, &StillEncodeOptions::default()).expect("lossless");
    let lossy = encode_still(
        &img,
        &StillEncodeOptions {
            base_q_idx: 100,
            ..Default::default()
        },
    )
    .expect("lossy");
    assert!(
        lossy.len() < lossless.len(),
        "lossy {} must be smaller than lossless {}",
        lossy.len(),
        lossless.len()
    );

    let vf = decode_own("lossy q100", &lossy);
    let got: Vec<u16> = vf.planes[0].data.iter().map(|&s| s as u16).collect();
    let db = psnr(&y, &got, 8);
    assert!(db >= 38.0, "luma PSNR {db:.2} dB below the 38 dB gate");
}

// ───────────────────────── grid encode ─────────────────────────

/// Grid encode: a canvas split into 2×2 independently coded tiles
/// reassembles pixel-exact through this crate's own grid decode path,
/// with right/bottom trim exercised (canvas extents not multiples of
/// the tile extents).
#[test]
fn grid_encode_round_trips_exact_with_trim() {
    let (w, h) = (150u32, 130u32);
    let img = build_image(w, h, 8, StillChroma::Yuv420);
    let avif = encode_still_grid(&img, &StillEncodeOptions::default(), 2, 2).expect("grid encode");

    let info = inspect(&avif).expect("inspect");
    assert!(info.is_grid, "grid primary");
    assert_eq!((info.width, info.height), (w, h), "canvas extents");
    assert!(audit_mif1(&avif).expect("audit").is_compliant());
    let grid = info
        .grid_resolutions
        .first()
        .expect("grid resolution resolved");
    assert!(grid.covers_canvas(), "tiles cover the canvas");
    assert!(grid.trimmed_tile_count() > 0, "trim exercised");

    let vf = decode_own("grid 2x2", &avif);
    assert_eq!(vf.planes[0].stride as u32, w);
    assert_eq!(vf.planes[0].data, narrow(&img.y), "Y exact across seams");
    assert_eq!(vf.planes[1].data, narrow(&img.u), "U exact across seams");
    assert_eq!(vf.planes[2].data, narrow(&img.v), "V exact across seams");
}

/// Grid guards: a tiling that leaves fully-trimmed tiles is rejected
/// up front, as are a depth map and an identity derivation over the
/// grid.
#[test]
fn grid_encode_guards() {
    // 506 wide split into 9 columns → ceil(56.2) = 57 → 64-wide coded
    // tiles → 8 × 64 = 512 ≥ 506: column 8 starts past the canvas.
    let wide = build_image(506, 64, 8, StillChroma::Yuv420);
    let err = encode_still_grid(&wide, &StillEncodeOptions::default(), 9, 1).unwrap_err();
    assert!(err.to_string().contains("fully-trimmed"), "{err}");
    // Tiles below the 64-pixel floor are rejected.
    let img = build_image(16, 16, 8, StillChroma::Yuv420);
    let err = encode_still_grid(&img, &StillEncodeOptions::default(), 2, 1).unwrap_err();
    assert!(err.to_string().contains("floor"), "{err}");
    let with_depth = build_image(128, 128, 8, StillChroma::Yuv420)
        .with_depth_map(plane(128, 128, 8, 5))
        .unwrap();
    assert!(encode_still_grid(&with_depth, &StillEncodeOptions::default(), 2, 1).is_err());
    let with_iden = img.with_props(StillProperties {
        identity_derivation: Some(oxideav_avif::IdentityDerivation::default()),
        ..Default::default()
    });
    assert!(encode_still_grid(&with_iden, &StillEncodeOptions::default(), 2, 1).is_err());
}

/// Grid + alpha: the alpha auxiliary of a grid primary is a hidden
/// alpha `grid` of monochrome tiles (`auxl` → the colour grid). Both
/// grids stitch sample-exact through the decoder with trim, at 8 and
/// 10 bits; the container audits (mif1, alpha bit depth, grid
/// derivation) stay compliant; and the external AVIF decoder binary
/// recovers the exact alpha plane.
#[test]
fn grid_encode_with_alpha_round_trips_exact() {
    for (depth, premultiplied) in [(8u8, false), (10, true)] {
        let (w, h) = (150u32, 130u32);
        let img = build_image(w, h, depth, StillChroma::Yuv420)
            .with_alpha(plane(w, h, depth, 9))
            .unwrap();
        let opts = StillEncodeOptions {
            premultiplied_alpha: premultiplied,
            ..Default::default()
        };
        let avif = encode_still_grid(&img, &opts, 2, 2).expect("grid+alpha encode");
        let label = format!("grid alpha {depth}-bit");

        let info = inspect(&avif).expect("inspect");
        assert!(info.is_grid && info.has_alpha, "{label}: grid + alpha");
        assert_eq!(info.premultiplied_alpha, premultiplied, "{label}: prem");
        assert!(audit_mif1(&avif).expect("audit").is_compliant());
        assert_eq!(
            info.grid_resolutions.len(),
            2,
            "{label}: colour + alpha grids"
        );
        assert!(info
            .grid_derivation_compliance
            .iter()
            .all(|a| a.is_compliant()));
        assert!(info
            .alpha_bit_depth_compliance
            .iter()
            .all(|a| a.is_compliant()));
        let hdr = parse_header(&avif).expect("parse");
        let alpha_id =
            oxideav_avif::find_alpha_item_id(&hdr.meta, hdr.meta.primary_item_id.unwrap())
                .expect("alpha grid attached to the colour grid");
        assert_eq!(
            hdr.meta.item_by_id(alpha_id).unwrap().item_type,
            oxideav_avif::ITEM_TYPE_GRID
        );

        let vf = decode_own(&label, &avif);
        assert_eq!(vf.planes.len(), 4, "{label}: Yuva");
        let alpha = img.alpha.as_ref().unwrap();
        if depth == 8 {
            assert_eq!(vf.planes[0].data, narrow(&img.y), "{label}: Y");
            assert_eq!(vf.planes[1].data, narrow(&img.u), "{label}: U");
            assert_eq!(vf.planes[3].data, narrow(alpha), "{label}: A");
        } else {
            assert_eq!(le_u16(&vf.planes[0].data), img.y, "{label}: Y");
            assert_eq!(le_u16(&vf.planes[2].data), img.v, "{label}: V");
            assert_eq!(le_u16(&vf.planes[3].data), *alpha, "{label}: A");
        }

        // Black box: alpha plane through the external AVIF decoder
        // (PNG output, alpha channel extracted by ImageMagick).
        if depth == 8 {
            let tmp = std::env::temp_dir().join(format!("oxideav-avif-ga-{}", std::process::id()));
            std::fs::create_dir_all(&tmp).expect("tmp dir");
            let in_path = tmp.join("ga.avif");
            let png = tmp.join("ga.png");
            let raw = tmp.join("ga_alpha.raw");
            std::fs::write(&in_path, &avif).expect("write");
            let run = std::process::Command::new("avifdec")
                .arg(&in_path)
                .arg(&png)
                .output();
            match run {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("{label}: external decoder not installed — leg skipped");
                }
                Err(e) => panic!("{label}: spawn failed: {e}"),
                Ok(out) => {
                    assert!(
                        out.status.success(),
                        "{label}: external decoder rejected the grid+alpha file: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    let conv = std::process::Command::new("magick")
                        .arg(&png)
                        .arg("-alpha")
                        .arg("extract")
                        .arg("-depth")
                        .arg("8")
                        .arg(format!("gray:{}", raw.display()))
                        .output();
                    match conv {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            eprintln!("{label}: ImageMagick not installed — alpha leg skipped");
                        }
                        Err(e) => panic!("{label}: spawn failed: {e}"),
                        Ok(c) => {
                            assert!(c.status.success(), "{label}: magick failed");
                            let ext = std::fs::read(&raw).expect("read alpha raw");
                            assert_eq!(ext, narrow(alpha), "{label}: external alpha plane exact");
                        }
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }
}

/// Depth map auxiliary: coded as a hidden monochrome item at the
/// master's depth (`auxC` depth URN + `auxl`), surfaced by `inspect`,
/// and its payload decodes back sample-exact.
#[test]
fn depth_map_auxiliary_round_trips_exact() {
    let (w, h) = (24u32, 16u32);
    let depth_map = plane(w, h, 10, 77);
    let img = build_image(w, h, 10, StillChroma::Yuv420)
        .with_depth_map(depth_map.clone())
        .unwrap();
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");
    let info = inspect(&avif).expect("inspect");
    let depth_id = info.depth_map_item_id.expect("depth aux surfaced");
    assert!(!info.has_alpha);
    let hdr = parse_header(&avif).expect("parse");
    let item = hdr.meta.item_by_id(depth_id).expect("depth item");
    assert!(item.is_hidden());
    let bytes = oxideav_avif::item_payload_bytes(&avif, depth_id).expect("depth payload");
    let params = CodecParameters::video(CodecId::new("av1"));
    let mut d = oxideav_av1::registry::make_decoder(&params).expect("av1 decoder");
    d.send_packet(&Packet::new(0, TimeBase::new(1, 90_000), bytes))
        .expect("depth send");
    let Frame::Video(vf) = d.receive_frame().expect("depth frame") else {
        panic!("non-video");
    };
    assert_eq!(vf.planes.len(), 1, "monochrome depth");
    assert_eq!(le_u16(&vf.planes[0].data), depth_map, "depth samples exact");
    // The primary decode is unaffected (depth is not composited).
    let vf = decode_own("depth primary", &avif);
    assert_eq!(vf.planes.len(), 3);
    assert_eq!(le_u16(&vf.planes[0].data), img.y);
}

/// Grid path pass-through: HDR (`mdcv` / `clli` / `amve`), Exif / XMP
/// and `irot` land on the grid item and round-trip; the rotation is
/// applied to the stitched canvas.
#[test]
fn grid_pass_through_properties_round_trip() {
    let (w, h) = (200u32, 80u32);
    let exif = b"\x00\x00\x00\x00II*\x00grid-exif".to_vec();
    let mdcv = oxideav_avif::Mdcv {
        display_primaries_xy: [(34000, 16000), (13250, 34500), (7500, 3000)],
        white_point_xy: (15635, 16450),
        max_display_mastering_luminance: 10_000_000,
        min_display_mastering_luminance: 50,
    };
    let amve = oxideav_avif::meta::Amve {
        ambient_illuminance: 314_000,
        ambient_light_x: 15635,
        ambient_light_y: 16450,
    };
    let img = build_image(w, h, 8, StillChroma::Yuv420).with_props(StillProperties {
        exif: Some(exif.clone()),
        mdcv: Some(mdcv),
        amve: Some(amve),
        clli: Some(oxideav_avif::Clli {
            max_content_light_level: 4000,
            max_pic_average_light_level: 400,
        }),
        irot: Some(1),
        pasp: Some(oxideav_avif::Pasp {
            h_spacing: 4,
            v_spacing: 3,
        }),
        ..Default::default()
    });
    let avif = encode_still_grid(&img, &StillEncodeOptions::default(), 2, 1).expect("grid encode");
    let info = inspect(&avif).expect("inspect");
    assert!(info.is_grid);
    assert_eq!(info.mdcv, Some(mdcv), "mdcv on the grid item");
    assert_eq!(info.amve, Some(amve), "amve on the grid item");
    assert_eq!(info.clli.map(|c| c.max_content_light_level), Some(4000));
    assert_eq!(info.pasp.map(|p| (p.h_spacing, p.v_spacing)), Some((4, 3)));
    let exif_id = info.exif_item_id.expect("exif item");
    assert_eq!(
        oxideav_avif::item_payload_bytes(&avif, exif_id).expect("exif bytes"),
        exif
    );
    assert!(audit_mif1(&avif).expect("audit").is_compliant());
    let vf = decode_own("grid irot", &avif);
    assert_eq!(vf.planes[0].stride as u32, h, "rotated width = h");
    // irot 1: out(x, y) = in(W-1-y, x), out extents (H, W).
    for y in 0..w {
        for x in 0..h {
            let src = img.y[(x * w + (w - 1 - y)) as usize] as u8;
            assert_eq!(vf.planes[0].data[(y * h + x) as usize], src, "Y at {x},{y}");
        }
    }
}

// ───────────────── HBD AVIS sequence decode ─────────────────

/// 10-bit AVIS image sequence decodes end to end: two 10-bit KEY
/// frames coded by the still encoder are re-wrapped into a synthetic
/// `avis` file (ftyp + moov/trak/mdia/minf/stbl + mdat), and the
/// registry decoder's sequence path hands both frames out as
/// little-endian 16-bit word planes, sample-exact. Pins the removal
/// of the sequence-track `high_bitdepth` rejection.
#[test]
fn hbd_avis_sequence_decodes_sample_exact() {
    fn bx(t: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(t);
        out.extend_from_slice(payload);
        out
    }
    fn full(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8, 0, 0, 0]; // version 0, flags 0
        out.extend_from_slice(body);
        out
    }

    // Two distinct 10-bit 4:2:0 frames at coded-grid extents (16×16 —
    // no padding, no clap) coded losslessly; lift each coded payload +
    // av1C from the still-encoder output black-box.
    let (w, h) = (16u32, 16u32);
    let img0 = build_image(w, h, 10, StillChroma::Yuv420);
    let img1 = {
        let y = plane(w, h, 10, 51);
        let u = plane(w / 2, h / 2, 10, 52);
        let v = plane(w / 2, h / 2, 10, 53);
        StillImage::yuv(w, h, 10, StillChroma::Yuv420, y, u, v).expect("img1")
    };
    let lift = |img: &StillImage| -> (Vec<u8>, Vec<u8>) {
        let avif = encode_still(img, &StillEncodeOptions::default()).expect("encode");
        let parsed = parse(&avif).expect("parse");
        (
            parsed.primary_item_data.to_vec(),
            parsed.av1c.expect("av1C").to_vec(),
        )
    };
    let (pay0, av1c) = lift(&img0);
    let (pay1, _) = lift(&img1);

    // stsd → av01 sample entry (78-byte VisualSampleEntry header, then
    // the av1C child box).
    let av01_entry = {
        let mut p = vec![0u8; 78];
        p[7] = 1; // data_reference_index = 1
        p.extend_from_slice(&bx(b"av1C", &av1c));
        bx(b"av01", &p)
    };
    let stsd = bx(b"stsd", &{
        let mut b = full(&1u32.to_be_bytes());
        b.extend_from_slice(&av01_entry);
        b
    });
    let stts = bx(b"stts", &{
        let mut b = full(&1u32.to_be_bytes());
        b.extend_from_slice(&2u32.to_be_bytes()); // sample_count = 2
        b.extend_from_slice(&1u32.to_be_bytes()); // sample_delta = 1
        b
    });
    let stsc = bx(b"stsc", &{
        let mut b = full(&1u32.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
        b.extend_from_slice(&2u32.to_be_bytes()); // samples_per_chunk
        b.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
        b
    });
    let stsz = bx(b"stsz", &{
        let mut b = full(&0u32.to_be_bytes()); // sample_size = 0 (per-sample)
        b.extend_from_slice(&2u32.to_be_bytes());
        b.extend_from_slice(&(pay0.len() as u32).to_be_bytes());
        b.extend_from_slice(&(pay1.len() as u32).to_be_bytes());
        b
    });
    let stss = bx(b"stss", &{
        let mut b = full(&2u32.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&2u32.to_be_bytes());
        b
    });
    let ftyp = bx(b"ftyp", &{
        let mut b = b"avis".to_vec();
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"avismsf1miafmif1");
        b
    });

    // The chunk offset is absolute in the file; the stco payload is
    // fixed-size, so build the file once with a placeholder to measure
    // it, then again with the real offset.
    let build = |chunk_offset: u32| -> (Vec<u8>, usize) {
        let stco = bx(b"stco", &{
            let mut b = full(&1u32.to_be_bytes());
            b.extend_from_slice(&chunk_offset.to_be_bytes());
            b
        });
        let mut stbl_children = Vec::new();
        stbl_children.extend_from_slice(&stsd);
        stbl_children.extend_from_slice(&stts);
        stbl_children.extend_from_slice(&stsc);
        stbl_children.extend_from_slice(&stsz);
        stbl_children.extend_from_slice(&stco);
        stbl_children.extend_from_slice(&stss);
        let stbl = bx(b"stbl", &stbl_children);
        let minf = bx(b"minf", &stbl);
        let mdia = bx(b"mdia", &minf);
        let trak = bx(b"trak", &mdia);
        let moov = bx(b"moov", &trak);
        let mut file = ftyp.clone();
        file.extend_from_slice(&moov);
        let mdat_payload_at = file.len() + 8;
        let mut mdat_payload = pay0.clone();
        mdat_payload.extend_from_slice(&pay1);
        file.extend_from_slice(&bx(b"mdat", &mdat_payload));
        (file, mdat_payload_at)
    };
    let (_, offset) = build(0);
    let (file, offset2) = build(offset as u32);
    assert_eq!(offset, offset2, "fixed-size stco keeps the layout stable");

    // Decode through the registry decoder — the `avis` brand + moov
    // routes to the sequence path.
    let mut d = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    d.send_packet(&Packet::new(0, TimeBase::new(1, 1), file))
        .expect("avis send_packet");
    for (i, img) in [&img0, &img1].into_iter().enumerate() {
        let vf = match d
            .receive_frame()
            .unwrap_or_else(|e| panic!("frame {i}: {e}"))
        {
            Frame::Video(v) => v,
            other => panic!("frame {i}: expected VideoFrame, got {other:?}"),
        };
        assert_eq!(vf.planes.len(), 3, "frame {i}: planes");
        assert_eq!(le_u16(&vf.planes[0].data), img.y, "frame {i}: Y");
        assert_eq!(le_u16(&vf.planes[1].data), img.u, "frame {i}: U");
        assert_eq!(le_u16(&vf.planes[2].data), img.v, "frame {i}: V");
    }
}

// ─────────────────── black-box acceptance ───────────────────

/// Minimal Y4M reader for the black-box leg: returns
/// `(width, height, chroma_tag, planes)` of the first frame. A
/// `p10` / `p12` suffix on the chroma tag selects 2-byte-LE samples
/// (depth is retained in the Y4M output); plane byte lengths scale
/// accordingly.
fn parse_y4m(bytes: &[u8]) -> Option<(u32, u32, String, Vec<Vec<u8>>)> {
    let nl = bytes.iter().position(|&b| b == b'\n')?;
    let header = std::str::from_utf8(&bytes[..nl]).ok()?;
    if !header.starts_with("YUV4MPEG2") {
        return None;
    }
    let mut w = 0u32;
    let mut h = 0u32;
    let mut chroma = "420".to_string();
    for tok in header.split(' ').skip(1) {
        if let Some(v) = tok.strip_prefix('W') {
            w = v.parse().ok()?;
        } else if let Some(v) = tok.strip_prefix('H') {
            h = v.parse().ok()?;
        } else if let Some(v) = tok.strip_prefix('C') {
            chroma = v.to_string();
        }
    }
    let rest = &bytes[nl + 1..];
    let fnl = rest.iter().position(|&b| b == b'\n')?;
    if !rest.starts_with(b"FRAME") {
        return None;
    }
    let data = &rest[fnl + 1..];
    let (cw, ch) = if chroma.starts_with("420") {
        (w.div_ceil(2), h.div_ceil(2))
    } else if chroma.starts_with("422") {
        (w.div_ceil(2), h)
    } else if chroma.starts_with("444") {
        (w, h)
    } else if chroma.starts_with("mono") {
        (0, 0)
    } else {
        return None;
    };
    let bps: usize = if chroma.contains("p10") || chroma.contains("p12") || chroma.contains("p16") {
        2
    } else {
        1
    };
    let ylen = (w * h) as usize * bps;
    let clen = (cw * ch) as usize * bps;
    if data.len() < ylen + 2 * clen {
        return None;
    }
    let mut planes = vec![data[..ylen].to_vec()];
    if clen > 0 {
        planes.push(data[ylen..ylen + clen].to_vec());
        planes.push(data[ylen + clen..ylen + 2 * clen].to_vec());
    }
    Some((w, h, chroma, planes))
}

/// Black-box acceptance: an independent AVIF decoder binary
/// (`avifdec`), when installed, must decode this crate's encodes to
/// the exact source planes (Y4M leg, coded extents = requested
/// extents so no crop is involved). Skips silently when the binary is
/// not on PATH — the in-tree round-trip tests above are the always-on
/// gate; this leg adds cross-implementation acceptance where the
/// environment provides a validator.
#[test]
fn black_box_external_decoder_accepts_our_encodes() {
    let tmp = std::env::temp_dir().join(format!("oxideav-avif-bb-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("tmp dir");

    let cases = [
        (StillChroma::Yuv420, 8u8, "420"),
        (StillChroma::Yuv444, 8, "444"),
        (StillChroma::Monochrome, 8, "mono"),
        (StillChroma::Yuv420, 10, "420p10"),
        (StillChroma::Yuv422, 10, "422p10"),
        (StillChroma::Yuv444, 12, "444p12"),
    ];
    for (chroma, depth, tag) in cases {
        let label = format!("black-box {depth}-bit {chroma:?}");
        let img = build_image(32, 32, depth, chroma);
        let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");
        let in_path = tmp.join(format!("bb_{tag}.avif"));
        let out_path = tmp.join(format!("bb_{tag}.y4m"));
        std::fs::write(&in_path, &avif).expect("write avif");

        let run = std::process::Command::new("avifdec")
            .arg(&in_path)
            .arg(&out_path)
            .output();
        let out = match run {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("{label}: external decoder not installed — leg skipped");
                return;
            }
            Err(e) => panic!("{label}: spawning external decoder failed: {e}"),
        };
        assert!(
            out.status.success(),
            "{label}: external decoder rejected the file: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let y4m = std::fs::read(&out_path).expect("read y4m");
        let (w, h, ctag, planes) = parse_y4m(&y4m).expect("parse y4m");
        assert_eq!((w, h), (32, 32), "{label}: dims");
        assert!(ctag.starts_with(tag), "{label}: chroma tag {ctag}");
        if depth == 8 {
            assert_eq!(planes[0], narrow(&img.y), "{label}: Y exact");
            if chroma != StillChroma::Monochrome {
                assert_eq!(planes[1], narrow(&img.u), "{label}: U exact");
                assert_eq!(planes[2], narrow(&img.v), "{label}: V exact");
            }
        } else {
            assert_eq!(le_u16(&planes[0]), img.y, "{label}: Y exact");
            if chroma != StillChroma::Monochrome {
                assert_eq!(le_u16(&planes[1]), img.u, "{label}: U exact");
                assert_eq!(le_u16(&planes[2]), img.v, "{label}: V exact");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Reverse black-box direction: an independent AVIF **encoder** binary
/// (`avifenc`), when installed, produces lossless 8/10/12-bit files
/// from Y4M sources — and this crate's own decoder must recover the
/// exact source planes through the (HBD) composition layer. Skips
/// silently when the binary is not on PATH.
#[test]
fn black_box_external_encoder_files_decode_exact() {
    let tmp = std::env::temp_dir().join(format!("oxideav-avif-bbe-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("tmp dir");

    /// Serialize one frame as Y4M at the given depth/chroma tag.
    fn write_y4m(
        path: &std::path::Path,
        w: u32,
        h: u32,
        tag: &str,
        depth: u8,
        planes: [&[u16]; 3],
    ) {
        let mut out = format!("YUV4MPEG2 W{w} H{h} F25:1 Ip A1:1 C{tag}\nFRAME\n").into_bytes();
        let two_byte = depth > 8;
        for p in planes {
            for &s in p {
                if two_byte {
                    out.extend_from_slice(&s.to_le_bytes());
                } else {
                    out.push(s as u8);
                }
            }
        }
        std::fs::write(path, out).expect("write y4m");
    }

    let cases = [
        (StillChroma::Yuv420, 8u8, "420jpeg"),
        (StillChroma::Yuv420, 10, "420p10"),
        (StillChroma::Yuv444, 12, "444p12"),
    ];
    for (chroma, depth, tag) in cases {
        let label = format!("black-box encode {depth}-bit {chroma:?}");
        let (w, h) = (32u32, 32u32);
        let img = build_image(w, h, depth, chroma);
        let y4m_path = tmp.join(format!("bbe_{tag}.y4m"));
        let avif_path = tmp.join(format!("bbe_{tag}.avif"));
        write_y4m(&y4m_path, w, h, tag, depth, [&img.y, &img.u, &img.v]);

        // Quality 100 = quantizer 0 = a lossless YUV encode without
        // the identity-matrix implication (which is illegal with
        // chroma subsampling).
        let run = std::process::Command::new("avifenc")
            .arg("-q")
            .arg("100")
            .arg(&y4m_path)
            .arg(&avif_path)
            .output();
        let out = match run {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("{label}: external encoder not installed — leg skipped");
                return;
            }
            Err(e) => panic!("{label}: spawning external encoder failed: {e}"),
        };
        assert!(
            out.status.success(),
            "{label}: external encoder failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let avif = std::fs::read(&avif_path).expect("read avif");

        let vf = decode_own(&label, &avif);
        assert_eq!(vf.planes.len(), 3, "{label}: planes");
        if depth == 8 {
            assert_eq!(vf.planes[0].data, narrow(&img.y), "{label}: Y exact");
            assert_eq!(vf.planes[1].data, narrow(&img.u), "{label}: U exact");
            assert_eq!(vf.planes[2].data, narrow(&img.v), "{label}: V exact");
        } else {
            assert_eq!(le_u16(&vf.planes[0].data), img.y, "{label}: Y exact");
            assert_eq!(le_u16(&vf.planes[1].data), img.u, "{label}: U exact");
            assert_eq!(le_u16(&vf.planes[2].data), img.v, "{label}: V exact");
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
