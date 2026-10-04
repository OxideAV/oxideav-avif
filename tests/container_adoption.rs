//! Round-464 adoption of the published `oxideav-heif` 0.0.5 surface:
//! the HEIF Amd 1:2025 / Amd 2:2026 property types the container now
//! types must never make this crate refuse a file — they ride as
//! [`Property::Container`] (or, for the Amd 2 per-channel `pixi`, on
//! [`Property::Pixi`] with its channel descriptors) and the AVIF
//! decode of the item is unchanged.

// Registry-only: the codec path and the framework traits need the
// default `registry` feature; the standalone build skips this file.
#![cfg(feature = "registry")]

use oxideav_avif::meta::Property;
use oxideav_avif::{inspect, parse, parse_header, AvifDecoder};
use oxideav_core::{CodecId, CodecParameters, Decoder, Frame, Packet, TimeBase};
use oxideav_heif::boxes::FourCc;
use oxideav_heif::props::{self as hprops, Property as HProp};
use oxideav_heif::{Av1Config, HeifWriter};

const RED: &[u8] = include_bytes!("fixtures/red.avif");

fn fourcc(s: &[u8; 4]) -> FourCc {
    *s
}

/// The `red.avif` fixture's primary payload + `av1C` + `ispe`, as the
/// raw material for a container-authored file.
fn red_primary() -> (Vec<u8>, Av1Config, u32, u32) {
    let img = parse(RED).expect("parse red.avif");
    let av1c = Av1Config::parse(img.av1c.as_deref().expect("av1C")).expect("av1C record");
    let ispe = img.ispe.expect("ispe");
    (
        img.primary_item_data.to_vec(),
        av1c,
        ispe.width,
        ispe.height,
    )
}

/// Every HEIF Amd 1 / Amd 2 typed property the container knows,
/// attached (non-essential) to a coded AV1 item by the container's
/// writer.
fn amendment_properties() -> Vec<(HProp, bool)> {
    vec![
        (
            HProp::Reve(hprops::Reve::new(
                2_000_000, 3127, 3290, 500_000, 3127, 3290,
            )),
            false,
        ),
        (HProp::Ndwt(hprops::Ndwt::new(2_030_000)), false),
        (
            HProp::Cexg(hprops::Cexg::new(1, 1, 16, 16, false, None)),
            false,
        ),
        (HProp::Dadj(hprops::Dadj::new(-250)), false),
        (
            HProp::Stag(hprops::Stag::new(vec![hprops::StereoAggressor::new(
                3, 50, None,
            )])),
            false,
        ),
        (
            HProp::TilC(hprops::TilC::new(
                16,
                16,
                Vec::new(),
                Some((fourcc(b"av01"), Vec::new())),
            )),
            false,
        ),
    ]
}

/// A container-authored AVIF whose primary carries the Amd 2
/// per-channel `pixi` plus every Amd 1 / Amd 2 property type.
fn amendment_file() -> Vec<u8> {
    let (payload, av1c, w, h) = red_primary();
    let mut props = vec![
        (HProp::Av1C(av1c), true),
        (HProp::Ispe(hprops::Ispe::new(w, h)), false),
        (
            HProp::PixiExtended(hprops::PixiExtended::new(
                hprops::Pixi::new(vec![8, 8, 8]),
                vec![
                    hprops::PixiChannel::new(2, 0, None, Some("Y".to_owned())),
                    hprops::PixiChannel::new(3, 0, Some((1, 0)), None),
                    hprops::PixiChannel::new(4, 0, Some((1, 0)), None),
                ],
            )),
            false,
        ),
    ];
    props.extend(amendment_properties());
    let mut w = HeifWriter::new().with_brands(
        fourcc(b"avif"),
        vec![fourcc(b"avif"), fourcc(b"mif1"), fourcc(b"miaf")],
    );
    let id = w.add_coded_item(fourcc(b"av01"), payload, props);
    w.set_primary(id);
    w.write_to_vec().expect("container writer")
}

fn decode_own(file: &[u8]) -> oxideav_core::frame::VideoFrame {
    let mut d = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), file.to_vec());
    d.send_packet(&pkt).expect("send_packet");
    match d.receive_frame() {
        Ok(Frame::Video(v)) => v,
        other => panic!("expected a video frame, got {other:?}"),
    }
}

