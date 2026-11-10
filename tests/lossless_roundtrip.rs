//! Exact round trips: every lossless shape the encoder writes decodes
//! back to the source sample for sample — the RGB one-call paths
//! (identity matrix, 4:4:4, full range), RGBA (alpha auxiliary
//! lossless too), 8 / 10 / 12-bit, monochrome, odd extents, grids —
//! and the files state what they hold in the container (`colr` `nclx`,
//! `av1C`, `pixi`, `clap` on the colour item and its auxiliary) and in
//! the AV1 sequence header alike.

use oxideav_avif::meta::{Colr, Property};
use oxideav_avif::{
    AvifImage, AvifPixelFormat, AvifPlane, ColorInfo, EncodeOptions, GridTiling, StillChroma,
    decode, decode_rgb8, decode_rgba8, encode, encode_rgb8, encode_rgba8, find_alpha_item_id, info,
    parse_header,
};
use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{CodecId, CodecOptions, CodecParameters, Frame, PixelFormat, RuntimeContext};

/// Deterministic noise: the worst case for any transform that is not
/// exactly invertible.
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect()
}

/// A picture with structure (ramps, a weave, saturated corners) and
/// noise in the low bits.
fn textured(w: u32, h: u32, channels: usize, seed: u32) -> Vec<u8> {
    let n = noise(w as usize * h as usize * channels, seed);
    let mut out = Vec::with_capacity(n.len());
    for y in 0..h as usize {
        for x in 0..w as usize {
            for c in 0..channels {
                let i = (y * w as usize + x) * channels + c;
                let ramp = (x * 255 / (w as usize).max(1) + y * (c + 1) * 3) & 0xff;
                let v = match (x + y * 3 + c) % 11 {
                    0 => 0,
                    1 => 255,
                    _ => (ramp as u8) ^ (n[i] & 0x0f),
                };
                out.push(v);
            }
        }
    }
    out
}

const SIZES: &[(u32, u32)] = &[
    (1, 1),
    (2, 3),
    (7, 5),
    (8, 8),
    (16, 16),
    (17, 13),
    (33, 65),
    (64, 48),
    (100, 37),
];

/// The option sets that all mean "exact": the explicit switch, quality
/// 100, the default (`base_q_idx = 0`), and the switch winning over a
/// lossy quality, a lossy alpha quantiser and a sub-sampled `chroma`.
fn lossless_option_sets() -> Vec<(&'static str, EncodeOptions)> {
    vec![
        ("lossless", EncodeOptions::default().with_lossless(true)),
        ("quality 100", EncodeOptions::default().with_quality(100)),
        ("default", EncodeOptions::default()),
        (
            "lossless over quality 30 / alpha 120 / 4:2:0",
            EncodeOptions::default()
                .with_quality(30)
                .with_alpha_q_idx(120)
                .with_chroma(StillChroma::Yuv420)
                .with_lossless(true),
        ),
        (
            "lossless set before quality 30",
            EncodeOptions::default()
                .with_lossless(true)
                .with_quality(30),
        ),
    ]
}

#[test]
fn rgb_and_rgba_one_call_paths_round_trip_exactly() {
    let mut cells = 0;
    for &(w, h) in SIZES {
        let n = w as usize * h as usize;
        for (seed, source) in [noise(n * 3, w * 31 + h), textured(w, h, 3, w + h * 17)]
            .into_iter()
            .enumerate()
        {
            for (name, opts) in lossless_option_sets() {
                let file = encode_rgb8(w, h, &source, &opts).unwrap();
                let back = decode_rgb8(&file).unwrap();
                assert_eq!((back.width, back.height), (w, h), "{w}x{h} {name}");
                assert!(
                    back.as_bytes() == source.as_slice(),
                    "RGB {w}x{h} source {seed} under '{name}' is not exact"
                );
                let i = info(&file).unwrap();
                assert_eq!(i.color.matrix, 0, "{w}x{h} {name}: identity matrix");
                assert!(i.color.range.is_full());
                assert_eq!(i.format, AvifPixelFormat::Yuv444P);
                assert!(!i.has_alpha);
                cells += 1;
            }
        }
        for (seed, source) in [noise(n * 4, w * 7 + h * 3), textured(w, h, 4, w * 5 + h)]
            .into_iter()
            .enumerate()
        {
            for (name, opts) in lossless_option_sets() {
                let file = encode_rgba8(w, h, &source, &opts).unwrap();
                let back = decode_rgba8(&file).unwrap();
                assert_eq!((back.width, back.height), (w, h), "{w}x{h} {name}");
                assert!(
                    back.as_bytes() == source.as_slice(),
                    "RGBA {w}x{h} source {seed} under '{name}' is not exact"
                );
                let i = info(&file).unwrap();
                assert_eq!(i.color.matrix, 0);
                assert_eq!(i.format, AvifPixelFormat::Yuva444P);
                assert!(i.has_alpha);
                cells += 1;
            }
        }
    }
    assert_eq!(cells, SIZES.len() * 2 * 2 * 5);
}

