//! The `AV1CodecConfigurationRecord` (`av1C`) as this crate's AV1
//! layer reads it: the container crate's [`oxideav_heif::Av1Config`]
//! (AV1-ISOBMFF Binding §2.3 — marker / version, `seq_profile`,
//! `seq_level_idx_0`, `seq_tier_0`, `high_bitdepth`, `twelve_bit`,
//! `monochrome`, `chroma_subsampling_x` / `_y`,
//! `chroma_sample_position`, `initial_presentation_delay_minus_one`,
//! `configOBUs`, `bit_depth()`), with the parse error bridged into the
//! framework error type for the decoder.

use oxideav_core::{Error, Result};

/// The parsed `av1C` record.
pub(crate) type Av1CodecConfig = oxideav_heif::Av1Config;

/// Parse an `av1C` record body for the decoder.
pub(crate) fn parse_av1c(bytes: &[u8]) -> Result<Av1CodecConfig> {
    Av1CodecConfig::parse(bytes).map_err(|e| Error::invalid(e.to_string()))
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
        let cfg = parse_av1c(&bytes).expect("parse");
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
        let err = parse_av1c(&bytes).unwrap_err();
        assert!(err.to_string().contains("marker"));
    }

    #[test]
    fn av1c_rejects_wrong_version() {
        let bytes = [0x82, 0x00, 0x0c, 0x00];
        let err = parse_av1c(&bytes).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn av1c_carries_config_obus() {
        let bytes = [0x81, 0x00, 0x0c, 0x00, 0x0a, 0x0b, 0x0c];
        let cfg = parse_av1c(&bytes).expect("parse");
        assert_eq!(cfg.config_obus, vec![0x0a, 0x0b, 0x0c]);
    }

    #[test]
    fn av1c_bit_depth_derivation_covers_all_pairs() {
        let mut cfg = parse_av1c(&[0x81, 0x00, 0x0c, 0x00]).unwrap();
        assert_eq!(cfg.bit_depth(), 8);
        cfg.high_bitdepth = true;
        assert_eq!(cfg.bit_depth(), 10);
        cfg.twelve_bit = true;
        assert_eq!(cfg.bit_depth(), 12);
    }
}
