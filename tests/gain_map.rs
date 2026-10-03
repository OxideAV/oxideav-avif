//! Gain maps (`tmap`) in AVIF — av1-avif §4.2.2 over HEIF Amd 1:2025
//! §6.6.2.4 and ISO 21496-1: the base-by-default / opt-in applied
//! decode, the metadata wrapper, and authoring through
//! `StillProperties::gain_map`. The black-box legs run
//! `avifgainmaputil` (tone-map / print the metadata) when it is
//! installed and skip otherwise.

use oxideav_avif::meta::{Colr, Property};
use oxideav_avif::{
    encode_still, gain_map_metadata, inspect, parse_header, AvifDecoder, EncodeOptions,
    GainMapSpec, StillChroma, StillImage, StillProperties,
};
use oxideav_core::{
    CodecId, CodecParameters, ColorSignal, Decoder, Frame, Packet, PixelFormat, TimeBase,
};
use oxideav_heif::gainmap::{GainMapChannel, GainMapMetadata, Rational};
use oxideav_heif::props as hprops;
use oxideav_heif::{to_rgb, Chroma, HeifFrame, HeifPixelFormat, HeifPlane};

/// `avifgainmaputil combine` of a 32×24 sRGB base gradient and a
/// BT.2100 PQ alternate gradient (lossless, 8-bit 4:4:4, 4:4:4 8-bit
/// gain map, alternate headroom 4).
const TOOL: &[u8] = include_bytes!("fixtures/gainmap_tool.avif");
/// `avifgainmaputil tonemap --headroom 4 -q 100 -y 444 -d 10
/// --cicp-output 1/16/0` of the file above: the fully applied
/// rendition as a lossless 10-bit identity-matrix (planar RGB) item
/// in the `tmap` item's own primaries (the tool wrote the alternate
/// `colr` as 1 / 16 / 9, so no gamut conversion is involved).
const TOOL_APPLIED: &[u8] = include_bytes!("fixtures/gainmap_tool_applied.avif");

fn decode(file: &[u8], tone_mapped: bool) -> (AvifDecoder, oxideav_core::frame::VideoFrame) {
    let mut d =
        AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR)).with_tone_mapped(tone_mapped);
    d.send_packet(&Packet::new(0, TimeBase::new(1, 1), file.to_vec()))
        .expect("send_packet");
    let frame = match d.receive_frame() {
        Ok(Frame::Video(v)) => v,
        other => panic!("expected a video frame, got {other:?}"),
    };
    (d, frame)
}

/// Planes of a decoded frame as `u16` samples (8-bit or 16-bit LE
/// storage, by the stride / width ratio).
fn samples(frame: &oxideav_core::frame::VideoFrame, width: usize) -> Vec<Vec<u16>> {
    frame
        .image_planes()
        .iter()
        .map(|p| {
            if p.stride == width {
                p.data.iter().map(|&b| u16::from(b)).collect()
            } else {
                p.data
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect()
            }
        })
        .collect()
}

/// A 4:4:4 Y′CbCr frame's RGB at its depth, through the container's
/// H.273 conversion with the given `colr`.
fn ycbcr_to_rgb(
    planes: &[Vec<u16>],
    width: u32,
    height: u32,
    bit_depth: u8,
    colr: &Colr,
) -> Vec<u16> {
    let bps = if bit_depth > 8 { 2 } else { 1 };
    let plane = |p: &Vec<u16>| HeifPlane {
        stride: width as usize * bps,
        data: if bps == 1 {
            p.iter().map(|&v| v as u8).collect()
        } else {
            p.iter().flat_map(|v| v.to_le_bytes()).collect()
        },
    };
    let frame = HeifFrame {
        width,
        height,
        format: HeifPixelFormat::new(Chroma::Yuv444, bit_depth, false).unwrap(),
        planes: planes.iter().map(plane).collect(),
    };
    let rgb = to_rgb(&frame, Some(&hprops::Colr::from(colr))).expect("to_rgb");
    assert_eq!(rgb.channels, 3);
    rgb.data
}

/// Interleave the G / B / R planes of a `Gbrp*` frame as R G B.
fn gbr_to_rgb(planes: &[Vec<u16>]) -> Vec<u16> {
    planes[2]
        .iter()
        .zip(&planes[0])
        .zip(&planes[1])
        .flat_map(|((&r, &g), &b)| [r, g, b])
        .collect()
}

