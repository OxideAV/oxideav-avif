//! `avis` image sequences with an alpha auxiliary track: timed RGBA /
//! Y′CbCr + alpha / gray + alpha frames encode to a picture track plus
//! an `auxv` alpha track (HEIF §7.5.3, av1-avif §4.1) and decode back
//! exactly at the lossless setting; loop count, sequence metadata and
//! the worker budget ride the same writer.

use std::time::Duration;

use oxideav_avif::{
    AvifImage, AvifPixelFormat, AvifPlane, ColorInfo, EncodeOptions, Frame, Metadata,
    SequenceEncodeOptions, StillImage, decode_all, encode_all, encode_sequence, parse_avis,
};

fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect()
}

/// A moving structure over noise: frame `t` of an animation.
fn rgba_frame(w: u32, h: u32, t: u32) -> Vec<u8> {
    let n = noise(w as usize * h as usize * 4, 100 + t);
    let mut out = Vec::with_capacity(n.len());
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let inside = (x + t * 2) % 9 < 5 && (y + t) % 7 < 4;
            out.push(if inside { 250 } else { n[i] });
            out.push(n[i + 1] & 0xf0 | (t as u8 & 0x0f));
            out.push((x * 255 / w.max(1)) as u8 ^ (n[i + 2] & 3));
            out.push(if inside { 255 } else { n[i + 3] });
        }
    }
    out
}

fn rgba_frames(w: u32, h: u32, count: u32) -> Vec<Frame> {
    (0..count)
        .map(|t| {
            Frame::new(
                AvifImage::from_rgba8(w, h, rgba_frame(w, h, t)).unwrap(),
                Some(Duration::from_millis(40 + u64::from(t) * 10)),
            )
        })
        .collect()
}

#[test]
fn timed_rgba_frames_round_trip_exactly_through_an_alpha_track() {
    for &(w, h) in &[(16u32, 16u32), (17, 13), (40, 24)] {
        let frames = rgba_frames(w, h, 5);
        let file = encode_all(&frames, &EncodeOptions::default().with_lossless(true)).unwrap();
        let back = decode_all(&file).unwrap();
        assert_eq!(back.len(), frames.len(), "{w}x{h}");
        for (i, (got, want)) in back.iter().zip(&frames).enumerate() {
            assert_eq!(got.delay, want.delay, "{w}x{h} frame {i}: delay");
            assert_eq!(got.image.format, AvifPixelFormat::Yuva444P);
            assert!(got.image.color.is_identity_matrix());
            assert!(
                got.image.to_rgba8() == want.image.to_rgba8(),
                "{w}x{h} frame {i}: RGBA not exact"
            );
            assert!(got.image.planes == want.image.planes, "{w}x{h} frame {i}");
        }
        // Two tracks: the picture track and its alpha auxiliary.
        let avis = parse_avis(&file).unwrap();
        assert_eq!(avis.samples.len(), frames.len());
        let info = oxideav_avif::info(&file).unwrap();
        assert!(info.has_alpha, "{w}x{h}: info reports the alpha track");
        assert_eq!((info.width, info.height), (w, h));
        assert_eq!(info.frames as usize, frames.len());
    }
}

/// Y′CbCr 4:2:0 + alpha and gray + alpha sequences, 8 and 10 bits.
#[test]
fn ycbcr_and_gray_alpha_sequences_round_trip() {
    let (w, h) = (16u32, 8u32);
    let n = (w * h) as usize;
    let mut cases: Vec<(AvifPixelFormat, u8, Vec<Vec<AvifPlane>>)> = Vec::new();
    // 8-bit 4:2:0 + alpha.
    cases.push((
        AvifPixelFormat::Yuva420P,
        8,
        (0..3)
            .map(|t| {
                vec![
                    AvifPlane::new(w as usize, noise(n, t)),
                    AvifPlane::new(w as usize / 2, noise(n / 4, t + 10)),
                    AvifPlane::new(w as usize / 2, noise(n / 4, t + 20)),
                    AvifPlane::new(w as usize, noise(n, t + 30)),
                ]
            })
            .collect(),
    ));
    // 8-bit gray + alpha (packed).
    cases.push((
        AvifPixelFormat::Ya8,
        8,
        (0..3)
            .map(|t| vec![AvifPlane::new(w as usize * 2, noise(n * 2, t + 40))])
            .collect(),
    ));
    // 10-bit 4:4:4 + alpha.
    let words = |count: usize, seed: u32| -> Vec<u8> {
        noise(count * 2, seed)
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|b| (u16::from_le_bytes(*b) & 0x3ff).to_le_bytes())
            .collect()
    };
    cases.push((
        AvifPixelFormat::Yuva444P10Le,
        10,
        (0..3)
            .map(|t| {
                (0..4)
                    .map(|p| AvifPlane::new(w as usize * 2, words(n, t * 7 + p)))
                    .collect()
            })
            .collect(),
    ));
    for (format, depth, planes) in cases {
        let frames: Vec<Frame> = planes
            .into_iter()
            .map(|p| {
                Frame::new(
                    AvifImage::new(w, h, format, p)
                        .unwrap()
                        .with_bit_depth(depth),
                    Some(Duration::from_millis(100)),
                )
            })
            .collect();
        let file = encode_all(&frames, &EncodeOptions::default().with_lossless(true)).unwrap();
        let back = decode_all(&file).unwrap();
        assert_eq!(back.len(), frames.len(), "{format:?}");
        for (i, (got, want)) in back.iter().zip(&frames).enumerate() {
            assert_eq!(got.image.format, format, "{format:?} frame {i}");
            assert!(
                got.image.planes == want.image.planes,
                "{format:?} frame {i}"
            );
            assert_eq!(got.delay, want.delay);
        }
    }
}

