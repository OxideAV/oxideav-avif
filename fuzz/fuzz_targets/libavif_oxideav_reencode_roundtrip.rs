#![no_main]

//! Round-trip stability harness: take an oxideav-decoded AVIF, hand
//! its pixels back to libavif's encoder, decode the new bitstream
//! through oxideav-avif again, and assert pixels are stable across
//! the round-trip.
//!
//! ## Why this and not "oxideav encode + oxideav decode"
//!
//! The literal task spec for #304 round 2 calls for a self-roundtrip
//! (`fuzz-generated AVIF → decode → re-encode → decode again`). That
//! would require an oxideav AVIF encoder, which doesn't exist —
//! `oxideav_avif::make_encoder` returns `Error::Unsupported` because
//! writing AVIF needs an AV1 encoder and oxideav doesn't ship one
//! (see `lib.rs::make_encoder` and the round-2 README note).
//!
//! In its place this harness exercises the strongest property the
//! existing surface supports: oxideav-avif's decoder must produce
//! pixels that are stable under a re-encode by a different (libavif)
//! encoder. Concretely:
//!
//!   1. libavif encodes a fuzz-generated RGBA → AVIF₁ (lossless).
//!   2. oxideav-avif decodes AVIF₁ → YUV444 planes P₁.
//!   3. libavif re-encodes P₁ (converted back to RGBA via the
//!      IDENTITY matrix mapping V=R, Y=G, U=B) → AVIF₂ (lossless).
//!   4. oxideav-avif decodes AVIF₂ → YUV444 planes P₂.
//!   5. Assert P₁ == P₂ over the visible rectangle.
//!
//! If our decoder is bit-stable and libavif's encoder is
//! deterministic, P₁ and P₂ match exactly. A regression in our
//! decoder that emits slightly-different pixels for a slightly-
//! different input bitstream surfaces as a P₁ != P₂ assertion
//! failure on at least some fuzz inputs.
//!
//! ## Skip conditions
//!
//! * **libavif not installed** → return early. Same skip behaviour
//!   as the other libavif harnesses; CI without
//!   `apt install libavif-dev` / `brew install libavif` runs the
//!   binary without firing the assertions.
//! * **Any encode / decode step errors** → return. We're hunting for
//!   pixel divergence on inputs where the full chain succeeds, not
//!   crash detection (covered by other targets).

use libfuzzer_sys::fuzz_target;
use oxideav_avif::AvifDecoder;
use oxideav_avif_fuzz::libavif;
use oxideav_core::{CodecId, Decoder, Frame, Packet, TimeBase};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_WIDTH: usize = 64;
const MAX_PIXELS: usize = 4096;

fuzz_target!(|data: &[u8]| {
    if !libavif::available() {
        return;
    }

    let Some((width, height, rgba_in)) = image_from_fuzz_input(data) else {
        return;
    };

    // Step 1: libavif encode → AVIF₁.
    let Some(avif1) = libavif::encode_lossless_rgba(rgba_in, width, height) else {
        return;
    };

    // Step 2: oxideav decode → YUV444 planes P₁.
    let Some(p1) = oxideav_decode_yuv444(&avif1) else {
        return;
    };

    // Step 3: libavif re-encode P₁ (map back to RGBA via IDENTITY
    // matrix: R=V, G=Y, B=U) → AVIF₂.
    //
    // Use the visible rectangle from the original input: width ×
    // height. The AV1 coded frame may be padded to block alignment
    // (so p1.coded_w / p1.coded_h are >= width / height); re-encoding
    // the padded region would feed garbage padding bytes into the
    // re-encode step. Crop to the visible rect first.
    let needed = (width as usize) * (height as usize) * 4;
    let mut rgba_round = vec![0u8; needed];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let yi = y * p1.luma_stride + x;
            let ui = y * p1.u_stride + x;
            let vi = y * p1.v_stride + x;
            let oi = (y * (width as usize) + x) * 4;
            // IDENTITY: R=V, G=Y, B=U; alpha is implicit 0xFF (the
            // libavif encoder will ignore it under lossless RGB input).
            rgba_round[oi] = p1.v[vi];
            rgba_round[oi + 1] = p1.y[yi];
            rgba_round[oi + 2] = p1.u[ui];
            rgba_round[oi + 3] = 0xFF;
        }
    }
    let _ = needed;
    if rgba_round.len() < (width as usize) * (height as usize) * 4 {
        return;
    }
    let Some(avif2) = libavif::encode_lossless_rgba(&rgba_round, width, height) else {
        return;
    };

    // Step 4: oxideav decode AVIF₂ → P₂.
    let Some(p2) = oxideav_decode_yuv444(&avif2) else {
        return;
    };

    // Step 5: assert P₁ == P₂ across the visible rectangle.
    for yy in 0..height as usize {
        for xx in 0..width as usize {
            let i1 = yy * p1.luma_stride + xx;
            let i2 = yy * p2.luma_stride + xx;
            let u1 = yy * p1.u_stride + xx;
            let u2 = yy * p2.u_stride + xx;
            let v1 = yy * p1.v_stride + xx;
            let v2 = yy * p2.v_stride + xx;
            if p1.y[i1] != p2.y[i2] || p1.u[u1] != p2.u[u2] || p1.v[v1] != p2.v[v2] {
                dump_roundtrip_diagnostic(data, &avif1, &avif2, &p1, &p2);
            }
            assert_eq!(
                p1.y[i1], p2.y[i2],
                "Y plane unstable under round-trip at ({xx},{yy})"
            );
            assert_eq!(
                p1.u[u1], p2.u[u2],
                "U plane unstable under round-trip at ({xx},{yy})"
            );
            assert_eq!(
                p1.v[v1], p2.v[v2],
                "V plane unstable under round-trip at ({xx},{yy})"
            );
        }
    }
});