/// Item 1 of the round brief: every property the container types
/// beyond this crate's model parses as an opaque typed value — no
/// refusal — and the Amd 2 `pixi` keeps both its depths and its
/// per-channel descriptors.
#[test]
fn amendment_properties_parse_as_container_typed_not_errors() {
    let file = amendment_file();
    let hdr = parse_header(&file).expect("parse_header must not refuse Amd 1 / Amd 2 properties");
    let primary = hdr.meta.primary_item_id.expect("pitm");
    let props: Vec<&Property> = hdr.meta.properties_for(primary);
    let kinds: Vec<String> = props
        .iter()
        .map(|p| String::from_utf8_lossy(&p.kind()).into_owned())
        .collect();
    for tag in ["reve", "ndwt", "cexg", "dadj", "stag", "tilC"] {
        assert!(
            kinds.iter().any(|k| k == tag),
            "{tag} missing from {kinds:?}"
        );
    }
    let mut seen = 0;
    for p in &props {
        match p {
            Property::Container(HProp::Reve(r)) => {
                assert_eq!(r.surround_luminance, 2_000_000);
                seen += 1;
            }
            Property::Container(HProp::Ndwt(n)) => {
                assert_eq!(n.diffuse_white_luminance, 2_030_000);
                seen += 1;
            }
            Property::Container(HProp::Cexg(c)) => {
                assert_eq!((c.rows, c.columns), (1, 1));
                seen += 1;
            }
            Property::Container(HProp::Dadj(d)) => {
                assert_eq!(d.disparity_adjustment, -250);
                seen += 1;
            }
            Property::Container(HProp::Stag(s)) => {
                assert_eq!(s.aggressors.len(), 1);
                assert_eq!(s.aggressors[0].aggressor_type, 3);
                seen += 1;
            }
            Property::Container(HProp::TilC(t)) => {
                assert_eq!((t.tile_width, t.tile_height), (16, 16));
                seen += 1;
            }
            Property::Container(other) => panic!("unexpected container property {other:?}"),
            Property::Other(t, _) => panic!(
                "'{}' fell to Property::Other — the container types it",
                String::from_utf8_lossy(t)
            ),
            _ => {}
        }
    }
    assert_eq!(seen, 6, "every Amd 1 / Amd 2 property typed");
    // The Amd 2 per-channel pixi: depths on the AVIF model, channels
    // alongside.
    let pixi = props
        .iter()
        .find_map(|p| match p {
            Property::Pixi(p) => Some(p.clone()),
            _ => None,
        })
        .expect("pixi typed on Property::Pixi");
    assert_eq!(pixi.bits_per_channel, vec![8, 8, 8]);
    assert_eq!(pixi.channels.len(), 3);
    assert_eq!(pixi.channels[0].channel_idc, 2);
    assert_eq!(pixi.channels[0].label.as_deref(), Some("Y"));
    assert_eq!(pixi.channels[1].subsampling, Some((1, 0)));
    // None of them is essential, so nothing is reported as unsupported.
    assert!(hdr
        .meta
        .unsupported_essential_properties(primary)
        .is_empty());
}

/// `inspect` / `parse` and the pixel decode of the same file are
/// untouched by the extra properties: the fixture's constant red
/// planes come back.
#[test]
fn amendment_properties_leave_inspect_and_decode_unchanged() {
    let file = amendment_file();
    let info = inspect(&file).expect("inspect");
    assert_eq!(info.bits_per_channel, vec![8, 8, 8]);
    let mut reference = decode_own(RED);
    let mut frame = decode_own(&file);
    // The container-authored file carries no `colr` (the fixture's
    // is an identity triple), so only the pixel planes are compared.
    reference.take_color_signal();
    frame.take_color_signal();
    assert_eq!(frame.image_plane_count(), reference.image_plane_count());
    for (a, b) in frame.image_planes().iter().zip(reference.image_planes()) {
        assert_eq!(a.stride, b.stride);
        assert_eq!(a.data, b.data);
    }
}

