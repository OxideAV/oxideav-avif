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
/// registry decoder — the high-bit-depth validation path (this crate's
/// composition layer is 8-bit; the payload itself decodes fine).
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
/// sample-exact. 8-bit legs decode through this crate's own decoder;
/// 10/12-bit legs decode the extracted payload through the AV1
/// registry decoder (little-endian 2-byte planes). Also pins the
/// av1C field mapping and the §8 profile-brand election per pairing.
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

            // Sample-exact decode-back.
            if bit_depth == 8 {
                let vf = decode_own(&label, &avif);
                assert_eq!(vf.planes.len(), nplanes, "{label}: planes");
                assert_eq!(vf.planes[0].data, narrow(&img.y), "{label}: Y");
                if nplanes == 3 {
                    assert_eq!(vf.planes[1].data, narrow(&img.u), "{label}: U");
                    assert_eq!(vf.planes[2].data, narrow(&img.v), "{label}: V");
                }
            } else {
                let vf = decode_payload_av1(&label, &avif);
                assert_eq!(vf.planes.len(), nplanes, "{label}: planes");
                assert_eq!(le_u16(&vf.planes[0].data), img.y, "{label}: Y");
                if nplanes == 3 {
                    assert_eq!(le_u16(&vf.planes[1].data), img.u, "{label}: U");
                    assert_eq!(le_u16(&vf.planes[2].data), img.v, "{label}: V");
                }
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
/// `shall` holds at high bit depth too. The 8-bit composition layer
/// doesn't cover HBD yet, so both items validate sample-exact through
/// the AV1 registry decoder on the extracted payloads.
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

    // Primary payload sample-exact at 10 bits.
    let vf = decode_payload_av1("10-bit master", &avif);
    assert_eq!(le_u16(&vf.planes[0].data), img.y, "Y");

    // Alpha payload sample-exact at 10 bits (resolved via auxl iref).
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

    // The grid path rejects pass-through props until it grows them.
    assert!(encode_still_grid(&img, &StillEncodeOptions::default(), 2, 1).is_err());
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
    let (w, h) = (70u32, 50u32);
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

/// Grid guards: alpha is not yet supported on the grid path, and a
/// tiling that leaves fully-trimmed tiles is rejected up front.
#[test]
fn grid_encode_guards() {
    let img = build_image(16, 16, 8, StillChroma::Yuv420);
    let with_alpha = img.clone().with_alpha(plane(16, 16, 8, 5)).unwrap();
    assert!(encode_still_grid(&with_alpha, &StillEncodeOptions::default(), 2, 1).is_err());
    // 16 wide split into 3 columns → 8-wide tiles → column 2 starts at
    // x=16, past the canvas: rejected.
    assert!(encode_still_grid(&img, &StillEncodeOptions::default(), 3, 1).is_err());
}

// ─────────────────── black-box acceptance ───────────────────

/// Minimal Y4M reader for the black-box leg: returns
/// `(width, height, chroma_tag, planes)` of the first frame.
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
    let ylen = (w * h) as usize;
    let clen = (cw * ch) as usize;
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
        (StillChroma::Yuv420, "420"),
        (StillChroma::Yuv444, "444"),
        (StillChroma::Monochrome, "mono"),
    ];
    for (chroma, tag) in cases {
        let label = format!("black-box {chroma:?}");
        let img = build_image(32, 32, 8, chroma);
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
        assert_eq!(planes[0], narrow(&img.y), "{label}: Y exact");
        if chroma != StillChroma::Monochrome {
            assert_eq!(planes[1], narrow(&img.u), "{label}: U exact");
            assert_eq!(planes[2], narrow(&img.v), "{label}: V exact");
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
