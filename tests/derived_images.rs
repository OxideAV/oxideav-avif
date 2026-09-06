//! Derived-image round trips: overlay (`iovl`) and identity (`iden`)
//! encodes through the `still` module, decoded back through this
//! crate's own recursive derived-image decoder (sample-exact where the
//! composition is lossless), container audits, and a black-box leg
//! against an external HEIF/AVIF decoder binary (`heif-dec`) when one
//! is installed — the external AVIF-only decoder binary does not
//! implement overlay / identity derivations.

#![cfg(feature = "registry")]

use oxideav_avif::{
    audit_iden_derivations, encode_still, encode_still_overlay, inspect, parse_header, AvifDecoder,
    IdentityDerivation, OverlayCanvas, OverlayLayerImage, StillChroma, StillEncodeOptions,
    StillImage, StillProperties, ITEM_TYPE_IDEN, ITEM_TYPE_IOVL,
};
use oxideav_core::{CodecId, Decoder, Frame, Packet, TimeBase};

// ───────────────────────── helpers ─────────────────────────

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

fn build_image(w: u32, h: u32, bit_depth: u8, chroma: StillChroma, seed: u32) -> StillImage {
    let (sx, sy) = match chroma {
        StillChroma::Yuv420 => (1, 1),
        StillChroma::Yuv422 => (1, 0),
        StillChroma::Yuv444 | StillChroma::Monochrome => (0, 0),
    };
    let (y, u, v) = if chroma == StillChroma::Monochrome {
        (plane(w, h, bit_depth, seed), Vec::new(), Vec::new())
    } else {
        (
            plane(w, h, bit_depth, seed),
            plane(w >> sx, h >> sy, bit_depth, seed + 1),
            plane(w >> sx, h >> sy, bit_depth, seed + 2),
        )
    };
    StillImage::yuv(w, h, bit_depth, chroma, y, u, v).expect("build image")
}

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

fn words(data: &[u8], two_byte: bool) -> Vec<u16> {
    if two_byte {
        data.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    } else {
        data.iter().map(|&b| b as u16).collect()
    }
}

/// One image rendered by the external HEIF decoder: `(width, height,
/// RGBA samples)` — 8-bit per channel for `depth == 8`, big-endian
/// 16-bit otherwise, row-major, 4 channels.
type ExternalImage = (u32, u32, Vec<u8>);

/// Render `avif` with the external HEIF decoder binary (`heif-dec`,
/// PNG output — it composites derived images in RGB) and convert every
/// emitted image to raw RGBA through ImageMagick (`magick`). Returns
/// the images in output order (a file exposing several non-hidden
/// images yields several); `None` when either binary is not installed
/// (leg skipped); panics when a binary is installed but fails.
fn heif_dec_rgba(label: &str, avif: &[u8], depth: u8) -> Option<Vec<ExternalImage>> {
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-avif-derived-{}-{}",
        std::process::id(),
        label.replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let in_path = tmp.join("in.avif");
    std::fs::write(&in_path, avif).expect("write avif");
    let run = std::process::Command::new("heif-dec")
        .arg("--quiet")
        .arg("-C")
        .arg("nn")
        .arg(&in_path)
        .arg(tmp.join("out.png"))
        .output();
    let out = match run {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("{label}: external HEIF decoder not installed — leg skipped");
            return None;
        }
        Err(e) => panic!("{label}: spawning external HEIF decoder failed: {e}"),
    };
    assert!(
        out.status.success(),
        "{label}: external HEIF decoder rejected the file: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut pngs: Vec<std::path::PathBuf> = std::fs::read_dir(&tmp)
        .expect("read tmp")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "png"))
        .collect();
    pngs.sort();
    assert!(!pngs.is_empty(), "{label}: external decoder wrote no image");
    let mut images = Vec::with_capacity(pngs.len());
    for (i, png) in pngs.iter().enumerate() {
        let raw = tmp.join(format!("img{i}.rgba"));
        let run = std::process::Command::new("magick")
            .arg(png)
            .arg("-depth")
            .arg(if depth == 8 { "8" } else { "16" })
            .arg(format!("rgba:{}", raw.display()))
            .output();
        let out = match run {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("{label}: ImageMagick not installed — leg skipped");
                return None;
            }
            Err(e) => panic!("{label}: spawning ImageMagick failed: {e}"),
        };
        assert!(
            out.status.success(),
            "{label}: ImageMagick failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let dims = std::process::Command::new("magick")
            .arg("identify")
            .arg("-format")
            .arg("%w %h")
            .arg(png)
            .output()
            .expect("identify");
        let dims = String::from_utf8_lossy(&dims.stdout);
        let mut it = dims
            .split_whitespace()
            .map(|v| v.parse::<u32>().expect("dim"));
        let (w, h) = (it.next().unwrap(), it.next().unwrap());
        let bytes = std::fs::read(&raw).expect("read raw");
        let bps = if depth == 8 { 1 } else { 2 };
        assert_eq!(bytes.len(), (w * h * 4) as usize * bps, "{label}: raw size");
        images.push((w, h, bytes));
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Some(images)
}