/// An essential Amd 1 / Amd 2 property this crate does not act on is
/// reported through the essential-property audit (av1-avif
/// §2.3.2.1.2 / MIAF §7.3.5) rather than refused at parse time.
#[test]
fn essential_container_property_is_reported_not_refused() {
    let (payload, av1c, w, h) = red_primary();
    let props = vec![
        (HProp::Av1C(av1c), true),
        (HProp::Ispe(hprops::Ispe::new(w, h)), false),
        (HProp::Reve(hprops::Reve::new(1, 2, 3, 4, 5, 6)), true),
    ];
    let mut wr = HeifWriter::new().with_brands(
        fourcc(b"avif"),
        vec![fourcc(b"avif"), fourcc(b"mif1"), fourcc(b"miaf")],
    );
    let id = wr.add_coded_item(fourcc(b"av01"), payload, props);
    wr.set_primary(id);
    let file = wr.write_to_vec().expect("container writer");
    let hdr = parse_header(&file).expect("parse_header");
    assert_eq!(
        hdr.meta.unsupported_essential_properties(id),
        vec![*b"reve"]
    );
}

/// A malformed body of one of the opaque container-typed properties
/// stays a raw [`Property::Other`] (its `ipco` index must survive for
/// the associations around it) and never fails the parse.
#[test]
fn malformed_opaque_property_stays_raw() {
    let file = amendment_file();
    // Truncate the `ndwt` body: find the box and shrink its payload to
    // one byte by rewriting the size and splicing the bytes out.
    let pos = file
        .windows(4)
        .position(|w| w == b"ndwt")
        .expect("ndwt box present");
    let size_at = pos - 4;
    let size = u32::from_be_bytes(file[size_at..pos].try_into().unwrap()) as usize;
    assert!(size > 9, "ndwt box has a body");
    let mut out = Vec::with_capacity(file.len());
    out.extend_from_slice(&file[..size_at]);
    out.extend_from_slice(&9u32.to_be_bytes());
    out.extend_from_slice(b"ndwt");
    out.push(0);
    // Drop the rest of the original box body; every later file offset
    // in `iloc` shifts, so only the `meta` walk is exercised here.
    out.extend_from_slice(&file[size_at + size..]);
    // The enclosing boxes (`ipco` / `iprp` / `meta`) still declare the
    // old sizes; rewrite them by walking the nesting from the top.
    let removed = (size - 9) as u32;
    for parent in [b"meta", b"iprp", b"ipco"] {
        let p = out
            .windows(4)
            .position(|w| w == parent)
            .expect("parent box");
        let at = p - 4;
        let old = u32::from_be_bytes(out[at..p].try_into().unwrap());
        out[at..p].copy_from_slice(&(old - removed).to_be_bytes());
    }
    let hdr = parse_header(&out).expect("parse_header survives a malformed opaque property");
    let primary = hdr.meta.primary_item_id.unwrap();
    let raw = hdr
        .meta
        .properties_for(primary)
        .into_iter()
        .find(|p| &p.kind() == b"ndwt")
        .expect("ndwt still associated");
    assert!(matches!(raw, Property::Other(_, _)), "got {raw:?}");
}

// ---------------------------------------------------------------------
// Item 2: zero-copy header, colour signal on frames, identity-matrix
// items as planar RGB.
// ---------------------------------------------------------------------

use oxideav_core::{
    ColorPrimaries, ColorRange, ColorSignal, MatrixCoefficients, PixelFormat,
    TransferCharacteristics,
};
use oxideav_heif::HeifFileRef;

/// `heif-enc -A -L -p chroma=444` of a 24×16 red→blue gradient: an
/// identity-matrix (`nclx` 1 / 13 / 0, full range) lossless AV1 item.
const IDENTITY_RGB: &[u8] = include_bytes!("fixtures/identity_rgb_lossless.avif");
/// The gradient's interleaved RGB bytes (24 × 16 × 3).
const IDENTITY_RGB_REF: &[u8] = include_bytes!("fixtures/identity_rgb_lossless.rgb");
/// A black-box encoder's 20×12 10-bit 4:2:0 still signalled `nclx`
/// 9 / 16 / 9 with `full_range_flag = 1`.
const YUV420_10BIT_FULL: &[u8] = include_bytes!("fixtures/yuv420_10bit_full.avif");
const MONOCHROME: &[u8] = include_bytes!("fixtures/monochrome.avif");

fn decoder_after(file: &[u8]) -> (AvifDecoder, oxideav_core::frame::VideoFrame) {
    let mut d = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), file.to_vec());
    d.send_packet(&pkt).expect("send_packet");
    let frame = match d.receive_frame() {
        Ok(Frame::Video(v)) => v,
        other => panic!("expected a video frame, got {other:?}"),
    };
    (d, frame)
}