/// Max absolute difference between two sample sets, with the second
/// rescaled from `from_bits` to `to_bits` (rounded).
fn max_diff(a: &[u16], b: &[u16], from_bits: u8, to_bits: u8) -> u16 {
    assert_eq!(a.len(), b.len());
    let (amax, bmax) = ((1u32 << to_bits) - 1, (1u32 << from_bits) - 1);
    a.iter()
        .zip(b)
        .map(|(&x, &y)| {
            let y = ((u32::from(y) * amax + bmax / 2) / bmax) as u16;
            x.abs_diff(y)
        })
        .max()
        .unwrap_or(0)
}

/// The tool-authored file decodes to its base by default — the same
/// pixels as the base `av01` primary — and the `tmap` body parses as
/// a `ToneMapImage` (version 0 + C.2) with the headrooms the tool
/// printed.
#[test]
fn tool_file_decodes_base_by_default_and_metadata_parses() {
    let info = inspect(TOOL).expect("inspect");
    assert_eq!(info.tmap_item_ids.len(), 1);
    let tmap = info.tmap_item_ids[0];
    let meta = gain_map_metadata(TOOL, tmap).expect("ToneMapImage + C.2 parse");
    assert_eq!(meta.base_hdr_headroom.as_f64(), 0.0);
    assert_eq!(meta.alternate_hdr_headroom.as_f64(), 4.0);
    assert!(meta.use_base_colour_space);
    let compliance = &info.tone_map_compliance[0];
    assert!(compliance.paired_in_altr, "base + tmap in an altr group");
    assert!(compliance.gain_maps_hidden, "gain map hidden");
    let (d, base) = decode(TOOL, false);
    assert!(!d.tone_mapped());
    assert_eq!(d.output_format(), Some(PixelFormat::YuvJ444P));
    assert_eq!(
        d.color_signal(),
        Some(ColorSignal::from_code_points(1, 13, 6, true))
    );
    // The base is the primary: a plain decode of the same bytes.
    let (_, plain) = decode(TOOL, false);
    assert_eq!(samples(&base, 32), samples(&plain, 32));
}

/// Applied: the reconstruction of the tool's file matches the tool's
/// own tone-map at the alternate headroom (its fully applied
/// rendition, 10-bit planar RGB) within one 8-bit code.
#[test]
fn tool_file_applied_matches_the_tool_within_one_code() {
    let (d, ours) = decode(TOOL, true);
    assert!(d.tone_mapped());
    let sig = d.color_signal().expect("signal");
    assert_eq!(sig.transfer.0, 16, "the tmap colr: PQ alternate");
    assert_eq!(sig.matrix.0, 9);
    assert_eq!(sig.primaries.0, 1);
    let hdr = parse_header(TOOL).expect("parse");
    let tmap = inspect(TOOL).unwrap().tmap_item_ids[0];
    let alt_colr = match hdr.meta.property_for(tmap, b"colr") {
        Some(Property::Colr(c)) => c.clone(),
        other => panic!("tmap colr: {other:?}"),
    };
    let ours_planes = samples(&ours, 32);
    assert_eq!(ours_planes.len(), 3);
    let ours_rgb = ycbcr_to_rgb(&ours_planes, 32, 24, 8, &alt_colr);

    let (dt, tool) = decode(TOOL_APPLIED, false);
    assert_eq!(dt.output_format(), Some(PixelFormat::Gbrp10Le));
    let tool_planes = samples(&tool, 32);
    // G B R planes → interleaved RGB.
    let tool_rgb = gbr_to_rgb(&tool_planes);
    let diff = max_diff(&ours_rgb, &tool_rgb, 10, 8);
    eprintln!("tool file: max |ours - tool| = {diff} (8-bit codes)");
    assert!(
        diff <= 1,
        "reconstruction differs from the black-box tone-map by {diff} codes (8-bit)"
    );
}