/// Black-box equivalence leg: the external decoder must render
/// `derived` (an overlay / identity file) and `flat` (a plain encode of
/// the same expected pixels) to identical RGBA rasters — both go
/// through the same external colour pipeline, so any composition
/// difference shows as a byte mismatch.
fn assert_external_equivalent(label: &str, derived: &[u8], flat: &[u8], depth: u8) {
    let Some(d) = heif_dec_rgba(&format!("{label} derived"), derived, depth) else {
        return;
    };
    let f = heif_dec_rgba(&format!("{label} flat"), flat, depth).expect("flat leg");
    assert_eq!(f.len(), 1, "{label}: flat reference renders one image");
    let (fw, fh, fbytes) = &f[0];
    let matching = d
        .iter()
        .find(|(w, h, _)| (w, h) == (fw, fh))
        .unwrap_or_else(|| panic!("{label}: no external output with dims {fw}x{fh}"));
    assert!(
        matching.2 == *fbytes,
        "{label}: external render of the derived file differs from the flat reference"
    );
}

/// Expected canvas luma for opaque layers: last layer covering a pixel
/// wins; `fill` elsewhere.
fn expected_luma(
    cw: u32,
    ch: u32,
    layers: &[(&StillImage, i32, i32)],
    fill: Option<u16>,
) -> Vec<Option<u16>> {
    let mut out = vec![fill; (cw * ch) as usize];
    for (img, ox, oy) in layers {
        for y in 0..img.height {
            for x in 0..img.width {
                let cx = i64::from(x) + i64::from(*ox);
                let cy = i64::from(y) + i64::from(*oy);
                if cx < 0 || cy < 0 || cx >= i64::from(cw) || cy >= i64::from(ch) {
                    continue;
                }
                out[(cy as u32 * cw + cx as u32) as usize] =
                    Some(img.y[(y * img.width + x) as usize]);
            }
        }
    }
    out
}

// ───────────────────────── overlays ─────────────────────────