/// Persist both round-trip AVIF bitstreams + their decoded YUV planes to
/// `fuzz/artifacts/diagnostics/libavif_oxideav_reencode_roundtrip/`
/// before the libfuzzer assertion fires the process abort. CI then
/// uploads them as artifacts so we recover the actual Linux-libavif
/// encoded streams that triggered the round-trip instability — the
/// macOS libavif silently encodes differently and reproduction-on-host
/// fails (see AVIF round-42 notes).
///
/// Latched: only the FIRST mismatch per process is dumped.
fn dump_roundtrip_diagnostic(
    fuzz_input: &[u8],
    avif1: &[u8],
    avif2: &[u8],
    p1: &DecodedYuv444,
    p2: &DecodedYuv444,
) {
    static DUMPED: AtomicBool = AtomicBool::new(false);
    if DUMPED.swap(true, Ordering::SeqCst) {
        return;
    }
    let dir = PathBuf::from("fuzz/artifacts/diagnostics/libavif_oxideav_reencode_roundtrip");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(dir.join("divergence.fuzz_input"), fuzz_input);
    let _ = std::fs::write(dir.join("divergence.avif1"), avif1);
    let _ = std::fs::write(dir.join("divergence.avif2"), avif2);

    fn write_planes(path: PathBuf, p: &DecodedYuv444) {
        if let Ok(mut f) = std::fs::File::create(path) {
            let _ = f.write_all(&(p.luma_stride as u32).to_le_bytes());
            let _ = f.write_all(&(p.u_stride as u32).to_le_bytes());
            let _ = f.write_all(&(p.v_stride as u32).to_le_bytes());
            let _ = f.write_all(&p.y);
            let _ = f.write_all(&p.u);
            let _ = f.write_all(&p.v);
        }
    }
    write_planes(dir.join("divergence.p1.planes"), p1);
    write_planes(dir.join("divergence.p2.planes"), p2);

    if let Ok(mut f) = std::fs::File::create(dir.join("divergence.txt")) {
        let _ = writeln!(
            f,
            "harness:        libavif_oxideav_reencode_roundtrip\n\
             fuzz_input_len: {} bytes (saved to divergence.fuzz_input)\n\
             avif1_len:      {} bytes (saved to divergence.avif1; libavif encode of fuzz RGBA)\n\
             avif2_len:      {} bytes (saved to divergence.avif2; libavif re-encode of P1's planes)\n\
             p1 planes:      luma_stride={} u_stride={} v_stride={} (saved to divergence.p1.planes)\n\
             p2 planes:      luma_stride={} u_stride={} v_stride={} (saved to divergence.p2.planes)\n\
             \n\
             Round-trip is unstable: oxideav-avif decoded avif1 -> P1,\n\
             libavif re-encoded P1 (RGBA via R=V/G=Y/B=U) -> avif2,\n\
             oxideav-avif decoded avif2 -> P2, but P1 != P2.\n\
             The bug is in oxideav-av1's decode path (avif is just the\n\
             container around AV1).\n",
            fuzz_input.len(),
            avif1.len(),
            avif2.len(),
            p1.luma_stride, p1.u_stride, p1.v_stride,
            p2.luma_stride, p2.u_stride, p2.v_stride,
        );
    }
}

/// Tightly-packed YUV444 planes returned by [`oxideav_decode_yuv444`].
/// `*_stride` is the source-buffer stride (may exceed plane width when
/// libavif pads to AV1 block alignment); the data buffers are the
/// original (untrimmed) Vec<u8>.
struct DecodedYuv444 {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    luma_stride: usize,
    u_stride: usize,
    v_stride: usize,
}

/// Decode an AVIF buffer with oxideav-avif and check that it landed
/// in YUV444 (3 planes, all the same stride). Returns `None` for any
/// other layout (Gray8 / 4:2:0 / 4:2:2) so the caller can skip the
/// fuzz iteration cleanly — the round-trip harness only makes sense
/// for the YUV444 contract libavif's `lossless=1` enforces.
fn oxideav_decode_yuv444(avif: &[u8]) -> Option<DecodedYuv444> {
    let mut d = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), avif.to_vec());
    d.send_packet(&pkt).ok()?;
    let frame = d.receive_frame().ok()?;
    let vf = match frame {
        Frame::Video(v) => v,
        _ => return None,
    };
    if vf.planes.len() != 3 {
        return None;
    }
    let y = &vf.planes[0];
    let u = &vf.planes[1];
    let v = &vf.planes[2];
    if y.stride != u.stride || y.stride != v.stride || y.stride == 0 {
        // Subsampled layout — re-encode mapping doesn't apply.
        return None;
    }
    Some(DecodedYuv444 {
        y: y.data.clone(),
        u: u.data.clone(),
        v: v.data.clone(),
        luma_stride: y.stride,
        u_stride: u.stride,
        v_stride: v.stride,
    })
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