/// The `gain_map=apply` codec option on the registry factory selects
/// the applied decode; `base` (the default) and an unknown value are
/// handled.
#[test]
fn registry_option_selects_the_applied_decode() {
    let mut params = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
    params.options.insert("gain_map", "apply");
    let mut d = oxideav_avif::make_decoder(&params).expect("decoder");
    d.send_packet(&Packet::new(0, TimeBase::new(1, 1), TOOL.to_vec()))
        .unwrap();
    let frame = match d.receive_frame().unwrap() {
        Frame::Video(v) => v,
        other => panic!("{other:?}"),
    };
    assert_eq!(frame.color_signal().map(|s| s.transfer.0), Some(16));
    params.options.insert("gain_map", "base");
    let mut d = oxideav_avif::make_decoder(&params).expect("decoder");
    d.send_packet(&Packet::new(0, TimeBase::new(1, 1), TOOL.to_vec()))
        .unwrap();
    let frame = match d.receive_frame().unwrap() {
        Frame::Video(v) => v,
        other => panic!("{other:?}"),
    };
    assert_eq!(frame.color_signal().map(|s| s.transfer.0), Some(13));
    params.options.insert("gain_map", "hdr");
    assert!(oxideav_avif::make_decoder(&params).is_err());
    let mut params = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
    params.options.insert("reference_white", "-5");
    assert!(oxideav_avif::make_decoder(&params).is_err());
}

fn rational(num: i64, den: u32) -> Rational {
    Rational { num, den }
}