/// A lossy colour quality with lossless alpha keeps the alpha exact;
/// a lossy alpha quantiser does not.
#[test]
fn alpha_track_follows_the_alpha_quantiser() {
    let frames = rgba_frames(32, 24, 3);
    let alphas = |file: &[u8]| -> Vec<Vec<u8>> {
        decode_all(file)
            .unwrap()
            .iter()
            .map(|f| f.image.planes[3].data.clone())
            .collect()
    };
    let want: Vec<Vec<u8>> = frames
        .iter()
        .map(|f| f.image.planes[3].data.clone())
        .collect();
    let exact_alpha = encode_all(&frames, &EncodeOptions::default().with_quality(60)).unwrap();
    assert!(alphas(&exact_alpha) == want, "alpha is lossless by default");
    let lossy_alpha = encode_all(
        &frames,
        &EncodeOptions::default()
            .with_quality(60)
            .with_alpha_q_idx(160),
    )
    .unwrap();
    assert!(alphas(&lossy_alpha) != want);
    assert!(lossy_alpha.len() < exact_alpha.len());
}

/// Every frame carries alpha or none does; a mix is refused by data.
#[test]
fn mixed_alpha_presence_is_refused() {
    let mut frames = rgba_frames(16, 16, 3);
    frames[1].image = AvifImage::from_rgb8(16, 16, noise(16 * 16 * 3, 1)).unwrap();
    let err = encode_all(&frames, &EncodeOptions::default()).unwrap_err();
    assert!(
        matches!(err, oxideav_avif::Error::InvalidData(_)),
        "{err:?}"
    );
}

fn stills(w: u32, h: u32, count: u32) -> Vec<StillImage> {
    (0..count)
        .map(|t| StillImage::rgba8(w, h, &rgba_frame(w, h, t)).unwrap())
        .collect()
}

/// All-intra samples (and the alpha samples) are coded on the worker
/// budget; the file is the same at any budget.
#[test]
fn sequence_bytes_do_not_depend_on_the_worker_budget() {
    let frames = stills(40, 24, 6);
    let opts = SequenceEncodeOptions::default()
        .with_all_intra(true)
        .with_base_q_idx(40);
    let serial = encode_sequence(&frames, &opts).unwrap();
    for threads in [2usize, 4, 16] {
        let parallel = encode_sequence(&frames, &opts.with_threads(threads)).unwrap();
        assert!(serial == parallel, "{threads} workers changed the bytes");
    }
    assert_eq!(decode_all(&serial).unwrap().len(), 6);
}

/// The loop count is the edit list MIAF Annex A.2 reads it from.
#[test]
fn loop_count_is_written_as_the_repeating_edit_list() {
    use oxideav_heif::sequence::LoopBehaviour;
    let frames = stills(16, 16, 3);
    for (count, want) in [
        (None, LoopBehaviour::Forever),
        (Some(0), LoopBehaviour::Forever),
        (Some(1), LoopBehaviour::Times(1)),
        (Some(4), LoopBehaviour::Times(4)),
    ] {
        let opts = SequenceEncodeOptions::default().with_loop_count(count);
        let file = encode_sequence(&frames, &opts).unwrap();
        let heif = oxideav_heif::HeifFile::parse(file.as_slice()).unwrap();
        let movie = oxideav_heif::sequence::parse_movie(&heif).unwrap().unwrap();
        let track = movie
            .tracks
            .iter()
            .find(|t| t.handler == *b"pict")
            .expect("picture track");
        assert_eq!(track.loop_behaviour(), want, "loop_count {count:?}");
        assert_eq!(track.edits.is_empty(), count.is_none());
        assert_eq!(decode_all(&file).unwrap().len(), 3, "loop_count {count:?}");
    }
}

/// Frame 0's Exif / XMP describe the sequence and come back on the
/// decoded frames.
#[test]
fn sequence_metadata_rides_the_picture_track() {
    let mut exif = b"II*\0".to_vec();
    exif.extend_from_slice(&8u32.to_le_bytes());
    exif.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let xmp = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>".to_vec();
    let mut frames = rgba_frames(16, 16, 3);
    let icc = noise(200, 77);
    frames[0].image = frames[0].image.clone().with_metadata(Metadata::new(
        Some(icc.clone()),
        Some(exif.clone()),
        Some(xmp.clone()),
        None,
    ));
    let file = encode_all(&frames, &EncodeOptions::default()).unwrap();
    let back = decode_all(&file).unwrap();
    assert_eq!(back.len(), 3);
    assert_eq!(
        back[0].image.metadata.exif.as_deref(),
        Some(exif.as_slice())
    );
    assert_eq!(back[0].image.metadata.xmp.as_deref(), Some(xmp.as_slice()));
    assert_eq!(back[0].image.metadata.icc.as_deref(), Some(icc.as_slice()));
    assert_eq!(back[0].image.color, ColorInfo::identity_full_range());
    // The pixels are untouched by the metadata.
    assert!(back[2].image.planes == frames[2].image.planes);
}
