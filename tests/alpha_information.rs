//! The draft alpha information property (`alpx`) on the AVIF side:
//! av1-avif §9.1.2 lets boxes outside its table sit in the `meta`
//! hierarchy, so a file may carry one on its alpha auxiliary item (or
//! in an alpha track's sample entry). This crate reads it — the alpha
//! plane comes back in the HEIF §6.9.1 convention (0 transparent …
//! maximum opaque) whatever sample values the property names, and its
//! pre-multiplication modes are reported — and never writes it, so
//! the files here are assembled with the container crate's writer
//! around payloads this crate coded.

#![cfg(feature = "registry")]

use std::time::Duration;

use oxideav_avif::{
    AvifImage, AvifPixelFormat, AvifPlane, EncodeOptions, Frame, alpha_information, decode,
    decode_all, encode, encode_all, find_alpha_item_id, inspect, parse, parse_avis, parse_header,
};
use oxideav_heif::props::{AlphaInformation, AlphaPremultiplication, Ispe, Pixi, Property};
use oxideav_heif::{HeifWriter, SequenceAlphaTrack, SequenceSample, SequenceWriter};

const W: u32 = 12;
const H: u32 = 10;

fn colour_planes() -> Vec<AvifPlane> {
    let n = (W * H) as usize;
    (0..3)
        .map(|p| {
            AvifPlane::new(
                W as usize,
                (0..n)
                    .map(|i| ((i * (7 + p * 4) + p * 50) % 256) as u8)
                    .collect(),
            )
        })
        .collect()
}

/// Alpha samples covering 0..=255 and the values around the bounds
/// the tests name.
fn alpha_samples(seed: usize) -> Vec<u8> {
    (0..(W * H) as usize)
        .map(|i| match (i + seed) % 12 {
            0 => 0,
            1 => 255,
            2 => 16,
            3 => 235,
            4 => 15,
            5 => 236,
            k => ((i * 13 + k * 17 + seed * 5) % 256) as u8,
        })
        .collect()
}

/// The coded payload and `av1C` of one picture coded by this crate.
fn coded(image: &AvifImage) -> (Vec<u8>, Property) {
    let file = encode(image, &EncodeOptions::default()).expect("encode");
    let parsed = parse(&file).expect("parse");
    let av1c = parsed.av1c.clone().expect("av1C");
    (
        parsed.primary_item_data.to_vec(),
        Property::Av1C(oxideav_heif::av1c::Av1Config::parse(&av1c).expect("av1C record")),
    )
}