/// A base gradient + a monochrome gain map + metadata of our own.
fn authored() -> (StillImage, Vec<u8>) {
    let (w, h) = (32u32, 24u32);
    let n = (w * h) as usize;
    // Base: a colourful sRGB gradient coded as 4:4:4 8-bit Y′CbCr.
    let rgb: Vec<u8> = (0..n)
        .flat_map(|i| {
            let x = (i as u32 % w) as f64 / (w - 1) as f64;
            let y = (i as u32 / w) as f64 / (h - 1) as f64;
            [
                (40.0 + 180.0 * x) as u8,
                (60.0 + 150.0 * y) as u8,
                (200.0 - 120.0 * x) as u8,
            ]
        })
        .collect();
    let mut base = StillImage::rgb8(w, h, &rgb).unwrap();
    base.colr = Some(Colr::Nclx {
        colour_primaries: 1,
        transfer_characteristics: 13,
        matrix_coefficients: 0,
        full_range: true,
    });
    // Gain map: a horizontal ramp of normalised log2 gain.
    let map: Vec<u16> = (0..n)
        .map(|i| ((i as u32 % w) * 255 / (w - 1)) as u16)
        .collect();
    let mut map = StillImage::yuv(
        w,
        h,
        8,
        StillChroma::Monochrome,
        map,
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    map.colr = Some(Colr::Nclx {
        colour_primaries: 2,
        transfer_characteristics: 2,
        matrix_coefficients: 2,
        full_range: true,
    });
    let metadata = GainMapMetadata::new(
        0,
        0,
        false,
        true,
        rational(0, 1),
        rational(3, 1),
        vec![GainMapChannel::new(
            rational(0, 1),
            rational(3, 1),
            rational(1, 1),
            rational(1, 64),
            rational(1, 64),
        )],
    );
    base.props = StillProperties::default().with_gain_map(Some(Box::new(GainMapSpec::new(
        map,
        metadata,
        Colr::Nclx {
            colour_primaries: 9,
            transfer_characteristics: 16,
            matrix_coefficients: 9,
            full_range: true,
        },
        Some(oxideav_avif::meta::Clli::new(1600, 400)),
        Some(10),
        0,
    ))));
    let file = encode_still(&base, &EncodeOptions::default()).expect("encode_still");
    (base, file)
}

/// Authoring: the file carries the `tmap` brand, a hidden gain map, the
/// `altr` [tmap, base] group, the alternate `colr` / `clli` / `pixi`
/// on the `tmap`, the metadata round-trips, the base decodes
/// byte-exact by default and the applied decode is a 10-bit PQ picture.
#[test]
fn authored_tone_map_round_trips_and_applies() {
    let (base, file) = authored();
    let hdr = parse_header(&file).expect("parse");
    assert!(
        hdr.compatible_brands.contains(b"tmap"),
        "tmap brand (HEIF Amd 1 §10.2.6)"
    );
    let info = inspect(&file).expect("inspect");
    assert_eq!(info.tmap_item_ids.len(), 1);
    let tmap = info.tmap_item_ids[0];
    assert!(info.tone_map_compliance[0].paired_in_altr);
    assert!(info.tone_map_compliance[0].gain_maps_hidden);
    let meta = gain_map_metadata(&file, tmap).expect("metadata");
    assert_eq!(meta.alternate_hdr_headroom.as_f64(), 3.0);
    assert_eq!(meta.channels[0].gain_map_max.as_f64(), 3.0);
    match hdr.meta.property_for(tmap, b"colr") {
        Some(Property::Colr(Colr::Nclx {
            colour_primaries: 9,
            transfer_characteristics: 16,
            matrix_coefficients: 9,
            full_range: true,
        })) => {}
        other => panic!("tmap colr: {other:?}"),
    }
    assert!(matches!(
        hdr.meta.property_for(tmap, b"clli"),
        Some(Property::Clli(c)) if c.max_content_light_level == 1600
    ));
    assert!(matches!(
        hdr.meta.property_for(tmap, b"pixi"),
        Some(Property::Pixi(p)) if p.bits_per_channel == vec![10, 10, 10]
    ));
    // The gain map's colr: primaries / transfer 2, as coded.
    let gain = hdr.meta.iref_targets(b"dimg", tmap)[1];
    assert!(hdr.meta.item_by_id(gain).unwrap().is_hidden());
    assert!(matches!(
        hdr.meta.property_for(gain, b"colr"),
        Some(Property::Colr(Colr::Nclx {
            colour_primaries: 2,
            transfer_characteristics: 2,
            ..
        }))
    ));
    // Base by default, byte-exact (lossless identity RGB).
    let (d, out) = decode(&file, false);
    assert_eq!(d.output_format(), Some(PixelFormat::Gbrp8));
    let planes = samples(&out, 32);
    assert_eq!(planes[0], base.y, "G");
    assert_eq!(planes[1], base.u, "B");
    assert_eq!(planes[2], base.v, "R");
    // Applied: 10-bit (the tmap pixi), PQ / BT.2020 full range.
    let (d, out) = decode(&file, true);
    assert_eq!(d.output_format(), Some(PixelFormat::Yuv444P10Le));
    assert_eq!(
        d.color_signal(),
        Some(ColorSignal::from_code_points(9, 16, 9, true))
    );
    let planes = samples(&out, 32);
    assert_eq!(planes[0].len(), 32 * 24);
    // The ramp brightens left → right: the right column's luma is
    // above the left's on every row.
    for r in 0..24 {
        assert!(planes[0][r * 32 + 31] > planes[0][r * 32], "row {r}");
    }
}

/// Black box: `avifgainmaputil` reads our file's metadata and its
/// tone-map at the alternate headroom matches our applied decode
/// within one 10-bit code; skipped when the tool is absent.
#[test]
fn authored_tone_map_matches_the_black_box_tool() {
    let (_, file) = authored();
    let tmp = std::env::temp_dir().join(format!("oxideav-avif-tmap-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let in_path = tmp.join("ours.avif");
    let out_path = tmp.join("applied.avif");
    std::fs::write(&in_path, &file).expect("write");
    let printed = match std::process::Command::new("avifgainmaputil")
        .arg("printmetadata")
        .arg(&in_path)
        .output()
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("avifgainmaputil not installed — black-box leg skipped");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
        Err(e) => panic!("spawn failed: {e}"),
        Ok(out) => {
            assert!(
                out.status.success(),
                "the tool rejected our gain map: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
    };
    assert!(
        printed.contains("Alternate headroom: 3"),
        "metadata as printed by the tool:\n{printed}"
    );
    let out = std::process::Command::new("avifgainmaputil")
        .arg("tonemap")
        .arg(&in_path)
        .arg(&out_path)
        .args([
            "--headroom",
            "3",
            "-q",
            "100",
            "-y",
            "444",
            "-d",
            "10",
            "--cicp-output",
            "9/16/0",
        ])
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "tone-map failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let applied = std::fs::read(&out_path).expect("read");
    let _ = std::fs::remove_dir_all(&tmp);

    let (dt, tool) = decode(&applied, false);
    assert_eq!(dt.output_format(), Some(PixelFormat::Gbrp10Le));
    let tool_planes = samples(&tool, 32);
    let tool_rgb = gbr_to_rgb(&tool_planes);
    let (_, ours) = decode(&file, true);
    let ours_planes = samples(&ours, 32);
    let alt = Colr::Nclx {
        colour_primaries: 9,
        transfer_characteristics: 16,
        matrix_coefficients: 9,
        full_range: true,
    };
    let ours_rgb = ycbcr_to_rgb(&ours_planes, 32, 24, 10, &alt);
    let diff = max_diff(&ours_rgb, &tool_rgb, 10, 10);
    eprintln!("authored file: max |ours - tool| = {diff} (10-bit codes)");
    assert!(
        diff <= 1,
        "our applied decode differs from the tool's tone-map by {diff} codes (10-bit)"
    );
}