/// Two opaque 4:2:0 layers (a base covering the canvas + a stamp at an
/// even offset): the composed canvas is sample-exact on every plane
/// through our decoder, and the external HEIF decoder agrees.
#[test]
fn overlay_8bit_420_two_opaque_layers_round_trips_exact() {
    let base = build_image(32, 24, 8, StillChroma::Yuv420, 1);
    let stamp = build_image(16, 8, 8, StillChroma::Yuv420, 7);
    let canvas = OverlayCanvas::new(32, 24);
    let layers = [
        OverlayLayerImage {
            image: &base,
            x: 0,
            y: 0,
        },
        OverlayLayerImage {
            image: &stamp,
            x: 8,
            y: 8,
        },
    ];
    let avif = encode_still_overlay(&canvas, &layers, &StillEncodeOptions::default())
        .expect("overlay encode");

    // Container: iovl primary, two hidden av01 inputs, resolved placements.
    let hdr = parse_header(&avif).expect("parse");
    let primary = hdr.meta.primary_item_id.unwrap();
    assert_eq!(
        hdr.meta.item_by_id(primary).unwrap().item_type,
        ITEM_TYPE_IOVL
    );
    let info = inspect(&avif).expect("inspect");
    assert_eq!((info.width, info.height), (32, 24));
    let res = &info.overlay_resolutions[0];
    assert_eq!(res.canvas(), (32, 24));
    assert_eq!(res.placements.len(), 2);
    assert_eq!(
        (res.placements[1].offset_x, res.placements[1].offset_y),
        (8, 8)
    );
    assert_eq!(
        (
            res.placements[1].input_width,
            res.placements[1].input_height
        ),
        (16, 8)
    );
    assert!(!res.canvas_partially_filled());

    // Pixels: own decoder.
    let vf = decode_own("overlay 420", &avif);
    assert_eq!(vf.planes.len(), 3);
    let expect_y = expected_luma(32, 24, &[(&base, 0, 0), (&stamp, 8, 8)], None);
    let got_y = words(&vf.planes[0].data, false);
    assert_eq!(got_y.len(), 32 * 24);
    for (i, (g, e)) in got_y.iter().zip(&expect_y).enumerate() {
        assert_eq!(*g, e.unwrap(), "Y at {},{}", i % 32, i / 32);
    }
    // Chroma: even offsets → whole chroma samples copy through.
    let got_u = words(&vf.planes[1].data, false);
    let got_v = words(&vf.planes[2].data, false);
    for cy in 0..12u32 {
        for cx in 0..16u32 {
            let i = (cy * 16 + cx) as usize;
            let (eu, ev) = if (4..12).contains(&cx) && (4..8).contains(&cy) {
                let j = ((cy - 4) * 8 + (cx - 4)) as usize;
                (stamp.u[j], stamp.v[j])
            } else {
                (base.u[i], base.v[i])
            };
            assert_eq!(got_u[i], eu, "U at {cx},{cy}");
            assert_eq!(got_v[i], ev, "V at {cx},{cy}");
        }
    }

    // Black box: the external decoder renders the overlay file exactly
    // like a flat encode of the composed planes.
    let flat = StillImage::yuv(
        32,
        24,
        8,
        StillChroma::Yuv420,
        got_y.clone(),
        got_u.clone(),
        got_v.clone(),
    )
    .unwrap();
    let flat = encode_still(&flat, &StillEncodeOptions::default()).expect("flat encode");
    assert_external_equivalent("overlay 420", &avif, &flat, 8);
}

/// 10-bit 4:2:2 layers with a partially off-canvas stamp (negative
/// offset) — clipping per §6.6.2.2.3, sample-exact through the HBD
/// composition path.
#[test]
fn overlay_10bit_422_clipped_stamp_round_trips_exact() {
    let base = build_image(24, 16, 10, StillChroma::Yuv422, 3);
    let stamp = build_image(8, 8, 10, StillChroma::Yuv422, 11);
    let canvas = OverlayCanvas::new(24, 16);
    let layers = [
        OverlayLayerImage {
            image: &base,
            x: 0,
            y: 0,
        },
        OverlayLayerImage {
            image: &stamp,
            x: -2,
            y: 12,
        },
    ];
    let avif = encode_still_overlay(&canvas, &layers, &StillEncodeOptions::default())
        .expect("overlay encode");
    let vf = decode_own("overlay 422p10", &avif);
    assert_eq!(vf.planes.len(), 3);
    let expect_y = expected_luma(24, 16, &[(&base, 0, 0), (&stamp, -2, 12)], None);
    let got_y = words(&vf.planes[0].data, true);
    for (i, (g, e)) in got_y.iter().zip(&expect_y).enumerate() {
        assert_eq!(*g, e.unwrap(), "Y at {},{}", i % 24, i / 24);
    }
    let got_u = words(&vf.planes[1].data, true);
    // Stamp at x=-2: canvas chroma column cx covers luma 2cx..2cx+2 →
    // stamp luma 2cx+2.. → stamp chroma cx+1.
    for cy in 12..16u32 {
        for cx in 0..3u32 {
            let i = (cy * 12 + cx) as usize;
            let j = ((cy - 12) * 4 + cx + 1) as usize;
            assert_eq!(got_u[i], stamp.u[j], "U at {cx},{cy}");
        }
        assert_eq!(
            got_u[(cy * 12 + 3) as usize],
            base.u[(cy * 12 + 3) as usize]
        );
    }
    // (No external leg: the external HEIF decoder's rendition of
    // high-bit-depth overlays diverges from its own flat renders even
    // at aligned offsets — an observed tool limitation, so the 8-bit
    // legs carry the black-box cross-check.)
}

