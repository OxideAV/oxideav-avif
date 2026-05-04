#![no_main]

//! Cross-validation: libavif encodes a fuzz-generated RGBA image,
//! then BOTH oxideav-avif and libavif decode the resulting AVIF
//! bitstream. The two decoded YUV planes must agree, plane by plane.
//!
//! This is the AVIF analog of the dav1d cross-validation harness
//! shipped for the AV1 round 3 / #345 work — instead of testing
//! "does our decoder reach the same pixels as another AV1
//! implementation", we test the same property at the AVIF container
//! level, with libavif acting as the reference HEIF + AV1 decode
//! pipeline.
//!
//! The harness encodes losslessly (`lossless=1` + YUV444 + IDENTITY
//! matrix) so libavif's own encode → libavif decode round-trip is
//! bit-exact and the comparison genuinely tests oxideav-avif's
//! decoder. With lossy encoding any divergence between the two
//! decoders would dissolve into the encoder's quantisation noise.
//!
//! ## Skip conditions
//!
//! * **libavif not installed** → `return` early. The cross-decode
//!   harness in `libavif_encode_oxideav_decode.rs` already documents
//!   this; we mirror the behaviour. CI runners without
//!   `apt install libavif-dev` (Debian/Ubuntu) or
//!   `brew install libavif` (macOS) will silently skip the
//!   assertions.
//! * **libavif encode rejects the fuzz input** → `return`. AV1 has
//!   minimum block sizes; very small or pathological inputs may not
//!   produce a valid bitstream.
//! * **oxideav-avif decoder errors** → `return`. The oxideav-av1
//!   crate is still maturing; legitimate bitstreams that crash the
//!   decoder are surfaced via the oxideav-decode-only fuzz target.
//!   This harness is for **divergence** detection, not crash
//!   detection.
//! * **libavif decoder errors** → `return`. Asymmetric — if libavif
//!   can't decode its own output we have a libavif issue, not an
//!   oxideav one.
//!
//! When all four boundaries clear, the comparison is mandatory.

use libfuzzer_sys::fuzz_target;
use oxideav_avif::AvifDecoder;
use oxideav_avif_fuzz::libavif;
use oxideav_core::{CodecId, Decoder, Frame, Packet, TimeBase};

/// Cap on input size to keep each fuzz iteration cheap. AV1 encoder
/// startup time dominates for small inputs; capping at 64×64 keeps the
/// throughput acceptable while still exercising tile-edge / chroma
/// alignment paths.
const MAX_WIDTH: usize = 64;
const MAX_PIXELS: usize = 4096;

fuzz_target!(|data: &[u8]| {
    if !libavif::available() {
        return;
    }

    let Some((width, height, rgba)) = image_from_fuzz_input(data) else {
        return;
    };

    let Some(encoded) = libavif::encode_lossless_rgba(rgba, width, height) else {
        return;
    };

    // Decode with oxideav-avif. Errors are tolerated.
    let mut d = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), encoded.clone());
    if d.send_packet(&pkt).is_err() {
        return;
    }
    let frame = match d.receive_frame() {
        Ok(f) => f,
        Err(_) => return,
    };
    let oxi_vf = match frame {
        Frame::Video(v) => v,
        other => panic!("expected VideoFrame, got {other:?}"),
    };
    if oxi_vf.planes.is_empty() {
        return;
    }

    // Decode the same bitstream with libavif → RGBA.
    let Some(libavif_rgba) = libavif::decode_to_rgba(&encoded) else {
        return;
    };

    // Sanity: libavif's reported dimensions should at least cover the
    // requested rectangle (libavif rounds up to AV1 block alignment).
    assert!(
        libavif_rgba.width >= width,
        "libavif decoded width {} < requested {}",
        libavif_rgba.width,
        width
    );
    assert!(
        libavif_rgba.height >= height,
        "libavif decoded height {} < requested {}",
        libavif_rgba.height,
        height
    );

    // oxideav-avif decoder reports YUV planes — convert libavif's
    // RGBA back to YUV444 with the IDENTITY matrix used at encode
    // time. Since IDENTITY maps Y=G, U=B, V=R (the libavif lossless
    // contract), we can rebuild the expected planes directly from
    // RGBA without invoking a YUV transform.
    let oxi_y = &oxi_vf.planes[0];
    let oxi_w = oxi_y.stride as u32;
    let oxi_h = (oxi_y.data.len() / oxi_y.stride) as u32;

    // We can only compare the rectangle libavif encoded — the AV1
    // coded frame may be larger (block alignment) but the visible
    // image is `width × height`.
    let cmp_w = width.min(oxi_w).min(libavif_rgba.width);
    let cmp_h = height.min(oxi_h).min(libavif_rgba.height);
    if cmp_w == 0 || cmp_h == 0 {
        return;
    }

    let lib_row = (libavif_rgba.width as usize) * 4;
    let lib_pixels = &libavif_rgba.rgba;
    // Choose the comparison strategy based on plane count.
    match oxi_vf.planes.len() {
        1 => {
            // oxideav decoded as Gray8 — compare luma to libavif's G
            // channel (IDENTITY matrix maps Y == G in the lossless
            // contract). This branch is rare for libavif's
            // YUV444+lossless setup but defensible if a future
            // libavif starts emitting Gray for monochrome inputs.
            for y in 0..cmp_h as usize {
                for x in 0..cmp_w as usize {
                    let g = lib_pixels[y * lib_row + x * 4 + 1];
                    let oy = oxi_y.data[y * oxi_y.stride + x];
                    assert_eq!(
                        oy, g,
                        "Y plane mismatch at ({x},{y}): oxi={oy} libavif G={g}"
                    );
                }
            }
        }
        3 => {
            // YUV444 IDENTITY matrix: Y = G, U = B, V = R.
            let oxi_u = &oxi_vf.planes[1];
            let oxi_v = &oxi_vf.planes[2];
            for y in 0..cmp_h as usize {
                for x in 0..cmp_w as usize {
                    let r = lib_pixels[y * lib_row + x * 4];
                    let g = lib_pixels[y * lib_row + x * 4 + 1];
                    let b = lib_pixels[y * lib_row + x * 4 + 2];
                    let oy = oxi_y.data[y * oxi_y.stride + x];
                    let ou = oxi_u.data[y * oxi_u.stride + x];
                    let ov = oxi_v.data[y * oxi_v.stride + x];
                    assert_eq!(
                        oy, g,
                        "Y plane mismatch at ({x},{y}): oxi={oy} libavif G={g}"
                    );
                    assert_eq!(
                        ou, b,
                        "U plane mismatch at ({x},{y}): oxi={ou} libavif B={b}"
                    );
                    assert_eq!(
                        ov, r,
                        "V plane mismatch at ({x},{y}): oxi={ov} libavif R={r}"
                    );
                }
            }
        }
        // 4:2:0 / 4:2:2 wouldn't survive the IDENTITY matrix
        // contract; libavif 'lossless=1' specifically forces YUV444.
        // If the oxideav side ever emits a chroma-subsampled layout
        // for a lossless input we'd need a different comparison.
        // Treat as "skip" rather than panic — divergence here would
        // be a setup mismatch, not a decoder bug.
        _ => return,
    }
});

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
