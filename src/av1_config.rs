//! `av1C` (AV1CodecConfigurationRecord) parsing — the ISO BMFF binding
//! record every `av01` item property carries.
//!
//! `Av1CodecConfig` lives here legitimately — it's a parser for the
//! ISO BMFF `av1C` configuration box, an ISO 14496-12 binding concern,
//! not the AV1 bitstream itself. The byte layout is documented in the
//! "AV1 Codec ISO Media File Format Binding" (av1-isobmff §2.3).
//!
//! The actual pixel decode is delegated to `oxideav_av1`'s registry
//! decoder (see [`crate::decoder`]); this module only validates and
//! surfaces the configuration record's fields.

use oxideav_core::{Error, Result};

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
#[allow(dead_code)] // fields surfaced for future container-side audits
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

    /// §5.5.2-derived bit depth (8 / 10 / 12) from the
    /// `high_bitdepth` / `twelve_bit` flag pair.
    pub fn bit_depth(&self) -> u8 {
        match (self.high_bitdepth, self.twelve_bit) {
            (false, _) => 8,
            (true, false) => 10,
            (true, true) => 12,
        }
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
        assert_eq!(cfg.bit_depth(), 8);
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
    fn av1c_bit_depth_derivation_covers_all_pairs() {
        let mut cfg = Av1CodecConfig::parse(&[0x81, 0x00, 0x0c, 0x00]).unwrap();
        assert_eq!(cfg.bit_depth(), 8);
        cfg.high_bitdepth = true;
        assert_eq!(cfg.bit_depth(), 10);
        cfg.twelve_bit = true;
        assert_eq!(cfg.bit_depth(), 12);
    }
}