/// Identity-matrix RGB(A) layers: a translucent RGBA stamp over an
/// opaque RGB base with a transparent fill — the §6.9.1 straight
/// alpha-over is checked against a float evaluation, the canvas
/// opacity plane rides out as `Yuva444P`, and the fill converts
/// exactly on the uncovered strip.
#[test]
fn overlay_rgba_identity_alpha_over_and_transparent_fill() {
    let (bw, bh) = (16u32, 12u32);
    let rgb: Vec<u8> = (0..bw * bh * 3).map(|i| (i * 37 % 251) as u8).collect();
    let base = StillImage::rgb8(bw, bh, &rgb).expect("base");
    let (sw, sh) = (8u32, 8u32);
    let rgba: Vec<u8> = (0..sw * sh)
        .flat_map(|i| {
            let x = i % sw;
            [
                (i * 53 % 255) as u8,
                (i * 11 % 255) as u8,
                (i * 3 % 255) as u8,
                (x * 36) as u8,
            ]
        })
        .collect();
    let stamp = StillImage::rgba8(sw, sh, &rgba).expect("stamp");
    // Canvas wider than the base: columns 16..20 show the fill.
    let mut canvas = OverlayCanvas::new(20, 12);
    canvas.fill_rgba = [0x4000, 0x8000, 0xC000, 0x0000];
    let layers = [
        OverlayLayerImage {
            image: &base,
            x: 0,
            y: 0,
        },
        OverlayLayerImage {
            image: &stamp,
            x: 4,
            y: 2,
        },
    ];
    let avif = encode_still_overlay(&canvas, &layers, &StillEncodeOptions::default())
        .expect("overlay encode");
    let info = inspect(&avif).expect("inspect");
    assert!(info.overlay_resolutions[0].canvas_partially_filled());

    let vf = decode_own("overlay rgba", &avif);
    assert_eq!(vf.planes.len(), 4, "Yuva444P expected");
    let g = |p: usize| words(&vf.planes[p].data, false);
    let (oy, ou, ov, oa) = (g(0), g(1), g(2), g(3));
    for y in 0..12u32 {
        for x in 0..20u32 {
            let i = (y * 20 + x) as usize;
            if x >= 16 {
                // Transparent fill: opacity 0, colour = fill (Y=G,
                // Cb=B, Cr=R scaled 16→8 bits).
                assert_eq!(oa[i], 0, "alpha at {x},{y}");
                // 16-bit fill code values narrowed to 8 bits: 0x8000 →
                // 128, 0xC000 → 192, 0x4000 → 64.
                assert_eq!((oy[i], ou[i], ov[i]), (128, 192, 64), "fill at {x},{y}");
                continue;
            }
            assert_eq!(oa[i], 255, "alpha at {x},{y}");
            let bi = (y * bw + x) as usize;
            let (br, bg, bb) = (base.v[bi] as f64, base.y[bi] as f64, base.u[bi] as f64);
            let inside = (4..12).contains(&x) && (2..10).contains(&y);
            let (er, eg, eb) = if inside {
                let si = ((y - 2) * sw + (x - 4)) as usize;
                let a = stamp.alpha.as_ref().unwrap()[si] as f64 / 255.0;
                let (sr, sg, sb) = (stamp.v[si] as f64, stamp.y[si] as f64, stamp.u[si] as f64);
                (
                    sr * a + br * (1.0 - a),
                    sg * a + bg * (1.0 - a),
                    sb * a + bb * (1.0 - a),
                )
            } else {
                (br, bg, bb)
            };
            let close = |got: u16, e: f64| (got as f64 - e).abs() <= 1.0;
            assert!(close(oy[i], eg), "G at {x},{y}: {} vs {eg}", oy[i]);
            assert!(close(ou[i], eb), "B at {x},{y}: {} vs {eb}", ou[i]);
            assert!(close(ov[i], er), "R at {x},{y}: {} vs {er}", ov[i]);
        }
    }
    // Black box: the external decoder composites the same file in RGB;
    // with the identity matrix its colour raster is directly comparable
    // — exact on copied samples and on the fill, ±1 where the alpha
    // blend rounds. (It flattens the canvas opacity into an opaque PNG,
    // so only the colour channels are compared.)
    if let Some(ext) = heif_dec_rgba("overlay rgba", &avif, 8) {
        let (w, h, rgba) = &ext[0];
        assert_eq!((*w, *h), (20, 12));
        for y in 0..12u32 {
            for x in 0..20u32 {
                let i = (y * 20 + x) as usize;
                let px = &rgba[i * 4..i * 4 + 4];
                let ours = [ov[i], oy[i], ou[i]]; // R, G, B
                for c in 0..3 {
                    assert!(
                        (ours[c] as i32 - px[c] as i32).abs() <= 1,
                        "external channel {c} at {x},{y}: ours {} theirs {}",
                        ours[c],
                        px[c]
                    );
                }
            }
        }
    }
}