/// The header borrows the caller's bytes — the container view is the
/// zero-copy `HeifFileRef`, and the primary payload of a one-span
/// item is a borrowed slice of the input.
#[test]
fn header_borrows_the_input() {
    let hdr = parse_header(RED).expect("parse_header");
    let view: &HeifFileRef<'_> = &hdr.heif;
    assert!(view.meta.is_some());
    let img = parse(RED).expect("parse");
    assert!(matches!(
        img.primary_item_data,
        std::borrow::Cow::Borrowed(_)
    ));
    let payload: &[u8] = &img.primary_item_data;
    let start = payload.as_ptr() as usize - RED.as_ptr() as usize;
    assert!(
        start + payload.len() <= RED.len(),
        "payload lies inside the input"
    );
}

/// A full-range 10-bit still is announced full range: the frame's
/// colour-signal side channel and the decoder's `color_signal()` carry
/// the item's `nclx` (9 / 16 / 9, full), and the label stays the
/// 10-bit storage layout.
#[test]
fn full_range_ten_bit_still_reads_as_full_range() {
    let (d, frame) = decoder_after(YUV420_10BIT_FULL);
    let expected = ColorSignal::from_code_points(9, 16, 9, true);
    assert_eq!(frame.color_signal(), Some(expected));
    assert_eq!(d.color_signal(), Some(expected));
    assert_eq!(d.output_format(), Some(PixelFormat::Yuv420P10Le));
    assert_eq!(expected.range, ColorRange::Full);
}

/// Without a `colr`, the MIAF §7.3.6.4 default applies: full range,
/// BT.709 primaries, sRGB transfer, BT.601 matrix — never limited.
#[test]
fn absent_colr_is_the_miaf_full_range_default() {
    let (d, frame) = decoder_after(MONOCHROME);
    let sig = frame.color_signal().expect("signal attached");
    assert_eq!(sig.range, ColorRange::Full);
    assert_eq!(sig.primaries, ColorPrimaries(1));
    assert_eq!(sig.transfer, TransferCharacteristics(13));
    assert_eq!(sig.matrix, MatrixCoefficients(6));
    assert_eq!(d.output_format(), Some(PixelFormat::Gray8));
}

/// An identity-matrix lossless item from a black-box encoder decodes
/// as planar RGB: `Gbrp8`, planes G / B / R equal to the source
/// gradient's channels, and the signal says matrix 0 / full range.
#[test]
fn identity_matrix_item_is_planar_rgb() {
    let (d, frame) = decoder_after(IDENTITY_RGB);
    assert_eq!(d.output_format(), Some(PixelFormat::Gbrp8));
    let sig = frame.color_signal().expect("signal attached");
    assert_eq!(sig, ColorSignal::srgb());
    assert_eq!(frame.image_plane_count(), 3, "G, B, R planes");
    let (w, h) = (24usize, 16usize);
    let chan = |c: usize| -> Vec<u8> { IDENTITY_RGB_REF.chunks_exact(3).map(|p| p[c]).collect() };
    let plane = |i: usize| -> Vec<u8> {
        let p = &frame.planes[i];
        (0..h)
            .flat_map(|r| p.data[r * p.stride..r * p.stride + w].to_vec())
            .collect()
    };
    assert_eq!(plane(0), chan(1), "plane 0 is G");
    assert_eq!(plane(1), chan(2), "plane 1 is B");
    assert_eq!(plane(2), chan(0), "plane 2 is R");
}