/// Planes of `depth`-bit samples (LE words above 8 bits) for `format`,
/// noise in every plane.
fn noise_image(w: u32, h: u32, format: AvifPixelFormat, depth: u8, seed: u32) -> AvifImage {
    let (sx, sy) = format.chroma_subsampling();
    let packed_ya = format.is_packed_ya();
    let colour_planes = if format.plane_count() == 1 || packed_ya {
        1
    } else {
        3
    };
    let mut planes = Vec::new();
    for p in 0..format.plane_count() {
        let chroma = (1..colour_planes).contains(&p);
        let (pw, ph) = if chroma {
            (
                (w as usize).div_ceil(1 << sx),
                (h as usize).div_ceil(1 << sy),
            )
        } else {
            (w as usize, h as usize)
        };
        let per_px = if packed_ya { 2 } else { 1 };
        let raw = noise(pw * ph * per_px * 2, seed + p as u32 * 101);
        let bps = format.bytes_per_sample();
        let mut data = Vec::with_capacity(pw * ph * per_px * bps);
        for s in raw.as_chunks::<2>().0 {
            let v = u16::from_le_bytes(*s) & ((1u16 << depth) - 1);
            if bps == 1 {
                data.push(v as u8);
            } else {
                data.extend_from_slice(&v.to_le_bytes());
            }
        }
        planes.push(AvifPlane::new(pw * per_px * bps, data));
    }
    AvifImage::new(w, h, format, planes)
        .unwrap()
        .with_bit_depth(depth)
}

#[test]
fn every_depth_and_layout_round_trips_exactly_under_lossless() {
    use AvifPixelFormat as F;
    // (layout, depth, identity-matrix RGB planes)
    let layouts: &[(F, u8, bool)] = &[
        (F::Yuv444P, 8, true),
        (F::Yuva444P, 8, true),
        (F::Yuv444P10Le, 10, true),
        (F::Yuva444P10Le, 10, true),
        (F::Yuv444P12Le, 12, true),
        (F::Yuva444P12Le, 12, true),
        (F::Gray8, 8, false),
        (F::Gray10Le, 10, false),
        (F::Gray12Le, 12, false),
        (F::Ya8, 8, false),
        (F::Ya16Le, 10, false),
        (F::Ya16Le, 12, false),
        (F::Yuv444P, 8, false),
        (F::Yuv420P, 8, false),
        (F::Yuva420P, 8, false),
        (F::Yuv422P10Le, 10, false),
        (F::Yuv420P12Le, 12, false),
    ];
    // The switch must win over a lossy colour quality and a lossy
    // alpha quantiser.
    let opts = EncodeOptions::default()
        .with_quality(20)
        .with_alpha_q_idx(90)
        .with_lossless(true);
    let mut cells = 0;
    for &(format, depth, identity) in layouts {
        let (sx, sy) = format.chroma_subsampling();
        for &(w, h) in &[
            (1u32, 1u32),
            (2, 1),
            (1, 3),
            (8, 8),
            (17, 13),
            (40, 24),
            (33, 65),
        ] {
            if (sx == 1 && w % 2 == 1) || (sy == 1 && h % 2 == 1) {
                continue; // sub-sampled chroma is coded at even extents only
            }
            let mut img = noise_image(w, h, format, depth, w * 13 + h + u32::from(depth));
            if identity {
                img = img.with_color(ColorInfo::identity_full_range());
            }
            let file = encode(&img, &opts).unwrap();
            let back = decode(&file).unwrap();
            assert_eq!(back.format, format, "{format:?} {depth}-bit {w}x{h}");
            assert_eq!(back.bit_depth, depth);
            assert_eq!(back.color, img.color, "{format:?} {w}x{h}: colour");
            for (p, (a, b)) in back.planes.iter().zip(&img.planes).enumerate() {
                assert_eq!(a.stride, b.stride, "{format:?} {w}x{h} plane {p}");
                assert!(
                    a.data == b.data,
                    "{format:?} {depth}-bit {w}x{h}: plane {p} is not exact"
                );
            }
            cells += 1;
        }
    }
    assert_eq!(cells, 100, "the layout matrix");
}

