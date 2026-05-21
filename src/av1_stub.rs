//! Local stubs for the `oxideav-av1` API surface that AVIF historically
//! pulled in. After `oxideav-av1`'s 2026-05-20 clean-room orphan rebuild,
//! the old decoder API (`Av1CodecConfig`, `Av1Decoder`, `register_codecs`)
//! is gone pending re-implementation across many rounds. AVIF's container
//! parse / grid composition / alpha compositing all still work; only the
//! actual AV1 pixel decode path is gated off.
//!
//! `Av1CodecConfig` lives here legitimately — it's a parser for the
//! ISO BMFF `av1C` configuration box, an ISO 14496-12 binding concern,
//! not the AV1 bitstream itself. The byte layout is documented in the
//! "AV1 Codec ISO Media File Format Binding" (av1-isobmff §2.3).
//!
//! `Av1Decoder` is a stub that always surfaces `Error::Unsupported` so the
//! `Decoder::send_packet` / `receive_frame` trait contract is honoured —
//! the framework consumer gets a clear "pixel decode pending" signal
//! rather than a hard build failure on a workspace-wide compile.

use oxideav_core::frame::VideoFrame;
use oxideav_core::{CodecParameters, Error, Frame, Packet, Result};

/// AV1 Codec Configuration Box (`av1C`) per av1-isobmff §2.3.
///
/// 4 fixed bytes plus optional `configOBUs` payload.
///
/// ```text
/// byte 0: marker(1) | version(7)               // marker=1, version=1
/// byte 1: seq_profile(3) | seq_level_idx_0(5)
/// byte 2: seq_tier_0(1) | high_bitdepth(1) | twelve_bit(1)
///       | monochrome(1) | chroma_subsampling_x(1)
///       | chroma_subsampling_y(1) | chroma_sample_position(2)
/// byte 3: reserved(3) | initial_presentation_delay_present(1)
///       | initial_presentation_delay_minus_one(4)
///       OR reserved(3) | 0 | reserved(4)
/// configOBUs: byte 4..
/// ```
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields surfaced for a future AV1 decoder consumer
pub(crate) struct Av1CodecConfig {
    pub seq_profile: u8,
    pub seq_level_idx_0: u8,
    pub seq_tier_0: bool,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub chroma_subsampling_x: bool,
    pub chroma_subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub initial_presentation_delay_present: bool,
    pub initial_presentation_delay_minus_one: u8,
    pub config_obus: Vec<u8>,
}

impl Av1CodecConfig {
    /// Parse the 4-byte fixed header + optional `configOBUs` payload.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 4 {
            return Err(Error::invalid(format!(
                "av1C: configuration record requires at least 4 bytes, got {}",
                bytes.len()
            )));
        }
        let b0 = bytes[0];
        let marker = (b0 >> 7) & 0x1;
        let version = b0 & 0x7F;
        if marker != 1 {
            return Err(Error::invalid(format!(
                "av1C: marker bit must be 1, got {marker}"
            )));
        }
        if version != 1 {
            return Err(Error::invalid(format!(
                "av1C: version must be 1, got {version}"
            )));
        }
        let b1 = bytes[1];
        let seq_profile = (b1 >> 5) & 0x7;
        let seq_level_idx_0 = b1 & 0x1F;
        let b2 = bytes[2];
        let seq_tier_0 = (b2 >> 7) & 0x1 != 0;
        let high_bitdepth = (b2 >> 6) & 0x1 != 0;
        let twelve_bit = (b2 >> 5) & 0x1 != 0;
        let monochrome = (b2 >> 4) & 0x1 != 0;
        let chroma_subsampling_x = (b2 >> 3) & 0x1 != 0;
        let chroma_subsampling_y = (b2 >> 2) & 0x1 != 0;
        let chroma_sample_position = b2 & 0x3;
        let b3 = bytes[3];
        let initial_presentation_delay_present = (b3 >> 4) & 0x1 != 0;
        let initial_presentation_delay_minus_one = if initial_presentation_delay_present {
            b3 & 0xF
        } else {
            0
        };
        Ok(Self {
            seq_profile,
            seq_level_idx_0,
            seq_tier_0,
            high_bitdepth,
            twelve_bit,
            monochrome,
            chroma_subsampling_x,
            chroma_subsampling_y,
            chroma_sample_position,
            initial_presentation_delay_present,
            initial_presentation_delay_minus_one,
            config_obus: bytes[4..].to_vec(),
        })
    }
}