/// Monochrome 12-bit layers at an odd offset plus `irot` on the overlay
/// item: luma exact, rotation applied to the composed canvas.
#[test]
fn overlay_12bit_mono_odd_offset_with_irot() {
    let base = build_image(12, 10, 12, StillChroma::Monochrome, 5);
    let stamp = build_image(5, 3, 12, StillChroma::Monochrome, 9);
    let mut canvas = OverlayCanvas::new(12, 10);
    canvas.irot = Some(1);
    let layers = [
        OverlayLayerImage {
            image: &base,
            x: 0,
            y: 0,
        },
        OverlayLayerImage {
            image: &stamp,
            x: 3,
            y: 5,
        },
    ];
    let avif = encode_still_overlay(&canvas, &layers, &StillEncodeOptions::default())
        .expect("overlay encode");
    let info = inspect(&avif).expect("inspect");
    assert_eq!(
        (info.width, info.height),
        (10, 12),
        "irot swaps the canvas extents"
    );
    let vf = decode_own("overlay mono12 irot", &avif);
    assert_eq!(vf.planes.len(), 1);
    let got = words(&vf.planes[0].data, true);
    let unrotated = expected_luma(12, 10, &[(&base, 0, 0), (&stamp, 3, 5)], None);
    // irot angle 1 = 90° anti-clockwise: out(x, y) = in(W-1-y, x) with
    // out extents (H, W).
    for y in 0..12u32 {
        for x in 0..10u32 {
            let src = unrotated[((x) * 12 + (12 - 1 - y)) as usize].unwrap();
            assert_eq!(got[(y * 10 + x) as usize], src, "rotated Y at {x},{y}");
        }
    }
    // Black box (8-bit twin of the same geometry — the external decoder
    // renders high-bit-depth overlays unreliably): the overlay file
    // renders identically to a flat mono encode carrying the same irot.
    let base8 = build_image(12, 10, 8, StillChroma::Monochrome, 5);
    let stamp8 = build_image(5, 3, 8, StillChroma::Monochrome, 9);
    let layers8 = [
        OverlayLayerImage {
            image: &base8,
            x: 0,
            y: 0,
        },
        OverlayLayerImage {
            image: &stamp8,
            x: 3,
            y: 5,
        },
    ];
    let avif8 = encode_still_overlay(&canvas, &layers8, &StillEncodeOptions::default())
        .expect("overlay encode 8-bit");
    let composed: Vec<u16> = expected_luma(12, 10, &[(&base8, 0, 0), (&stamp8, 3, 5)], None)
        .iter()
        .map(|v| v.unwrap())
        .collect();
    let props = StillProperties {
        irot: Some(1),
        ..Default::default()
    };
    let flat = StillImage::yuv(
        12,
        10,
        8,
        StillChroma::Monochrome,
        composed,
        Vec::new(),
        Vec::new(),
    )
    .unwrap()
    .with_props(props);
    let flat = encode_still(&flat, &StillEncodeOptions::default()).expect("flat encode");
    assert_external_equivalent("overlay mono8 irot", &avif8, &flat, 8);
}

