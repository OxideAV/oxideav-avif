#![no_main]

//! The literal #304 self-roundtrip, now that oxideav-avif ships a real
//! pixel encoder: fuzz-shaped RGBA pixels → `oxideav_avif::encode_still`
//! (lossless identity-matrix 4:4:4 + alpha auxiliary) → decode back
//! through `oxideav_avif`'s own decoder → assert the pixels are
//! **byte-exact**. Runs fully in-tree — no external library involved —
//! so it never skips.
//!
//! Lossless exactness is the strongest invariant the surface offers:
//! any drift in the AV1 KEY-frame encode, the container muxing
//! (`ispe` / `clap` / `auxl` wiring), the HEIF re-parse, the AV1
//! decode, or the alpha/transform composition shows up as a byte
//! mismatch.

use libfuzzer_sys::fuzz_target;
use oxideav_avif::{encode_still, AvifDecoder, StillEncodeOptions, StillImage};
use oxideav_core::{CodecId, Decoder, Frame, Packet, TimeBase};

const MAX_WIDTH: usize = 64;
const MAX_PIXELS: usize = 4096;

/// Same input shaping as the cross-decode harnesses: first byte picks
/// the width, the rest is RGBA pixel data.
fn image_from_fuzz_input(data: &[u8]) -> Option<(u32, u32, &[u8])> {
    let (&shape, rgba) = data.split_first()?;
    let pixel_count = (rgba.len() / 4).min(MAX_PIXELS);
    if pixel_count == 0 {
        return None;
    }
    let width = ((shape as usize) % MAX_WIDTH).max(1) + 1;
    let width = width.min(pixel_count);
    let height = pixel_count / width;
    if height == 0 {
        return None;
    }
    let used_len = width * height * 4;
    let rgba = &rgba[..used_len];
    Some((width as u32, height as u32, rgba))
}

fuzz_target!(|data: &[u8]| {
    let Some((width, height, rgba)) = image_from_fuzz_input(data) else {
        return;
    };

    // Encode: lossless identity-matrix 4:4:4 with the A channel as an
    // alpha auxiliary. Construction only fails on shape violations the
    // shaper cannot produce; encode failures are real bugs.
    let img = StillImage::rgba8(width, height, rgba).expect("rgba8 construction");
    let avif = encode_still(&img, &StillEncodeOptions::default()).expect("encode_still");

    // Decode back through our own decoder.
    let mut dec = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), avif);
    dec.send_packet(&pkt).expect("decode our own encode");
    let vf = match dec.receive_frame().expect("frame") {
        Frame::Video(v) => v,
        other => panic!("expected VideoFrame, got {other:?}"),
    };
    assert_eq!(vf.planes.len(), 4, "identity RGBA decodes to YUVA 4:4:4");

    // Byte-exact: identity matrix maps Y=G, U=B, V=R; alpha verbatim.
    let n = (width as usize) * (height as usize);
    for i in 0..n {
        let px = &rgba[i * 4..i * 4 + 4];
        assert_eq!(vf.planes[2].data[i], px[0], "R (Cr) at {i}");
        assert_eq!(vf.planes[0].data[i], px[1], "G (Y) at {i}");
        assert_eq!(vf.planes[1].data[i], px[2], "B (Cb) at {i}");
        assert_eq!(vf.planes[3].data[i], px[3], "A at {i}");
    }
});
