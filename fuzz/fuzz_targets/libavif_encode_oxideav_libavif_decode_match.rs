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
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

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
                    if oy != g {
                        dump_diagnostic_bundle(
                            "libavif_encode_oxideav_libavif_decode_match",
                            data,
                            &encoded,
                            oxi_w,
                            oxi_h,
                            oxi_y.stride,
                            0,
                            0,
                            &oxi_y.data,
                            &[],
                            &[],
                            libavif_rgba.width,
                            libavif_rgba.height,
                            lib_pixels,
                        );
                    }
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
                    if oy != g || ou != b || ov != r {
                        dump_diagnostic_bundle(
                            "libavif_encode_oxideav_libavif_decode_match",
                            data,
                            &encoded,
                            oxi_w,
                            oxi_h,
                            oxi_y.stride,
                            oxi_u.stride,
                            oxi_v.stride,
                            &oxi_y.data,
                            &oxi_u.data,
                            &oxi_v.data,
                            libavif_rgba.width,
                            libavif_rgba.height,
                            lib_pixels,
                        );
                    }
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

/// Write a self-describing diagnostic bundle (`divergence.txt`,
/// `divergence.avif`, `divergence.oxi.planes`,
/// `divergence.libavif.rgba`) under
/// `fuzz/artifacts/diagnostics/<harness>/` so a CI run that ends in an
/// assertion failure ships the actual divergent bitstream as an
/// artifact alongside the libfuzzer `crash-*` input.
///
/// This addresses the env-divergence problem flagged by AVIF round-42:
/// the macOS libavif (1.4.x + aom 3.x) produces a different lossless
/// AV1 bitstream than the Linux libavif (1.0.4 + libgav1) used in CI.
/// Re-running the fuzz `crash-*` input on a developer's macOS host
/// silently passes because libavif emits a bitstream that oxideav-av1
/// happens to handle correctly. With the diagnostic bundle uploaded as
/// a CI artifact we recover the actual Linux-libavif AV1 stream that
/// triggers the divergence and can replay it directly against
/// `oxideav_av1::Av1Decoder` offline.
///
/// Only the FIRST divergence per fuzz invocation is dumped — once the
/// flag latches, subsequent calls are no-ops. The libfuzzer harness
/// abort on the assertion that follows ensures a single bundle per
/// process lifetime regardless.
#[allow(clippy::too_many_arguments)]
fn dump_diagnostic_bundle(
    harness: &str,
    fuzz_input: &[u8],
    encoded_avif: &[u8],
    oxi_w: u32,
    oxi_h: u32,
    luma_stride: usize,
    u_stride: usize,
    v_stride: usize,
    oxi_y: &[u8],
    oxi_u: &[u8],
    oxi_v: &[u8],
    lib_w: u32,
    lib_h: u32,
    lib_rgba: &[u8],
) {
    static DUMPED: AtomicBool = AtomicBool::new(false);
    if DUMPED.swap(true, Ordering::SeqCst) {
        return;
    }

    let mut dir = PathBuf::from("fuzz/artifacts/diagnostics");
    dir.push(harness);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    let avif_path = dir.join("divergence.avif");
    let _ = std::fs::write(&avif_path, encoded_avif);

    let input_path = dir.join("divergence.fuzz_input");
    let _ = std::fs::write(&input_path, fuzz_input);

    let planes_path = dir.join("divergence.oxi.planes");
    if let Ok(mut f) = std::fs::File::create(&planes_path) {
        // Tiny header: u32 width, u32 height, u32 luma_stride, u32
        // u_stride, u32 v_stride, u32 plane_count then concatenated
        // plane data. Little-endian. Plain enough to read back with a
        // 30-line Rust harness.
        let plane_count: u32 = if oxi_u.is_empty() && oxi_v.is_empty() {
            1
        } else {
            3
        };
        let _ = f.write_all(&oxi_w.to_le_bytes());
        let _ = f.write_all(&oxi_h.to_le_bytes());
        let _ = f.write_all(&(luma_stride as u32).to_le_bytes());
        let _ = f.write_all(&(u_stride as u32).to_le_bytes());
        let _ = f.write_all(&(v_stride as u32).to_le_bytes());
        let _ = f.write_all(&plane_count.to_le_bytes());
        let _ = f.write_all(oxi_y);
        let _ = f.write_all(oxi_u);
        let _ = f.write_all(oxi_v);
    }

    let lib_path = dir.join("divergence.libavif.rgba");
    if let Ok(mut f) = std::fs::File::create(&lib_path) {
        let _ = f.write_all(&lib_w.to_le_bytes());
        let _ = f.write_all(&lib_h.to_le_bytes());
        let _ = f.write_all(lib_rgba);
    }

    let txt_path = dir.join("divergence.txt");
    if let Ok(mut f) = std::fs::File::create(&txt_path) {
        let _ = writeln!(
            f,
            "harness:        {harness}\n\
             fuzz_input_len: {} bytes (saved to divergence.fuzz_input)\n\
             encoded_avif:   {} bytes (saved to divergence.avif)\n\
             oxi_w x oxi_h:  {oxi_w} x {oxi_h}\n\
             luma_stride:    {luma_stride}\n\
             u_stride:       {u_stride}\n\
             v_stride:       {v_stride}\n\
             plane_count:    {}\n\
             lib_w x lib_h:  {lib_w} x {lib_h}\n\
             lib_rgba_len:   {} bytes (saved to divergence.libavif.rgba)\n\
             \n\
             To replay against oxideav-av1 directly, extract the AV1\n\
             OBU stream from divergence.avif's `mdat` box and pass it\n\
             to Av1Decoder::send_packet — bypasses the AVIF container\n\
             so divergence is isolated to the AV1 layer.\n",
            fuzz_input.len(),
            encoded_avif.len(),
            if oxi_u.is_empty() { 1 } else { 3 },
            lib_rgba.len(),
        );
    }
}

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