/// Encoder guards: mixed layouts and empty layer lists are refused.
#[test]
fn overlay_encode_guards() {
    let a = build_image(8, 8, 8, StillChroma::Yuv420, 1);
    let b = build_image(8, 8, 10, StillChroma::Yuv420, 1);
    let canvas = OverlayCanvas::new(8, 8);
    let err = encode_still_overlay(
        &canvas,
        &[
            OverlayLayerImage {
                image: &a,
                x: 0,
                y: 0,
            },
            OverlayLayerImage {
                image: &b,
                x: 0,
                y: 0,
            },
        ],
        &StillEncodeOptions::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("share one coded layout"), "{err}");
    assert!(encode_still_overlay(&canvas, &[], &StillEncodeOptions::default()).is_err());
    assert!(encode_still_overlay(
        &OverlayCanvas::new(0, 8),
        &[OverlayLayerImage {
            image: &a,
            x: 0,
            y: 0
        }],
        &StillEncodeOptions::default()
    )
    .is_err());
}

// ───────────────────────── identity ─────────────────────────

/// `iden` primary carrying `irot` over an untransformed coded item:
/// the coded item stays exposed, the iden has no extents, and the
/// decode is the rotated image — sample-exact, also through the
/// external decoder.
#[test]
fn identity_derivation_rotates_and_round_trips() {
    let img = build_image(12, 8, 8, StillChroma::Yuv420, 21);
    let props = StillProperties {
        identity_derivation: Some(IdentityDerivation {
            clap: None,
            irot: Some(3),
            imir: None,
        }),
        ..Default::default()
    };
    let img = img.with_props(props);
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");

    let hdr = parse_header(&avif).expect("parse");
    let primary = hdr.meta.primary_item_id.unwrap();
    let pinfo = hdr.meta.item_by_id(primary).unwrap();
    assert_eq!(pinfo.item_type, ITEM_TYPE_IDEN);
    assert!(!pinfo.is_hidden());
    assert!(
        hdr.meta.location_by_id(primary).is_none(),
        "iden has no extents"
    );
    let audits = audit_iden_derivations(&hdr.meta);
    assert_eq!(audits.len(), 1);
    assert!(audits[0].is_compliant(), "{:?}", audits[0].missing());
    let info = inspect(&avif).expect("inspect");
    assert_eq!((info.width, info.height), (8, 12));
    assert_eq!(info.iden_resolutions[0].output_dims, Some((8, 12)));

    let vf = decode_own("iden irot", &avif);
    let got = words(&vf.planes[0].data, false);
    // irot 3 = 270° anti-clockwise = 90° clockwise: out(x, y) = in(y, H-1-x)
    // with out extents (H, W) = (8, 12).
    for y in 0..12u32 {
        for x in 0..8u32 {
            let src = img.y[((8 - 1 - x) * 12 + y) as usize];
            assert_eq!(got[(y * 8 + x) as usize], src, "Y at {x},{y}");
        }
    }
    // Black box: the external decoder renders the iden primary exactly
    // like a plain encode carrying the same irot on the coded item.
    let flat_props = StillProperties {
        irot: Some(3),
        ..Default::default()
    };
    let flat = build_image(12, 8, 8, StillChroma::Yuv420, 21).with_props(flat_props);
    let flat = encode_still(&flat, &StillEncodeOptions::default()).expect("flat encode");
    assert_external_equivalent("iden irot", &avif, &flat, 8);
}