/// The Sequence Header OBU of an AV1 Image Item Data payload.
fn sequence_header(payload: &[u8]) -> oxideav_av1::SequenceHeader {
    let mut off = 0;
    while off < payload.len() {
        let (obu, used) = oxideav_av1::parse_obu(&payload[off..]).unwrap();
        if obu.obu_type == oxideav_av1::ObuType::SequenceHeader {
            return oxideav_av1::parse_sequence_header(obu.payload).unwrap();
        }
        off += used;
    }
    panic!("no Sequence Header OBU");
}

fn nclx_of(props: &[&Property]) -> Option<(u16, u16, u16, bool)> {
    props.iter().find_map(|p| match p {
        Property::Colr(Colr::Nclx {
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range,
        }) => Some((
            *colour_primaries,
            *transfer_characteristics,
            *matrix_coefficients,
            *full_range,
        )),
        _ => None,
    })
}

fn pixi_of(props: &[&Property]) -> Option<Vec<u8>> {
    props.iter().find_map(|p| match p {
        Property::Pixi(pixi) => Some(pixi.bits_per_channel.clone()),
        _ => None,
    })
}

fn has_clap(props: &[&Property]) -> bool {
    props.iter().any(|p| matches!(p, Property::Clap(_)))
}

/// What a lossless RGB(A) file must say about itself: `colr` `nclx`
/// 1 / 13 / 0 full range and `pixi` 8, 8, 8 on the colour item, `av1C`
/// 4:4:4 non-monochrome mirroring a sequence header that carries the
/// same colour description (H.273 identity, full range) and the
/// picture's own extents (odd ones included: no padding, no `clap`),
/// next to an alpha auxiliary whose stream is monochrome, full range.
#[test]
fn lossless_rgb_is_signalled_in_the_container_and_the_sequence_header() {
    for &(w, h) in &[(16u32, 16u32), (17, 13)] {
        let rgba = textured(w, h, 4, 5);
        let file =
            encode_rgba8(w, h, &rgba, &EncodeOptions::default().with_lossless(true)).unwrap();
        let hdr = parse_header(&file).unwrap();
        let primary = hdr.meta.primary_item_id.expect("pitm");
        let props = hdr.meta.properties_for(primary);
        assert_eq!(nclx_of(&props), Some((1, 13, 0, true)), "{w}x{h}");
        assert_eq!(pixi_of(&props), Some(vec![8, 8, 8]));
        let av1c = props
            .iter()
            .find_map(|p| match p {
                Property::Av1C(raw) => Some(raw.clone()),
                _ => None,
            })
            .expect("av1C");
        // high_bitdepth 0, twelve_bit 0, monochrome 0, subsampling 0 / 0.
        assert_eq!(av1c[2] & 0x7c, 0, "{w}x{h}: av1C layout bits");
        let seq = sequence_header(&hdr.item_data(primary).unwrap());
        let cc = &seq.color_config;
        assert!(seq.still_picture);
        assert_eq!(seq.seq_profile, 1, "4:4:4 8-bit is the High profile");
        assert_eq!(av1c[1] >> 5, seq.seq_profile);
        assert!(cc.color_description_present_flag);
        assert_eq!(
            (
                cc.color_primaries,
                cc.transfer_characteristics,
                cc.matrix_coefficients
            ),
            (1, 13, 0)
        );
        assert!(cc.color_range, "identity RGB is full range");
        assert!(!cc.mono_chrome && !cc.subsampling_x && !cc.subsampling_y);
        assert_eq!(cc.bit_depth, 8);

        let alpha = find_alpha_item_id(&hdr.meta, primary).expect("alpha auxiliary");
        let aprops = hdr.meta.properties_for(alpha);
        assert_eq!(pixi_of(&aprops), Some(vec![8]));
        // Coded at the picture's own extents: `ispe` is the picture,
        // nothing to crop on either item.
        for (what, list) in [("colour", &props), ("alpha", &aprops)] {
            let ispe = list.iter().find_map(|p| match p {
                Property::Ispe(i) => Some((i.width, i.height)),
                _ => None,
            });
            assert_eq!(ispe, Some((w, h)), "{w}x{h}: {what} ispe");
            assert!(!has_clap(list), "{w}x{h}: {what} needs no clap");
        }
        assert_eq!(
            (
                seq.max_frame_width_minus_1 + 1,
                seq.max_frame_height_minus_1 + 1
            ),
            (w, h),
            "{w}x{h}: the coded frame size"
        );
        let aseq = sequence_header(&hdr.item_data(alpha).unwrap());
        assert!(aseq.color_config.mono_chrome);
        assert!(aseq.color_config.color_range, "av1-avif §4.1");
    }
}