/// A 4:4:4 colour item plus an alpha auxiliary carrying `info`
/// (`essential` as given) — the muxing by the container crate.
fn file_with(alpha: &[u8], info: Option<AlphaInformation>, essential: bool) -> Vec<u8> {
    let colour =
        AvifImage::new(W, H, AvifPixelFormat::Yuv444P, colour_planes()).expect("colour picture");
    let gray = AvifImage::new(
        W,
        H,
        AvifPixelFormat::Gray8,
        vec![AvifPlane::new(W as usize, alpha.to_vec())],
    )
    .expect("alpha picture");
    let (colour_data, colour_config) = coded(&colour);
    let (alpha_data, alpha_config) = coded(&gray);
    let mut writer = HeifWriter::new().with_brands(*b"avif", vec![*b"avif", *b"mif1", *b"miaf"]);
    let master = writer.add_coded_item(
        *b"av01",
        colour_data,
        vec![
            (colour_config, true),
            (Property::Ispe(Ispe::new(W, H)), false),
            (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
        ],
    );
    writer.set_primary(master);
    let mut props = vec![
        (alpha_config, true),
        (Property::Ispe(Ispe::new(W, H)), false),
        (Property::Pixi(Pixi::new(vec![8])), false),
    ];
    if let Some(info) = info {
        props.push((Property::Alpx(info), essential));
    }
    writer.add_alpha(master, *b"av01", alpha_data, props, false);
    writer.write_to_vec().expect("write")
}

/// What a coded alpha sample stands for under `info`, at 8 bits.
fn expected(sample: u8, opaque: f64, transparent: f64) -> u8 {
    let position = ((f64::from(sample) - transparent) / (opaque - transparent)).clamp(0.0, 1.0);
    (position * 255.0).round() as u8
}

#[test]
fn alpha_item_without_the_property_is_read_as_coded() {
    let alpha = alpha_samples(0);
    let file = file_with(&alpha, None, false);
    let img = decode(&file).expect("decode");
    assert_eq!(img.format, AvifPixelFormat::Yuva444P);
    assert_eq!(img.planes[3].data, alpha);
    assert_eq!(img.planes[..3], colour_planes()[..]);
    let hdr = parse_header(&file).expect("header");
    let alpha_id = find_alpha_item_id(&hdr.meta, hdr.meta.primary_item_id.unwrap()).unwrap();
    assert_eq!(alpha_information(&hdr.meta, alpha_id), None);
    assert!(!inspect(&file).unwrap().premultiplied_alpha);
}

#[test]
fn alpha_item_values_are_normalised_to_the_opacity_convention() {
    let alpha = alpha_samples(3);
    for (opaque, transparent) in [(235u16, 16u16), (0, 255), (255, 0), (128, 127), (200, 40)] {
        for essential in [false, true] {
            let info =
                AlphaInformation::integer(AlphaPremultiplication::Straight, opaque, transparent);
            let file = file_with(&alpha, Some(info), essential);
            let label = format!("opaque {opaque} transparent {transparent} essential {essential}");
            let hdr = parse_header(&file).expect("header");
            let alpha_id =
                find_alpha_item_id(&hdr.meta, hdr.meta.primary_item_id.unwrap()).unwrap();
            assert_eq!(
                alpha_information(&hdr.meta, alpha_id),
                Some(info),
                "{label}"
            );
            assert!(
                !hdr.meta.has_unsupported_essential_property(alpha_id),
                "{label}: the property is understood"
            );
            let img = decode(&file).unwrap_or_else(|e| panic!("{label}: {e}"));
            let want: Vec<u8> = alpha
                .iter()
                .map(|&s| expected(s, f64::from(opaque), f64::from(transparent)))
                .collect();
            assert_eq!(img.planes[3].data, want, "{label}");
            // The colour planes are untouched.
            assert_eq!(img.planes[..3], colour_planes()[..], "{label}");
            if (opaque, transparent) == (255, 0) {
                assert_eq!(want, alpha, "the default values change nothing");
            }
            assert!(!inspect(&file).unwrap().premultiplied_alpha, "{label}");
        }
    }
    // Floating-point values are compared with the samples as numbers.
    let info = AlphaInformation::float(AlphaPremultiplication::Straight, 200.0, 50.0);
    let img = decode(&file_with(&alpha, Some(info), false)).expect("decode");
    let want: Vec<u8> = alpha.iter().map(|&s| expected(s, 200.0, 50.0)).collect();
    assert_eq!(img.planes[3].data, want);
}

#[test]
fn premultiplication_modes_are_reported() {
    let alpha = alpha_samples(5);
    for (mode, premultiplied) in [
        (AlphaPremultiplication::Straight, false),
        (AlphaPremultiplication::NonLinear, true),
        (AlphaPremultiplication::Linear, true),
    ] {
        let info = AlphaInformation::integer(mode, 255, 0);
        let file = file_with(&alpha, Some(info), false);
        assert_eq!(
            inspect(&file).unwrap().premultiplied_alpha,
            premultiplied,
            "{mode:?}"
        );
        // The samples themselves are returned as stored.
        assert_eq!(decode(&file).unwrap().planes[3].data, alpha, "{mode:?}");
    }
}

/// An alpha auxiliary track whose sample entry carries the box: every
/// frame's alpha is normalised the same way.
#[test]
fn alpha_track_sample_entry_values_are_normalised() {
    // Colour + alpha sequence coded by this crate, then re-assembled
    // with the box in the alpha sample entry.
    let frames: Vec<Frame> = (0..3)
        .map(|t| {
            let mut planes = colour_planes();
            planes.push(AvifPlane::new(W as usize, alpha_samples(t)));
            Frame::new(
                AvifImage::new(W, H, AvifPixelFormat::Yuva444P, planes).unwrap(),
                Some(Duration::from_millis(50)),
            )
        })
        .collect();
    let plain = encode_all(&frames, &EncodeOptions::default()).expect("encode_all");
    let back = decode_all(&plain).expect("decode_all");
    for (t, f) in back.iter().enumerate() {
        assert_eq!(f.image.planes[3].data, alpha_samples(t), "plain frame {t}");
    }

    let avis = parse_avis(&plain).expect("parse_avis");
    let colour_config = Property::Av1C(
        oxideav_heif::av1c::Av1Config::parse(avis.av1_codec_config.as_ref().unwrap()).unwrap(),
    );
    let heif = oxideav_heif::HeifFile::parse_borrowed(&plain).expect("container");
    let movie = oxideav_heif::sequence::parse_movie(&heif)
        .expect("movie")
        .expect("moov");
    let alpha_track = movie
        .tracks
        .iter()
        .find(|t| t.handler == *b"auxv")
        .expect("alpha track");
    let alpha_entry = &alpha_track.sample_entries[0];
    let alpha_config = Property::Av1C(alpha_entry.av1c.clone().expect("alpha av1C"));
    let bytes_of = |offset: u64, size: u32| -> Vec<u8> {
        plain[offset as usize..offset as usize + size as usize].to_vec()
    };
    for (opaque, transparent) in [(235u16, 16u16), (0, 255)] {
        let info = AlphaInformation::integer(AlphaPremultiplication::Straight, opaque, transparent);
        let mut writer =
            SequenceWriter::new(*b"av01", colour_config.clone(), W as u16, H as u16, 1000)
                .with_brands(*b"avis", vec![*b"avis", *b"msf1", *b"miaf", *b"av01"]);
        for s in &avis.samples {
            writer.push_sample(bytes_of(s.offset, s.size), s.duration, s.is_sync);
        }
        let samples: Vec<SequenceSample> = alpha_track
            .samples
            .iter()
            .map(|s| SequenceSample::new(bytes_of(s.offset, s.size), s.duration, s.is_sync))
            .collect();
        writer.alpha = Some(SequenceAlphaTrack::new(
            *b"av01",
            alpha_config.clone(),
            W as u16,
            H as u16,
            oxideav_avif::ALPHA_URN_PREFIX.to_string(),
            samples,
            vec![Property::Alpx(info)],
        ));
        let file = writer.write_to_vec().expect("write");
        let frames = decode_all(&file).expect("decode_all");
        assert_eq!(frames.len(), 3);
        for (t, f) in frames.iter().enumerate() {
            let want: Vec<u8> = alpha_samples(t)
                .iter()
                .map(|&s| expected(s, f64::from(opaque), f64::from(transparent)))
                .collect();
            assert_eq!(
                f.image.planes[3].data, want,
                "frame {t} ({opaque}, {transparent})"
            );
            assert_eq!(f.image.planes[..3], colour_planes()[..], "frame {t}");
        }
    }
}
