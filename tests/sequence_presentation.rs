//! An `avis` track's samples are presented the way MIAF §6.6 step 2
//! asks a reader to ("apply matrix of TrackHeaderBox and
//! CleanApertureBox to the decoded pictures"): the `av01` sample
//! entry's clean aperture first (§7.4.4.1), then the `tkhd` matrix's
//! mirror / rotation (HEIF §7.2.1). The decoded extents, `info()`,
//! `stream_params()` and the registry decoder's `output_params()` all
//! agree.

#![cfg(feature = "registry")]

use oxideav_avif::{
    AvifDecoder, SequenceEncodeOptions, StillChroma, StillImage, decode_all, encode_sequence, info,
    stream_params,
};
use oxideav_core::{CodecId, Decoder, Frame, Packet, TimeBase};

/// Two all-intra 8-bit monochrome samples of `w × h` with a gradient
/// that tells every pixel apart.
fn mono_sequence(w: u32, h: u32) -> (Vec<u8>, Vec<Vec<u16>>) {
    let pictures: Vec<Vec<u16>> = (0..2u32)
        .map(|i| {
            (0..w * h)
                .map(|k| ((k % w) * 5 + (k / w) * 11 + i * 3) as u16 % 230 + 12)
                .collect()
        })
        .collect();
    let frames: Vec<StillImage> = pictures
        .iter()
        .map(|y| {
            StillImage::yuv(
                w,
                h,
                8,
                StillChroma::Monochrome,
                y.clone(),
                Vec::new(),
                Vec::new(),
            )
            .unwrap()
        })
        .collect();
    // Lossless, so the presented samples are exactly the source.
    let opts = SequenceEncodeOptions::default()
        .with_all_intra(true)
        .with_base_q_idx(0);
    (encode_sequence(&frames, &opts).unwrap(), pictures)
}

/// The luma samples of a decoded 8-bit picture, row-major, tight.
fn luma(image: &oxideav_avif::AvifImage) -> Vec<u16> {
    let p = &image.planes[0];
    let mut out = Vec::new();
    for r in 0..image.height as usize {
        let row = &p.data[r * p.stride..r * p.stride + image.width as usize];
        out.extend(row.iter().map(|&v| u16::from(v)));
    }
    out
}

/// Overwrite the first `tkhd`'s matrix (ISO/IEC 14496-12 §8.3.2).
fn with_matrix(file: &[u8], m: [i32; 9]) -> Vec<u8> {
    let mut out = file.to_vec();
    let at = out
        .windows(4)
        .position(|w| w == b"tkhd")
        .expect("tkhd present");
    let version = out[at + 4];
    // fullbox word, then times / track_ID / reserved / duration (v0:
    // 4+4+4+4+4, v1: 8+8+4+4+8), reserved[2], layer, alternate_group,
    // volume, reserved.
    let fixed = if version == 1 { 32 } else { 20 };
    let matrix_at = at + 4 + 4 + fixed + 8 + 8;
    for (i, v) in m.iter().enumerate() {
        out[matrix_at + 4 * i..matrix_at + 4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

const ONE: i32 = 0x0001_0000;
const W: i32 = 0x4000_0000;

#[test]
fn clean_aperture_crops_every_sample() {
    // 31 × 17 codes as a padded 32 × 24 picture with a top-left `clap`
    // in the sample entry; black-box readers present 31 × 17.
    let (file, pictures) = mono_sequence(31, 17);
    let i = info(&file).unwrap();
    assert_eq!((i.width, i.height), (31, 17));
    let p = stream_params(&file).unwrap();
    assert_eq!((p.width, p.height), (Some(31), Some(17)));
    let frames = decode_all(&file).unwrap();
    assert_eq!(frames.len(), 2);
    for (f, src) in frames.iter().zip(&pictures) {
        assert_eq!((f.image.width, f.image.height), (31, 17));
        assert_eq!(&luma(&f.image), src, "lossless sample exact after the crop");
    }
    let mut dec = AvifDecoder::new(CodecId::new("avif"));
    dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), file))
        .unwrap();
    let Frame::Video(vf) = dec.receive_frame().unwrap() else {
        panic!("video")
    };
    let out = dec.output_params().unwrap();
    assert_eq!((out.width, out.height), (Some(31), Some(17)));
    assert_eq!(vf.planes[0].stride, 31);
    assert_eq!(vf.planes[0].data.len(), 31 * 17);
}

