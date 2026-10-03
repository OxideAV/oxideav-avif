#![no_main]

//! The image-crate API on raw bytes: `probe` → `info` → `decode_with`
//! → `to_rgba8` / `to_rgb8`, then `decode_all_with`, under tight
//! `DecodeOptions` limits so a hostile header cannot drive a large
//! allocation. The invariant is "no panic, bounded memory": hostile
//! input surfaces as `Err`, a successful decode is self-consistent
//! (plane geometry covers the picture, the RGB views have the right
//! length, `info` also succeeds), and a lossless re-encode of a small
//! decoded picture decodes back to the same planes.
//!
//! The first byte steers the options (strict / tone-mapped / layer);
//! the rest is the file.

use libfuzzer_sys::fuzz_target;
use oxideav_avif::{
    decode_all_with, decode_with, encode, info, probe, DecodeOptions, EncodeOptions,
};

/// Keep every decode inside a few megabytes of pixels.
const MAX_SIDE: u32 = 512;
const MAX_PIXELS: u64 = 1 << 16;

fuzz_target!(|data: &[u8]| {
    let Some((&knob, file)) = data.split_first() else {
        return;
    };
    let opts = DecodeOptions::default()
        .with_max_width(Some(MAX_SIDE))
        .with_max_height(Some(MAX_SIDE))
        .with_max_pixels(Some(MAX_PIXELS))
        .with_max_bytes(Some(1 << 20))
        .with_strict(knob & 1 != 0)
        .with_tone_mapped(knob & 2 != 0)
        .with_layer(if knob & 4 != 0 {
            Some(u16::from(knob >> 3))
        } else {
            None
        });

    // Never panics, never allocates.
    let _ = probe(file);

    let header = info(file);
    if let Ok(i) = &header {
        assert_eq!(i.format.has_alpha(), i.has_alpha);
    }

    let decoded = decode_with(file, &opts);
    if let Ok(img) = &decoded {
        assert!(header.is_ok(), "decode succeeded but info failed");
        assert!(img.width() > 0 && img.height() > 0);
        assert!(img.width() <= MAX_SIDE && img.height() <= MAX_SIDE);
        assert!(u64::from(img.width()) * u64::from(img.height()) <= MAX_PIXELS);
        assert_eq!(img.planes.len(), img.format().plane_count());
        let n = img.width() as usize * img.height() as usize;
        // The exact view may refuse a matrix without a kernel; the
        // infallible view then reads the picture as BT.601.
        match img.try_to_rgba8() {
            Ok(rgba) => assert_eq!(rgba.len(), n * 4),
            Err(oxideav_avif::Error::Unsupported(_)) => {}
            Err(e) => panic!("decoded planes convert: {e}"),
        }
        let rgba = img.to_rgba8();
        assert_eq!(rgba.len(), n * 4);
        let rgb = img.to_rgb8();
        assert_eq!(rgb.len(), n * 3);
        if !img.has_alpha() {
            assert!(rgba.chunks_exact(4).all(|p| p[3] == 255));
        }
        // A small picture re-encodes losslessly and decodes back the
        // same (sub-sampled layouts at odd extents are refused, which
        // is also fine).
        if n <= 64 * 64 {
            match encode(img, &EncodeOptions::default()) {
                Ok(bytes) => {
                    // Limits only: the re-encode is a plain single-layer
                    // still, so the layer / strict knobs do not apply.
                    let plain = DecodeOptions::default()
                        .with_max_width(Some(MAX_SIDE))
                        .with_max_height(Some(MAX_SIDE))
                        .with_max_pixels(Some(MAX_PIXELS));
                    let back = decode_with(&bytes, &plain).expect("own output decodes");
                    assert_eq!(back.planes, img.planes);
                    assert_eq!(back.format(), img.format());
                    assert_eq!(back.color, img.color);
                }
                Err(oxideav_avif::Error::Unsupported(_)) => {}
                Err(e) => panic!("encode of a decoded picture failed: {e}"),
            }
        }
    }

    if let Ok(frames) = decode_all_with(file, &opts) {
        assert!(!frames.is_empty());
        for f in &frames {
            assert!(f.image.width() <= MAX_SIDE && f.image.height() <= MAX_SIDE);
            assert_eq!(f.image.planes.len(), f.image.format().plane_count());
        }
        // The primary is one of the frames (frame 0 of a sequence or
        // a still; any position inside a burst group).
        if let Ok(img) = &decoded {
            assert!(frames.iter().any(|f| &f.image == img));
        }
    }
});
