//! Round-464 adoption of the published `oxideav-heif` 0.0.5 surface:
//! the HEIF Amd 1:2025 / Amd 2:2026 property types the container now
//! types must never make this crate refuse a file — they ride as
//! [`Property::Container`] (or, for the Amd 2 per-channel `pixi`, on
//! [`Property::Pixi`] with its channel descriptors) and the AVIF
//! decode of the item is unchanged.

use oxideav_avif::meta::Property;
use oxideav_avif::{inspect, parse, parse_header, AvifDecoder};
use oxideav_core::{CodecId, Decoder, Frame, Packet, TimeBase};
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
            HProp::Reve(hprops::Reve {
                surround_luminance: 2_000_000,
                surround_light_x: 3127,
                surround_light_y: 3290,
                periphery_luminance: 500_000,
                periphery_light_x: 3127,
                periphery_light_y: 3290,
            }),
            false,
        ),
        (
            HProp::Ndwt(hprops::Ndwt {
                diffuse_white_luminance: 2_030_000,
            }),
            false,
        ),
        (
            HProp::Cexg(hprops::Cexg {
                rows: 1,
                columns: 1,
                tile_width: 16,
                tile_height: 16,
                large_fields: false,
                extent_config: None,
            }),
            false,
        ),
        (
            HProp::Dadj(hprops::Dadj {
                disparity_adjustment: -250,
            }),
            false,
        ),
        (
            HProp::Stag(hprops::Stag {
                aggressors: vec![hprops::StereoAggressor {
                    aggressor_type: 3,
                    severity: 50,
                    sub_type_uri: None,
                }],
            }),
            false,
        ),
        (
            HProp::TilC(hprops::TilC {
                tile_width: 16,
                tile_height: 16,
                extra_dimensions: Vec::new(),
                in_file_tiles: Some((fourcc(b"av01"), Vec::new())),
            }),
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
        (
            HProp::Ispe(hprops::Ispe {
                width: w,
                height: h,
            }),
            false,
        ),
        (
            HProp::PixiExtended(hprops::PixiExtended {
                pixi: hprops::Pixi {
                    bits_per_channel: vec![8, 8, 8],
                },
                channels: vec![
                    hprops::PixiChannel {
                        channel_idc: 2,
                        component_format: 0,
                        subsampling: None,
                        label: Some("Y".to_owned()),
                    },
                    hprops::PixiChannel {
                        channel_idc: 3,
                        component_format: 0,
                        subsampling: Some((1, 0)),
                        label: None,
                    },
                    hprops::PixiChannel {
                        channel_idc: 4,
                        component_format: 0,
                        subsampling: Some((1, 0)),
                        label: None,
                    },
                ],
            }),
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
    let reference = decode_own(RED);
    let frame = decode_own(&file);
    assert_eq!(frame.planes.len(), reference.planes.len());
    for (a, b) in frame.planes.iter().zip(&reference.planes) {
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
        (
            HProp::Ispe(hprops::Ispe {
                width: w,
                height: h,
            }),
            false,
        ),
        (
            HProp::Reve(hprops::Reve {
                surround_luminance: 1,
                surround_light_x: 2,
                surround_light_y: 3,
                periphery_luminance: 4,
                periphery_light_x: 5,
                periphery_light_y: 6,
            }),
            true,
        ),
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