/// `iden` with `clap` + `imir` over a coded item that carries alpha:
/// the identity derivation inherits the alpha auxiliary (§6.6.1) and
/// crops colour and alpha together.
#[test]
fn identity_derivation_clap_imir_with_alpha() {
    let (w, h) = (16u32, 8u32);
    let rgba: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            [
                (i * 7 % 255) as u8,
                (i * 13 % 255) as u8,
                (i * 29 % 255) as u8,
                (i % 255) as u8,
            ]
        })
        .collect();
    let img = StillImage::rgba8(w, h, &rgba).expect("rgba");
    let props = StillProperties {
        identity_derivation: Some(IdentityDerivation {
            // Centre 8×4 crop.
            clap: Some(oxideav_avif::Clap {
                clean_aperture_width_n: 8,
                clean_aperture_width_d: 1,
                clean_aperture_height_n: 4,
                clean_aperture_height_d: 1,
                horiz_off_n: 0,
                horiz_off_d: 1,
                vert_off_n: 0,
                vert_off_d: 1,
            }),
            irot: None,
            imir: Some(1),
        }),
        ..Default::default()
    };
    let img = img.with_props(props);
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode");
    let info = inspect(&avif).expect("inspect");
    assert_eq!((info.width, info.height), (8, 4));
    let vf = decode_own("iden clap imir alpha", &avif);
    assert_eq!(vf.planes.len(), 4, "alpha inherited through the iden");
    let alpha = img.alpha.as_ref().unwrap();
    let got_a = words(&vf.planes[3].data, false);
    let got_y = words(&vf.planes[0].data, false);
    // Crop origin (4, 2); imir axis 1 mirrors left↔right.
    for y in 0..4u32 {
        for x in 0..8u32 {
            let sx = 4 + (8 - 1 - x);
            let sy = 2 + y;
            let si = (sy * w + sx) as usize;
            assert_eq!(got_a[(y * 8 + x) as usize], alpha[si], "A at {x},{y}");
            assert_eq!(got_y[(y * 8 + x) as usize], img.y[si], "G at {x},{y}");
        }
    }
    // Black box: identity-matrix RGBA is directly comparable; the file
    // exposes two images (coded + iden), pick the cropped one.
    if let Some(ext) = heif_dec_rgba("iden clap imir alpha", &avif, 8) {
        let (_, _, rgba) = ext
            .iter()
            .find(|(w, h, _)| (*w, *h) == (8, 4))
            .expect("external output at the iden's 8x4 extents");
        for y in 0..4u32 {
            for x in 0..8u32 {
                let i = (y * 8 + x) as usize;
                let si = ((2 + y) * w + 4 + (8 - 1 - x)) as usize;
                // Colour only — the external decoder does not carry the
                // coded item's alpha auxiliary through the identity
                // derivation into its PNG.
                let px = &rgba[i * 4..i * 4 + 3];
                assert_eq!(
                    px,
                    [img.v[si] as u8, img.y[si] as u8, img.u[si] as u8],
                    "external RGB at {x},{y}"
                );
            }
        }
    }
}