/// `(x, y)` of the source pixel shown at output `(ox, oy)`, for the
/// matrix `[[a, c], [b, d]]` mapping source to output with the
/// translation chosen so the output starts at the origin.
fn source_of(m: [[i32; 2]; 2], (w, h): (i32, i32), (ox, oy): (i32, i32)) -> (i32, i32) {
    // Forward map of the source corners → output bounding box origin.
    let fwd = |p: i32, q: i32| (m[0][0] * p + m[0][1] * q, m[1][0] * p + m[1][1] * q);
    let corners = [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)].map(|(p, q)| fwd(p, q));
    let min_x = corners.iter().map(|c| c.0).min().unwrap();
    let min_y = corners.iter().map(|c| c.1).min().unwrap();
    for q in 0..h {
        for p in 0..w {
            let (x, y) = fwd(p, q);
            if (x - min_x, y - min_y) == (ox, oy) {
                return (p, q);
            }
        }
    }
    unreachable!("({ox}, {oy}) is not an output pixel")
}

#[test]
fn track_matrix_mirrors_and_rotates_every_sample() {
    let (w, h) = (16u32, 8u32);
    let (file, pictures) = mono_sequence(w, h);
    // Each of the seven non-identity matrices HEIF §7.2.1 allows.
    let cases: [[[i32; 2]; 2]; 7] = [
        [[-1, 0], [0, 1]],
        [[1, 0], [0, -1]],
        [[-1, 0], [0, -1]],
        [[0, 1], [-1, 0]],
        [[0, -1], [1, 0]],
        [[0, 1], [1, 0]],
        [[0, -1], [-1, 0]],
    ];
    for m in cases {
        // Stored order {a, b, u, c, d, v, x, y, w}.
        let stored = [
            m[0][0] * ONE,
            m[1][0] * ONE,
            0,
            m[0][1] * ONE,
            m[1][1] * ONE,
            0,
            0,
            0,
            W,
        ];
        let patched = with_matrix(&file, stored);
        let swaps = m[0][0] == 0;
        let (ow, oh) = if swaps { (h, w) } else { (w, h) };
        let i = info(&patched).unwrap();
        assert_eq!((i.width, i.height), (ow, oh), "{m:?}");
        let frames = decode_all(&patched).unwrap();
        for (f, src) in frames.iter().zip(&pictures) {
            assert_eq!((f.image.width, f.image.height), (ow, oh), "{m:?}");
            let got = luma(&f.image);
            for oy in 0..oh as i32 {
                for ox in 0..ow as i32 {
                    let (p, q) = source_of(m, (w as i32, h as i32), (ox, oy));
                    assert_eq!(
                        got[(oy * ow as i32 + ox) as usize],
                        src[(q * w as i32 + p) as usize],
                        "{m:?} at ({ox}, {oy})"
                    );
                }
            }
        }
        let mut dec = AvifDecoder::new(CodecId::new("avif"));
        dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), patched))
            .unwrap();
        dec.receive_frame().unwrap();
        let out = dec.output_params().unwrap();
        assert_eq!((out.width, out.height), (Some(ow), Some(oh)), "{m:?}");
        assert_eq!(dec.output_params_generation(), 0);
    }
    // A scaling matrix is not an orientation: presented as decoded.
    let scaled = with_matrix(&file, [2 * ONE, 0, 0, 0, 2 * ONE, 0, 0, 0, W]);
    let frames = decode_all(&scaled).unwrap();
    assert_eq!((frames[0].image.width, frames[0].image.height), (w, h));
    assert_eq!(luma(&frames[0].image), pictures[0]);
}