/// A lossy quality keeps the Y′CbCr coding (and says so in both
/// places); only the lossless request switches to RGB.
#[test]
fn lossy_qualities_keep_the_ycbcr_coding() {
    let (w, h) = (32u32, 24u32);
    let rgb = textured(w, h, 3, 9);
    for q in [0u8, 50, 90, 99] {
        let file = encode_rgb8(w, h, &rgb, &EncodeOptions::default().with_quality(q)).unwrap();
        let i = info(&file).unwrap();
        assert_eq!(i.color.matrix, 6, "quality {q}");
        assert_eq!(i.format, AvifPixelFormat::Yuv420P, "quality {q}");
        let hdr = parse_header(&file).unwrap();
        let primary = hdr.meta.primary_item_id.unwrap();
        let cc = sequence_header(&hdr.item_data(primary).unwrap()).color_config;
        assert!(cc.color_description_present_flag);
        assert_eq!(
            (
                cc.color_primaries,
                cc.transfer_characteristics,
                cc.matrix_coefficients
            ),
            (1, 13, 6)
        );
        assert!(cc.color_range && cc.subsampling_x && cc.subsampling_y);
    }
}

/// A grid: every tile is an identity-matrix item in its own right
/// (`colr` + `pixi` on the tile, the description in its sequence
/// header), alpha tiles are monochrome, and the canvas is exact.
#[test]
fn lossless_grid_tiles_carry_the_colour_and_the_canvas_is_exact() {
    let (w, h) = (201u32, 150u32);
    let rgba = textured(w, h, 4, 77);
    let opts = EncodeOptions::default()
        .with_lossless(true)
        .with_grid(GridTiling::MaxTile(128));
    let file = encode_rgba8(w, h, &rgba, &opts).unwrap();
    assert!(decode_rgba8(&file).unwrap().as_bytes() == rgba.as_slice());

    let hdr = parse_header(&file).unwrap();
    let grid = hdr.meta.primary_item_id.unwrap();
    assert_eq!(&hdr.meta.item_by_id(grid).unwrap().item_type, b"grid");
    assert_eq!(
        nclx_of(&hdr.meta.properties_for(grid)),
        Some((1, 13, 0, true))
    );
    let tiles = hdr.meta.iref_targets(b"dimg", grid);
    assert_eq!(tiles.len(), 4, "2 x 2 tiles");
    for &tile in &tiles {
        let props = hdr.meta.properties_for(tile);
        assert_eq!(nclx_of(&props), Some((1, 13, 0, true)), "tile {tile}");
        assert_eq!(pixi_of(&props), Some(vec![8, 8, 8]), "tile {tile}");
        let cc = sequence_header(&hdr.item_data(tile).unwrap()).color_config;
        assert!(cc.color_description_present_flag && cc.color_range);
        assert_eq!(cc.matrix_coefficients, 0, "tile {tile}");
    }
    let alpha_grid = find_alpha_item_id(&hdr.meta, grid).expect("alpha grid");
    for &tile in &hdr.meta.iref_targets(b"dimg", alpha_grid) {
        let cc = sequence_header(&hdr.item_data(tile).unwrap()).color_config;
        assert!(cc.mono_chrome && cc.color_range);
    }
}