/// Stub `Av1Decoder` — keeps `oxideav-avif`'s registry surface alive
/// during the period the clean-room rebuild of `oxideav-av1` has no
/// pixel-decode path. Every packet/frame call returns
/// `Error::Unsupported`; consumers can detect this and fall back to a
/// different AV1 backend (a future HW-accel bridge or external bind).
#[derive(Debug)]
pub(crate) struct Av1Decoder {
    _params: CodecParameters,
}

impl Av1Decoder {
    pub fn new(params: CodecParameters) -> Self {
        Self { _params: params }
    }

    pub fn send_packet(&mut self, _pkt: &Packet) -> Result<()> {
        Err(Error::unsupported(
            "avif: AV1 decoder unavailable — oxideav-av1 clean-room rebuild pending pixel-decode implementation",
        ))
    }

    pub fn receive_frame(&mut self) -> Result<Frame> {
        Err(Error::unsupported(
            "avif: AV1 decoder unavailable — oxideav-av1 clean-room rebuild pending pixel-decode implementation",
        ))
    }

    #[allow(dead_code)]
    pub fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

#[allow(dead_code)]
pub(crate) fn frame_as_video(f: Frame) -> Result<VideoFrame> {
    match f {
        Frame::Video(v) => Ok(v),
        other => Err(Error::unsupported(format!(
            "avif: AV1 decoder returned non-video frame: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1c_parses_minimal_record() {
        // byte 0: marker=1 version=1 → 0x81
        // byte 1: seq_profile(3)=0, seq_level_idx_0(5)=12 → 0x0c
        // byte 2: chroma 4:2:0 (sub_x=1, sub_y=1) → 0x0c
        // byte 3: no presentation delay → 0x00
        let bytes = [0x81, 0x0c, 0x0c, 0x00];
        let cfg = Av1CodecConfig::parse(&bytes).expect("parse");
        assert_eq!(cfg.seq_profile, 0);
        assert_eq!(cfg.seq_level_idx_0, 12);
        assert!(cfg.chroma_subsampling_x);
        assert!(cfg.chroma_subsampling_y);
        assert!(!cfg.high_bitdepth);
        assert!(!cfg.monochrome);
        assert_eq!(cfg.config_obus.len(), 0);
    }

    #[test]
    fn av1c_rejects_wrong_marker() {
        let bytes = [0x01, 0x00, 0x0c, 0x00];
        let err = Av1CodecConfig::parse(&bytes).unwrap_err();
        assert!(err.to_string().contains("marker"));
    }

    #[test]
    fn av1c_rejects_wrong_version() {
        let bytes = [0x82, 0x00, 0x0c, 0x00];
        let err = Av1CodecConfig::parse(&bytes).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn av1c_carries_config_obus() {
        let bytes = [0x81, 0x00, 0x0c, 0x00, 0x0a, 0x0b, 0x0c];
        let cfg = Av1CodecConfig::parse(&bytes).expect("parse");
        assert_eq!(cfg.config_obus, vec![0x0a, 0x0b, 0x0c]);
    }

    #[test]
    fn decoder_stub_returns_unsupported() {
        let params = CodecParameters::video(oxideav_core::CodecId::new("av1"));
        let mut dec = Av1Decoder::new(params);
        let pkt = Packet::new(0, oxideav_core::TimeBase::new(1, 1), vec![]);
        assert!(dec.send_packet(&pkt).is_err());
        assert!(dec.receive_frame().is_err());
    }
}
