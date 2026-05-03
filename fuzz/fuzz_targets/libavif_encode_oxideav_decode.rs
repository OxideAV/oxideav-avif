#![no_main]

//! Cross-decode harness: encode random RGBA via libavif, then drive
//! oxideav-avif's decoder over the resulting AVIF bitstream and check
//! that nothing panics. Dimensions are asserted on successful decode;
//! decode errors are tolerated because the underlying oxideav-av1
//! decoder is still incomplete and may legitimately reject bitstreams
//! that libavif (via aom) emits.
//!
//! The harness runs libavif's encoder with `lossless=1` + YUV444 (no
//! chroma subsampling). True bit-exact lossless additionally requires
//! `matrixCoefficients = IDENTITY` on the avifImage, which would mean
//! poking an unstable struct layout — we deliberately don't (see the
//! comment in fuzz/src/lib.rs). The harness only asserts that decode
//! doesn't panic and that dimensions are recovered correctly, not
//! pixel equality, so a slightly-lossy YUV transform is acceptable.

use libfuzzer_sys::fuzz_target;
use oxideav_avif::AvifDecoder;
use oxideav_avif_fuzz::libavif;
use oxideav_core::{CodecId, Decoder, Frame, Packet, TimeBase};

const MAX_WIDTH: usize = 64;
const MAX_PIXELS: usize = 2048;

fuzz_target!(|data: &[u8]| {
    // Skip silently if libavif isn't installed on this host.
    if !libavif::available() {
        return;
    }

    let Some((width, height, rgba)) = image_from_fuzz_input(data) else {
        return;
    };

    // libavif may reject e.g. width-1 inputs (AV1 has minimum block
    // sizes); treat encode failures as "skip this input" rather than
    // a fuzz failure.
    let Some(encoded) = libavif::encode_lossless_rgba(rgba, width, height) else {
        return;
    };

    let mut d = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), encoded);
    // We tolerate decoder errors — oxideav-av1 is still maturing and
    // may reject inputs libavif happily produces. The fuzz win is
    // catching panics, not enforcing decode success.
    if d.send_packet(&pkt).is_err() {
        return;
    }
    let frame = match d.receive_frame() {
        Ok(f) => f,
        Err(_) => return,
    };
    let vf = match frame {
        Frame::Video(v) => v,
        other => panic!("expected VideoFrame, got {other:?}"),
    };
    assert!(!vf.planes.is_empty(), "decoded frame has no planes");

    // Sanity-check dimensions: oxideav-avif's `infer_av1_pixmap`
    // recovers width from the Y plane's stride. A successful decode
    // should report dimensions that at least cover the requested
    // area (libavif rounds up to AV1 block alignment, so >= is the
    // correct relation, not ==).
    let y = &vf.planes[0];
    assert!(y.stride > 0, "Y plane stride is zero");
    let inferred_w = y.stride as u32;
    let inferred_h = (y.data.len() / y.stride) as u32;
    assert!(
        inferred_w >= width,
        "inferred width {inferred_w} < requested {width}"
    );
    assert!(
        inferred_h >= height,
        "inferred height {inferred_h} < requested {height}"
    );
});

fn image_from_fuzz_input(data: &[u8]) -> Option<(u32, u32, &[u8])> {
    let (&shape, rgba) = data.split_first()?;

    let pixel_count = (rgba.len() / 4).min(MAX_PIXELS);
    if pixel_count == 0 {
        return None;
    }

    // libavif/aom reject single-pixel-wide inputs in some configs;
    // start from width=2 to keep the encoder happy. Min block size in
    // AV1 is 4x4 but the encoder generally pads up automatically.
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