/// Orientation on an alpha-carrying picture: the auxiliary carries the
/// colour item's `irot` / `imir` / `clap`, and alpha stays co-located
/// with the colour it belongs to (alpha := the red channel here, so
/// any misplaced transform shows as A != R).
#[test]
fn alpha_stays_co_located_through_clap_and_orientation() {
    use oxideav_avif::{StillImage, StillProperties, encode_still};
    let (w, h) = (21u32, 10u32);
    let mut rgba = textured(w, h, 4, 3);
    for px in rgba.as_chunks_mut::<4>().0 {
        px[3] = px[0];
    }
    for (irot, imir) in [
        (None, None),
        (Some(1u8), None),
        (Some(3), Some(0u8)),
        (Some(2), Some(1)),
    ] {
        let still = StillImage::rgba8(w, h, &rgba)
            .unwrap()
            .with_props(StillProperties::default().with_irot(irot).with_imir(imir));
        let file = encode_still(&still, &EncodeOptions::default().with_lossless(true)).unwrap();
        let back = decode_rgba8(&file).unwrap();
        let turned = irot.is_some_and(|t| t % 2 == 1);
        assert_eq!(
            (back.width, back.height),
            if turned { (h, w) } else { (w, h) },
            "irot {irot:?} imir {imir:?}"
        );
        for (i, px) in back.as_bytes().as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(px[3], px[0], "irot {irot:?} imir {imir:?}: pixel {i}");
        }
        if (irot, imir) == (None, None) {
            assert!(back.as_bytes() == rgba.as_slice());
        }
        let mut sorted_in: Vec<[u8; 4]> = rgba.as_chunks::<4>().0.to_vec();
        let mut sorted_out: Vec<[u8; 4]> = back.as_bytes().as_chunks::<4>().0.to_vec();
        sorted_in.sort_unstable();
        sorted_out.sort_unstable();
        assert!(sorted_in == sorted_out, "the same pixels, re-arranged");
    }
}

/// A cropped colour item whose alpha auxiliary shares the crop (the
/// muxer hands the colour item's `clap` to the auxiliary): both items
/// carry it and the decode is the cropped picture, alpha co-located.
#[test]
fn muxer_gives_the_alpha_auxiliary_the_colour_items_clap() {
    use oxideav_avif::meta::Clap;
    use oxideav_avif::{AvifMuxer, StillImage, encode_still};
    let (cw, ch, w, h) = (24u32, 16u32, 17u32, 13u32);
    let full = textured(cw, ch, 4, 41);
    let coded = encode_still(
        &StillImage::rgba8(cw, ch, &full).unwrap(),
        &EncodeOptions::default().with_lossless(true),
    )
    .unwrap();
    let hdr = parse_header(&coded).unwrap();
    let primary = hdr.meta.primary_item_id.unwrap();
    let alpha = find_alpha_item_id(&hdr.meta, primary).unwrap();
    let av1c = |id: u32| {
        hdr.meta
            .properties_for(id)
            .iter()
            .find_map(|p| match p {
                Property::Av1C(raw) => Some(raw.clone()),
                _ => None,
            })
            .unwrap()
    };
    // Top-left anchored crop to 17 x 13 (HEIF 6.5.11 centre geometry).
    let clap = Clap::new(
        w as i32,
        1,
        h as i32,
        1,
        w as i32 - cw as i32,
        2,
        h as i32 - ch as i32,
        2,
    );
    let file = AvifMuxer::new(
        cw,
        ch,
        hdr.item_data(primary).unwrap().into_owned(),
        av1c(primary),
    )
    .with_pixi(vec![8, 8, 8])
    .with_colr(ColorInfo::identity_full_range().to_colr())
    .with_clap(clap)
    .with_alpha(
        hdr.item_data(alpha).unwrap().into_owned(),
        av1c(alpha),
        false,
    )
    .advanced_profile()
    .build()
    .unwrap();
    let cropped = parse_header(&file).unwrap();
    let p = cropped.meta.primary_item_id.unwrap();
    let a = find_alpha_item_id(&cropped.meta, p).unwrap();
    assert!(has_clap(&cropped.meta.properties_for(p)));
    assert!(has_clap(&cropped.meta.properties_for(a)));
    let back = decode_rgba8(&file).unwrap();
    assert_eq!((back.width, back.height), (w, h));
    let mut want = Vec::new();
    for row in 0..h as usize {
        let at = row * cw as usize * 4;
        want.extend_from_slice(&full[at..at + w as usize * 4]);
    }
    assert!(back.as_bytes() == want.as_slice());
}

/// A file whose alpha auxiliary carries no `clap` next to a cropped
/// colour item (the shape this crate wrote before the auxiliary got
/// the colour item's transforms): the planes are co-located sample for
/// sample, so the crop applies to both.
#[test]
fn alpha_auxiliary_without_its_own_clap_is_cropped_with_the_colour() {
    let file = include_bytes!("fixtures/alpha_aux_without_clap.avif");
    let hdr = parse_header(file).unwrap();
    let primary = hdr.meta.primary_item_id.unwrap();
    let alpha = find_alpha_item_id(&hdr.meta, primary).unwrap();
    assert!(has_clap(&hdr.meta.properties_for(primary)));
    assert!(!has_clap(&hdr.meta.properties_for(alpha)));
    let back = decode_rgba8(file).unwrap();
    assert_eq!((back.width, back.height), (17, 13));
    assert!(back.as_bytes() == noise(17 * 13 * 4, 2).as_slice());
}