/// Encoder side: the stream's `color_signal` is written as the item's
/// `colr` and coded into the AV1 range bit; a 10-bit planar input
/// signalled full range round-trips as a full-range 10-bit still.
#[test]
fn encoder_writes_the_signalled_colr_and_range() {
    let (w, h) = (16u32, 8u32);
    let mut params = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
    params.width = Some(w);
    params.height = Some(h);
    params.pixel_format = Some(PixelFormat::Yuv420P10Le);
    params.color_signal = ColorSignal::from_code_points(9, 16, 9, true);
    let mut enc = oxideav_avif::make_encoder(&params).expect("encoder");
    let plane16 = |n: usize, v: u16| -> Vec<u8> { (0..n).flat_map(|_| v.to_le_bytes()).collect() };
    let frame = oxideav_core::frame::VideoFrame {
        pts: Some(0),
        planes: vec![
            oxideav_core::frame::VideoPlane {
                stride: w as usize * 2,
                data: plane16((w * h) as usize, 1000),
            },
            oxideav_core::frame::VideoPlane {
                stride: w as usize,
                data: plane16((w * h / 4) as usize, 512),
            },
            oxideav_core::frame::VideoPlane {
                stride: w as usize,
                data: plane16((w * h / 4) as usize, 512),
            },
        ],
    };
    enc.send_frame(&Frame::Video(frame)).expect("send_frame");
    let pkt = enc.receive_packet().expect("packet");
    assert_eq!(
        enc.output_params().color_signal,
        ColorSignal::from_code_points(9, 16, 9, true)
    );
    let info = inspect(&pkt.data).expect("inspect");
    assert!(
        matches!(
            info.colour,
            Some(oxideav_avif::Colr::Nclx {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 9,
                full_range: true,
            })
        ),
        "{:?}",
        info.colour
    );
    let (d, out) = decoder_after(&pkt.data);
    assert_eq!(d.output_format(), Some(PixelFormat::Yuv420P10Le));
    assert_eq!(
        out.color_signal(),
        Some(ColorSignal::from_code_points(9, 16, 9, true))
    );
    // Lossless: the constant planes come back exactly.
    assert!(out.planes[0]
        .data
        .chunks_exact(2)
        .all(|c| u16::from_le_bytes([c[0], c[1]]) == 1000));
    // A limited-range signal writes full_range_flag = 0.
    params.color_signal = ColorSignal::bt709_limited();
    let mut enc = oxideav_avif::make_encoder(&params).expect("encoder");
    let frame = oxideav_core::frame::VideoFrame {
        pts: Some(0),
        planes: vec![
            oxideav_core::frame::VideoPlane {
                stride: w as usize * 2,
                data: plane16((w * h) as usize, 600),
            },
            oxideav_core::frame::VideoPlane {
                stride: w as usize,
                data: plane16((w * h / 4) as usize, 512),
            },
            oxideav_core::frame::VideoPlane {
                stride: w as usize,
                data: plane16((w * h / 4) as usize, 512),
            },
        ],
    };
    enc.send_frame(&Frame::Video(frame)).expect("send_frame");
    let pkt = enc.receive_packet().expect("packet");
    let (d, out) = decoder_after(&pkt.data);
    assert_eq!(out.color_signal(), Some(ColorSignal::bt709_limited()));
    assert_eq!(d.color_signal().map(|s| s.range), Some(ColorRange::Limited));
}

/// Planar RGB input (`Gbrp8` / `Gbrp10Le`) is coded as an identity
/// 4:4:4 item and comes back as the same planar RGB, sample-exact.
#[test]
fn planar_rgb_input_round_trips_as_planar_rgb() {
    let (w, h) = (12u32, 10u32);
    for (fmt, depth) in [(PixelFormat::Gbrp8, 8u8), (PixelFormat::Gbrp10Le, 10u8)] {
        let mut params = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
        params.width = Some(w);
        params.height = Some(h);
        params.pixel_format = Some(fmt);
        let mut enc = oxideav_avif::make_encoder(&params).expect("encoder");
        let n = (w * h) as usize;
        let max = (1u32 << depth) - 1;
        let sample = |i: usize, k: u32| -> u16 { ((i as u32 * 37 + k * 101) % (max + 1)) as u16 };
        let planes: Vec<oxideav_core::frame::VideoPlane> = (0..3)
            .map(|k| {
                let data: Vec<u8> = if depth == 8 {
                    (0..n).map(|i| sample(i, k) as u8).collect()
                } else {
                    (0..n).flat_map(|i| sample(i, k).to_le_bytes()).collect()
                };
                oxideav_core::frame::VideoPlane {
                    stride: w as usize * if depth == 8 { 1 } else { 2 },
                    data,
                }
            })
            .collect();
        let frame = oxideav_core::frame::VideoFrame {
            pts: Some(0),
            planes: planes.clone(),
        };
        enc.send_frame(&Frame::Video(frame)).expect("send_frame");
        let pkt = enc.receive_packet().expect("packet");
        assert_eq!(enc.output_params().color_signal, ColorSignal::srgb());
        let (d, out) = decoder_after(&pkt.data);
        assert_eq!(d.output_format(), Some(fmt), "{fmt:?}");
        assert_eq!(out.color_signal(), Some(ColorSignal::srgb()));
        for (k, (got, want)) in out.image_planes().iter().zip(&planes).enumerate() {
            assert_eq!(got.stride, want.stride);
            assert_eq!(got.data, want.data, "{fmt:?} plane {k}");
        }
    }
}
