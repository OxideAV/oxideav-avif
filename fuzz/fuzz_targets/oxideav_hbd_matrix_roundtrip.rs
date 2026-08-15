#![no_main]

//! HBD-matrix self-roundtrip: fuzz-shaped planes across the whole
//! (bit depth × chroma format × alpha) matrix — 8/10/12-bit ×
//! 4:2:0 / 4:2:2 / 4:4:4 / monochrome, alpha auxiliary on or off —
//! lossless `oxideav_avif::encode_still` → decode back through
//! `oxideav_avif`'s own decoder → assert every plane is
//! **sample-exact**. Fully in-tree; never skips.
//!
//! This is the fuzz companion of the high-bit-depth composition layer:
//! the 10/12-bit legs decode into little-endian 16-bit word planes
//! (the `*10Le` / `*12Le` / `Ya16Le` output surfaces), so any drift in
//! the HBD grid/alpha/transform byte maths, the `clap`-back crop of
//! padded extents, or the packed `Ya16Le` interleave shows up as a
//! word mismatch.

use libfuzzer_sys::fuzz_target;
use oxideav_avif::{encode_still, AvifDecoder, StillChroma, StillEncodeOptions, StillImage};
use oxideav_core::{CodecId, Decoder, Frame, Packet, TimeBase};

const MAX_WIDTH: usize = 40;
const MAX_PIXELS: usize = 2048;

/// Reassemble a decoded plane (1 or 2 bytes per sample) into `u16`s.
fn plane_words(data: &[u8], two_byte: bool) -> Vec<u16> {
    if two_byte {
        data.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    } else {
        data.iter().map(|&b| b as u16).collect()
    }
}

fuzz_target!(|data: &[u8]| {
    // Shape bytes: [0] width, [1] depth/chroma/alpha selector, rest =
    // sample stream.
    if data.len() < 3 {
        return;
    }
    let shape_w = data[0];
    let sel = data[1];
    let samples = &data[2..];

    let bit_depth = [8u8, 10, 12][(sel & 0x03) as usize % 3];
    let chroma = [
        StillChroma::Yuv420,
        StillChroma::Yuv422,
        StillChroma::Yuv444,
        StillChroma::Monochrome,
    ][((sel >> 2) & 0x03) as usize];
    let with_alpha = (sel >> 4) & 1 == 1;

    let pixel_count = samples.len().min(MAX_PIXELS);
    if pixel_count == 0 {
        return;
    }
    let mut width = ((shape_w as usize) % MAX_WIDTH).max(1) + 1;
    let mut height = (pixel_count / width).max(1);
    // Subsampled layouts need even extents on the halved axes.
    match chroma {
        StillChroma::Yuv420 => {
            width &= !1;
            height &= !1;
        }
        StillChroma::Yuv422 => {
            width &= !1;
        }
        _ => {}
    }
    if width == 0 || height == 0 {
        return;
    }

    let mask = (1u16 << bit_depth) - 1;
    let n = width * height;
    let sample = |i: usize, salt: u16| -> u16 {
        let b = samples[i % samples.len()] as u16;
        (b.wrapping_mul(2654435761u32 as u16)
            .wrapping_add(salt.wrapping_mul(97))
            .wrapping_add(i as u16))
            & mask
    };
    let y: Vec<u16> = (0..n).map(|i| sample(i, 1)).collect();
    let (sx, sy) = match chroma {
        StillChroma::Yuv420 => (1u32, 1u32),
        StillChroma::Yuv422 => (1, 0),
        _ => (0, 0),
    };
    let (u, v) = if chroma == StillChroma::Monochrome {
        (Vec::new(), Vec::new())
    } else {
        let cn = (width >> sx) * (height >> sy);
        (
            (0..cn).map(|i| sample(i, 2)).collect(),
            (0..cn).map(|i| sample(i, 3)).collect(),
        )
    };
    let img = StillImage::yuv(
        width as u32,
        height as u32,
        bit_depth,
        chroma,
        y.clone(),
        u.clone(),
        v.clone(),
    )
    .expect("shape-valid StillImage");
    let alpha: Option<Vec<u16>> = with_alpha.then(|| (0..n).map(|i| sample(i, 4)).collect());
    let img = match &alpha {
        Some(a) => img.with_alpha(a.clone()).expect("alpha shape"),
        None => img,
    };

    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode_still");

    let mut dec = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), avif);
    dec.send_packet(&pkt).expect("decode our own encode");
    let vf = match dec.receive_frame().expect("frame") {
        Frame::Video(v) => v,
        other => panic!("expected VideoFrame, got {other:?}"),
    };

    let two_byte = bit_depth > 8;
    let packed_ya = chroma == StillChroma::Monochrome && with_alpha;
    if packed_ya {
        // Packed Ya8 / Ya16Le: one image plane, Y A interleaved.
        assert_eq!(vf.image_plane_count(), 1, "packed YA plane count");
        let words = plane_words(&vf.planes[0].data, two_byte);
        assert_eq!(words.len(), n * 2, "packed YA word count");
        let a = alpha.as_ref().unwrap();
        for i in 0..n {
            assert_eq!(words[i * 2], y[i], "Y at {i}");
            assert_eq!(words[i * 2 + 1], a[i], "A at {i}");
        }
        if two_byte {
            assert_eq!(
                vf.plane_significant_bits(0),
                Some(bit_depth),
                "Ya16Le significant bits"
            );
        }
    } else {
        let expect_planes = match (chroma, with_alpha) {
            (StillChroma::Monochrome, false) => 1,
            (_, false) => 3,
            (_, true) => 4,
        };
        assert_eq!(vf.image_plane_count(), expect_planes, "plane count");
        assert_eq!(plane_words(&vf.planes[0].data, two_byte), y, "Y plane");
        if chroma != StillChroma::Monochrome {
            assert_eq!(plane_words(&vf.planes[1].data, two_byte), u, "U plane");
            assert_eq!(plane_words(&vf.planes[2].data, two_byte), v, "V plane");
        }
        if let Some(a) = &alpha {
            assert_eq!(plane_words(&vf.planes[3].data, two_byte), *a, "A plane");
        }
    }
});