fn registry() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_avif::register(&mut ctx);
    ctx
}

fn registry_encode(
    fmt: PixelFormat,
    w: u32,
    h: u32,
    planes: Vec<VideoPlane>,
    options: &[(&str, &str)],
) -> Vec<u8> {
    let ctx = registry();
    let mut p = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = Some(fmt);
    let mut o = CodecOptions::new();
    for (k, v) in options {
        o = o.set(*k, *v);
    }
    p.options = o;
    let mut enc = ctx.codecs.first_encoder(&p).unwrap();
    enc.send_frame(&Frame::Video(VideoFrame {
        pts: Some(0),
        planes,
    }))
    .unwrap();
    enc.receive_packet().unwrap().data
}

/// The registry's typed `lossless` option: exact for packed and planar
/// RGB(A) at 8 / 10 / 12 bits and for gray + alpha, and it wins over
/// `quality`, `q`, `alpha_q` and `chroma`.
#[test]
fn registry_lossless_option_is_exact_and_wins_over_the_quality_options() {
    let lossy_and_lossless: &[(&str, &str)] = &[
        ("quality", "10"),
        ("q", "200"),
        ("alpha_q", "150"),
        ("chroma", "420"),
        ("lossless", "true"),
    ];
    for &(w, h) in &[(16u32, 16u32), (19, 11)] {
        let n = w as usize * h as usize;
        // Packed RGBA / BGRA / RGB.
        let rgba = textured(w, h, 4, w + h);
        let plane = |data: Vec<u8>, ch: usize| {
            vec![VideoPlane {
                stride: w as usize * ch,
                data,
            }]
        };
        let file = registry_encode(
            PixelFormat::Rgba,
            w,
            h,
            plane(rgba.clone(), 4),
            lossy_and_lossless,
        );
        assert!(decode_rgba8(&file).unwrap().as_bytes() == rgba.as_slice());
        let bgra: Vec<u8> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        let file = registry_encode(PixelFormat::Bgra, w, h, plane(bgra, 4), lossy_and_lossless);
        assert!(decode_rgba8(&file).unwrap().as_bytes() == rgba.as_slice());
        let rgb = textured(w, h, 3, w * 3 + h);
        let file = registry_encode(
            PixelFormat::Rgb24,
            w,
            h,
            plane(rgb.clone(), 3),
            lossy_and_lossless,
        );
        assert!(decode_rgb8(&file).unwrap().as_bytes() == rgb.as_slice());

        // Planar G, B, R, A at 10 and 12 bits.
        for (fmt, depth) in [(PixelFormat::Gbrap10Le, 10u8), (PixelFormat::Gbrap12Le, 12)] {
            let planes: Vec<Vec<u8>> = (0..4)
                .map(|p| {
                    noise(n * 2, p + u32::from(depth))
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .flat_map(|s| (u16::from_le_bytes(*s) & ((1 << depth) - 1)).to_le_bytes())
                        .collect()
                })
                .collect();
            let file = registry_encode(
                fmt,
                w,
                h,
                planes
                    .iter()
                    .map(|d| VideoPlane {
                        stride: w as usize * 2,
                        data: d.clone(),
                    })
                    .collect(),
                lossy_and_lossless,
            );
            let back = decode(&file).unwrap();
            assert_eq!(back.bit_depth, depth);
            assert!(back.color.is_identity_matrix());
            for (p, plane) in back.planes.iter().enumerate() {
                assert!(plane.data == planes[p], "{fmt:?} {w}x{h} plane {p}");
            }
        }

        // Gray + alpha.
        let ya = noise(n * 2, 91);
        let file = registry_encode(
            PixelFormat::Ya8,
            w,
            h,
            plane(ya.clone(), 2),
            lossy_and_lossless,
        );
        let back = decode(&file).unwrap();
        assert_eq!(back.format, AvifPixelFormat::Ya8);
        assert!(back.planes[0].data == ya);
    }

    // Without the switch the same options are a lossy 4:2:0 file.
    let rgb = textured(16, 16, 3, 1);
    let lossy = registry_encode(
        PixelFormat::Rgb24,
        16,
        16,
        vec![VideoPlane {
            stride: 48,
            data: rgb.clone(),
        }],
        &[("quality", "10")],
    );
    assert_eq!(info(&lossy).unwrap().format, AvifPixelFormat::Yuv420P);
    assert!(decode_rgb8(&lossy).unwrap().as_bytes() != rgb.as_slice());
}
