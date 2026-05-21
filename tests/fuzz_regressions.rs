//! Regression tests for fuzz-discovered crashes / divergences.
//!
//! Each fixture under `tests/fixtures/fuzz/` is either:
//!
//! * A short AVIF bitstream produced by the libavif-encoder-driven
//!   cross-decode fuzz harness, captured when the harness asserted a
//!   pixel divergence between `oxideav-avif`'s decoded planes and
//!   `libavif`'s decoded planes. The crash is **not** a panic — it's an
//!   `assert_eq!` failure inside the fuzz harness. The bug is in
//!   `oxideav-av1`'s decode path, not in AVIF parsing.
//! * A short AVIF bitstream that previously tripped an arithmetic
//!   overflow inside `oxideav-av1`'s coefficient decoder when fed an
//!   adversarial AV1 OBU stream. The fix in `oxideav-av1` 0.1.7 closes
//!   the path; this regression test pins the host behaviour so a
//!   future regression in either layer surfaces here first.
//!
//! In all cases the AVIF-side contract is **no panic, decode returns
//! `Ok(_)` or `Err(_)` cleanly**. These tests do *not* assert on pixel
//! correctness — the cross-decode fuzz harness is the authoritative
//! oracle for that, and its current divergence is tracked as a sibling
//! follow-up in `oxideav-av1` (see workspace task #786).

#![cfg(feature = "registry")]

use oxideav_avif::AvifDecoder;
use oxideav_core::{CodecId, Decoder, Packet, TimeBase};

/// AVIF bitstream (309 bytes) captured from the
/// `libavif_encode_oxideav_libavif_decode_match` fuzz harness on
/// 2026-05-11. Encoded by libavif from a 6-byte fuzz seed; decodes
/// cleanly through `oxideav-avif`'s container layer but the AV1 layer's
/// Y plane diverges from libavif's reference decode. The AVIF-side
/// regression contract is **must not panic**.
const Y_PLANE_DIVERGENCE_MATCH: &[u8] =
    include_bytes!("fixtures/fuzz/y_plane_divergence_match.avif");

/// First half of the `libavif_oxideav_reencode_roundtrip` fuzz divergence
/// (310 bytes) — the original libavif encode of the fuzz RGBA seed.
const Y_PLANE_ROUNDTRIP_AVIF1: &[u8] = include_bytes!("fixtures/fuzz/y_plane_roundtrip_avif1.avif");

/// Second half of the same round-trip fixture (297 bytes) — libavif's
/// re-encode of `oxideav-avif`'s decoded planes from `..avif1`. Decoding
/// this with `oxideav-avif` produces a Y plane that diverges from the
/// original decode.
const Y_PLANE_ROUNDTRIP_AVIF2: &[u8] = include_bytes!("fixtures/fuzz/y_plane_roundtrip_avif2.avif");

/// Run a packet through `AvifDecoder` and pull frames until the queue
/// drains. Any panic at any layer below us bubbles up as a test
/// failure — the contract this regression suite is enforcing.
fn drive(bytes: &[u8]) {
    let mut d = AvifDecoder::new(CodecId::new(oxideav_avif::CODEC_ID_STR));
    let pkt = Packet::new(0, TimeBase::new(1, 1), bytes.to_vec());
    // We deliberately tolerate Ok and Err here — the contract is the
    // absence of a panic, not decode success.
    if d.send_packet(&pkt).is_err() {
        return;
    }
    // Drain any frames the packet produced. `receive_frame` returns
    // `NeedMore` once empty.
    while d.receive_frame().is_ok() {}
}

#[test]
fn fuzz_y_plane_divergence_match_does_not_panic() {
    drive(Y_PLANE_DIVERGENCE_MATCH);
}

#[test]
fn fuzz_y_plane_roundtrip_avif1_does_not_panic() {
    drive(Y_PLANE_ROUNDTRIP_AVIF1);
}

#[test]
fn fuzz_y_plane_roundtrip_avif2_does_not_panic() {
    drive(Y_PLANE_ROUNDTRIP_AVIF2);
}

// `malformed_av1c_high_profile_is_rejected` integration test removed
// after the 2026-05-20 clean-room orphan rebuild of `oxideav-av1`
// stripped `Av1CodecConfig` from its public surface. The av1C
// structural parser now lives locally in `oxideav_avif::av1_stub`
// (crate-private); its rejection-path coverage is exercised by the
// unit tests in that module (`av1c_parses_minimal_record` /
// `av1c_rejects_wrong_marker` / `av1c_rejects_wrong_version` /
// `av1c_carries_config_obus`).
