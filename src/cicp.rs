//! CICP (Coding-Independent Code Points) — ITU-T H.273 colour signalling.
//!
//! AVIF carries colour information in two ways:
//!
//! 1. The `colr` item property of `colour_type == 'nclx'` — three CICP
//!    code points (`colour_primaries`, `transfer_characteristics`,
//!    `matrix_coefficients`) plus a `full_range_flag` bit. Spec:
//!    av1-avif §2.1, §4.1, §9.1.1; ISO/IEC 14496-12 §12.1.5
//!    (ColourInformationBox); ITU-T H.273 (CICP).
//!
//! 2. The AV1 sequence header — same four fields, mirrored by the bitstream
//!    itself. Per av1-avif §2.2.1, when an AV1 metadata OBU or item
//!    property carries the value, "the values shall match those of the
//!    Sequence Header OBU in the AV1 Image Item Data". So the `colr`
//!    property is the file's authoritative copy.
//!
//! AVIF decoders **do not** apply colour transforms to the decoded
//! pixels. Per av1-avif §4.2.3.1: "No color space conversion, matrix
//! coefficients, or transfer characteristics function shall be applied
//! to the input samples. They are already in the same color space as
//! the output samples."
//!
//! What this module provides is therefore signalling, not transforms:
//!
//! * [`CicpTriple`] — the resolved `(primaries, transfer, matrix,
//!   full_range)` quadruple, with proper defaults applied.
//! * [`effective_cicp`] — apply CICP `2` (Unspecified) defaults when
//!   `colr` is absent or missing fields.
//! * Code-point predicates ([`primaries_name`], [`transfer_name`],
//!   [`matrix_name`], plus `is_*_reserved`) for callers (display
//!   pipelines, ICC builders) that need to reason about the file's
//!   intended colour space.
//!
//! Per av1-avif §4.1: for AV1 Alpha Image Items the `colr` should be
//! omitted; if present, readers shall ignore it. We therefore expose
//! [`CicpTriple::for_alpha`] that returns the spec-mandated alpha
//! defaults regardless of any `colr` attached to the alpha auxiliary.
//!
//! Per av1-avif §4.1: for AV1 Auxiliary Image Items (alpha included),
//! `color_range` in the AV1 sequence header `shall be set to 1`. Our
//! [`CicpTriple::for_alpha`] reflects that.
//!
//! All code points follow ITU-T H.273 §8 (ColourPrimaries),
//! §8.2 (TransferCharacteristics) and §8.3 (MatrixCoefficients).

use crate::meta::Colr;

/// Effective CICP signalling quadruple for an image item, ready for
/// downstream consumers (display engines, colour-managed renderers,
/// PNG/JPEG transcoders that emit a CICP marker).
///
/// `None` is never a valid state once defaults are applied — every
/// AVIF image has an effective triple, even if it's the canonical
/// "Unspecified" `(2, 2, 2, false)`. Use [`effective_cicp`] to fold
/// `Option<&Colr>` into a `CicpTriple`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CicpTriple {
    /// `colour_primaries` per ITU-T H.273 §8.1. 1 = BT.709,
    /// 9 = BT.2020, 12 = Display P3, etc. Value `2` means "Unspecified".
    pub colour_primaries: u16,
    /// `transfer_characteristics` per ITU-T H.273 §8.2. 1 = BT.709,
    /// 13 = sRGB / IEC 61966-2-1, 16 = SMPTE ST 2084 (PQ),
    /// 18 = ARIB STD-B67 (HLG). Value `2` means "Unspecified".
    pub transfer_characteristics: u16,
    /// `matrix_coefficients` per ITU-T H.273 §8.3. 0 = Identity (RGB,
    /// YCgCo when paired with appropriate primaries), 1 = BT.709,
    /// 6 = BT.601, 9 = BT.2020 NCL. Value `2` means "Unspecified".
    pub matrix_coefficients: u16,
    /// `video_full_range_flag`. `false` → studio range (limited),
    /// `true` → full range (0..=255 / 0..=1023).
    pub full_range: bool,
}

impl CicpTriple {
    /// Default CICP signalling per ITU-T H.273: every code point set
    /// to `2` (Unspecified), `full_range = false`.
    ///
    /// Spec: av1-avif §2.1 (when `colr` is absent the file makes no
    /// claim about its colour space — the consumer chooses), ITU-T
    /// H.273 §8.1.1 (the value `2` is "Unspecified" and means the
    /// content is interpreted by the application using video
    /// system or other information).
    pub const UNSPECIFIED: Self = Self {
        colour_primaries: 2,
        transfer_characteristics: 2,
        matrix_coefficients: 2,
        full_range: false,
    };

    /// CICP triple for an AV1 Alpha Image Item.
    ///
    /// Per av1-avif §4.1, alpha auxiliaries shall encode `color_range = 1`
    /// in the AV1 sequence header (full range), and any `colr`
    /// attached to the alpha item shall be ignored. The primaries /
    /// transfer / matrix code points are not meaningful for a single-
    /// channel alpha image, so we surface them as `Unspecified` (2).
    pub const ALPHA: Self = Self {
        colour_primaries: 2,
        transfer_characteristics: 2,
        matrix_coefficients: 2,
        full_range: true,
    };

    /// CICP for an AVIF Alpha Image Item — equivalent to [`ALPHA`].
    /// Spelled out as a constructor for callers that prefer the verbose
    /// form.
    pub const fn for_alpha() -> Self {
        Self::ALPHA
    }

    /// True when the matrix indicates the AV1 stream stores RGB
    /// samples in identity matrix form (no Y'CbCr → R'G'B' conversion
    /// at decode time). ITU-T H.273 §8.3.1 — code point 0.
    ///
    /// AVIF `MA1A` (Advanced Profile) AVIFs commonly use this
    /// matrix for lossless 4:4:4 RGB.
    pub fn is_identity_matrix(&self) -> bool {
        self.matrix_coefficients == 0
    }

    /// True when every code point is the spec-defined "Unspecified"
    /// value (`2`). Decoders should fall back to a system-default
    /// interpretation in that case (typically: BT.709 + sRGB + BT.601
    /// for SDR 8-bit content).
    pub fn is_unspecified(&self) -> bool {
        self.colour_primaries == 2
            && self.transfer_characteristics == 2
            && self.matrix_coefficients == 2
    }

    /// True when ANY code point is in the ITU-T H.273 "Reserved" range.
    /// Reserved values must not be emitted by encoders; readers that
    /// see them should treat the field as Unspecified. Useful as a
    /// sanity check before passing the triple into a colour-conversion
    /// pipeline.
    pub fn has_reserved(&self) -> bool {
        is_primaries_reserved(self.colour_primaries)
            || is_transfer_reserved(self.transfer_characteristics)
            || is_matrix_reserved(self.matrix_coefficients)
    }

    /// True when the triple is the canonical libavif default for
    /// 8-bit SDR sRGB content: BT.709 primaries (1) + sRGB transfer
    /// (13) + BT.601 matrix (6). avifenc emits this triple for every
    /// SDR 4:2:0 / 4:2:2 input that doesn't override.
    pub fn is_libavif_srgb_default(&self) -> bool {
        self.colour_primaries == 1
            && self.transfer_characteristics == 13
            && self.matrix_coefficients == 6
    }
}

/// Apply CICP defaults to an optional `colr` property and return the
/// effective triple. The mapping:
///
/// * `Some(Colr::Nclx { .. })` → fields surfaced verbatim, with no
///   special case for individual `2` values (a partially-specified
///   `colr` carrying e.g. `(1, 13, 2)` keeps the `2` matrix as
///   "Unspecified" — H.273 lets each code point independently mean
///   "Unspecified").
/// * `Some(Colr::Icc(_))` or `Some(Colr::Unknown(_))` → the file
///   declares a non-CICP colour space (typically an embedded ICC
///   profile). We surface [`CicpTriple::UNSPECIFIED`] so downstream
///   consumers know the CICP triple isn't authoritative — they should
///   instead consult the ICC payload via `Colr::Icc`.
/// * `None` → [`CicpTriple::UNSPECIFIED`] (file makes no claim).
///
/// This helper does NOT attempt to derive CICP from the AV1 sequence
/// header. Per av1-avif §2.2.1 the `colr` property and the AV1
/// sequence header must agree if both are present, so it's enough for
/// AVIF callers to consult the property — and `oxideav-av1` doesn't
/// expose the sequence header CICP fields through its public API.
pub fn effective_cicp(colr: Option<&Colr>) -> CicpTriple {
    match colr {
        Some(Colr::Nclx {
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range,
        }) => CicpTriple {
            colour_primaries: *colour_primaries,
            transfer_characteristics: *transfer_characteristics,
            matrix_coefficients: *matrix_coefficients,
            full_range: *full_range,
        },
        Some(Colr::Icc(_)) | Some(Colr::Unknown(_)) | None => CicpTriple::UNSPECIFIED,
    }
}

/// Human-readable name of a `colour_primaries` code point per ITU-T
/// H.273 §8.1.1 Table 2. Returns `None` for reserved code points or
/// unknown values; callers can render `"Reserved"` / `"Unknown ({})"`
/// themselves.
pub fn primaries_name(code: u16) -> Option<&'static str> {
    match code {
        1 => Some("BT.709"),
        2 => Some("Unspecified"),
        4 => Some("BT.470 System M (NTSC 1953)"),
        5 => Some("BT.470 System B/G (PAL/SECAM)"),
        6 => Some("BT.601 525 (SMPTE 170M)"),
        7 => Some("SMPTE 240M"),
        8 => Some("Generic film (illuminant C)"),
        9 => Some("BT.2020 / BT.2100"),
        10 => Some("SMPTE ST 428-1 (CIE 1931 XYZ)"),
        11 => Some("SMPTE RP 431-2 (DCI P3)"),
        12 => Some("SMPTE EG 432-1 (Display P3)"),
        22 => Some("EBU Tech. 3213-E"),
        _ => None,
    }
}

/// Human-readable name of a `transfer_characteristics` code point per
/// ITU-T H.273 §8.2 Table 3.
pub fn transfer_name(code: u16) -> Option<&'static str> {
    match code {
        1 => Some("BT.709"),
        2 => Some("Unspecified"),
        4 => Some("BT.470 System M (gamma 2.2)"),
        5 => Some("BT.470 System B/G (gamma 2.8)"),
        6 => Some("BT.601"),
        7 => Some("SMPTE 240M"),
        8 => Some("Linear"),
        9 => Some("Logarithmic 100:1"),
        10 => Some("Logarithmic 100*sqrt(10):1"),
        11 => Some("IEC 61966-2-4 (xvYCC)"),
        12 => Some("BT.1361"),
        13 => Some("sRGB / IEC 61966-2-1"),
        14 => Some("BT.2020 10-bit"),
        15 => Some("BT.2020 12-bit"),
        16 => Some("SMPTE ST 2084 (PQ)"),
        17 => Some("SMPTE ST 428-1"),
        18 => Some("ARIB STD-B67 (HLG)"),
        _ => None,
    }
}

/// Human-readable name of a `matrix_coefficients` code point per ITU-T
/// H.273 §8.3 Table 4.
pub fn matrix_name(code: u16) -> Option<&'static str> {
    match code {
        0 => Some("Identity (RGB / YCgCo)"),
        1 => Some("BT.709"),
        2 => Some("Unspecified"),
        4 => Some("FCC 73.682 (US NTSC)"),
        5 => Some("BT.470 System B/G"),
        6 => Some("BT.601"),
        7 => Some("SMPTE 240M"),
        8 => Some("YCgCo"),
        9 => Some("BT.2020 NCL"),
        10 => Some("BT.2020 CL"),
        11 => Some("SMPTE ST 2085"),
        12 => Some("Chromaticity-derived NCL"),
        13 => Some("Chromaticity-derived CL"),
        14 => Some("ICtCp"),
        _ => None,
    }
}

/// True when the `colour_primaries` code point falls in the ITU-T
/// H.273 §8.1.1 reserved ranges (3, 13..=21, 23..=255). Reserved
/// values are not assignable by any registered specification.
pub fn is_primaries_reserved(code: u16) -> bool {
    matches!(code, 3 | 13..=21 | 23..=255) || code > 255
}

/// True when the `transfer_characteristics` code point falls in the
/// ITU-T H.273 §8.2 reserved range (3, 19..=255).
pub fn is_transfer_reserved(code: u16) -> bool {
    matches!(code, 3 | 19..=255) || code > 255
}

/// True when the `matrix_coefficients` code point falls in the ITU-T
/// H.273 §8.3 reserved range (3, 15..=255).
pub fn is_matrix_reserved(code: u16) -> bool {
    matches!(code, 3 | 15..=255) || code > 255
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `effective_cicp(None)` returns the spec-mandated Unspecified
    /// quadruple — `(2, 2, 2, false)`. ITU-T H.273 §8.1.1.
    #[test]
    fn effective_cicp_none_is_unspecified() {
        let triple = effective_cicp(None);
        assert_eq!(triple, CicpTriple::UNSPECIFIED);
        assert!(triple.is_unspecified());
        assert!(!triple.full_range);
    }

    /// `effective_cicp(Some(Nclx))` surfaces the parsed values as-is
    /// (no normalisation, no per-field default substitution).
    #[test]
    fn effective_cicp_nclx_passes_through() {
        let colr = Colr::Nclx {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 6,
            full_range: false,
        };
        let triple = effective_cicp(Some(&colr));
        assert_eq!(triple.colour_primaries, 1);
        assert_eq!(triple.transfer_characteristics, 13);
        assert_eq!(triple.matrix_coefficients, 6);
        assert!(!triple.full_range);
        assert!(triple.is_libavif_srgb_default());
    }

    /// An ICC payload (`Colr::Icc`) is not a CICP triple — `effective_cicp`
    /// must still return a usable quadruple, and `Unspecified` is the
    /// safest default (downstream consumers should consult the ICC
    /// profile body). av1-avif §9.1.2.
    #[test]
    fn effective_cicp_icc_falls_back_to_unspecified() {
        let colr = Colr::Icc(vec![0u8; 16]);
        let triple = effective_cicp(Some(&colr));
        assert_eq!(triple, CicpTriple::UNSPECIFIED);
    }

    /// An `Unknown` `colour_type` (anything other than nclx/rICC/prof)
    /// also folds to Unspecified.
    #[test]
    fn effective_cicp_unknown_colr_type_unspecified() {
        let colr = Colr::Unknown([b'x'; 4]);
        let triple = effective_cicp(Some(&colr));
        assert_eq!(triple, CicpTriple::UNSPECIFIED);
    }

    /// CICP names round-trip the canonical SDR / HDR triples. Spot
    /// check the full panel of values listed in ITU-T H.273.
    #[test]
    fn primaries_names_cover_canonical_triples() {
        assert_eq!(primaries_name(1), Some("BT.709"));
        assert_eq!(primaries_name(2), Some("Unspecified"));
        assert_eq!(primaries_name(9), Some("BT.2020 / BT.2100"));
        assert_eq!(primaries_name(12), Some("SMPTE EG 432-1 (Display P3)"));
        // Reserved range — no name.
        assert_eq!(primaries_name(3), None);
        assert_eq!(primaries_name(20), None);
        // Out-of-table — no name.
        assert_eq!(primaries_name(255), None);
    }

    #[test]
    fn transfer_names_cover_hdr_curves() {
        assert_eq!(transfer_name(13), Some("sRGB / IEC 61966-2-1"));
        assert_eq!(transfer_name(16), Some("SMPTE ST 2084 (PQ)"));
        assert_eq!(transfer_name(18), Some("ARIB STD-B67 (HLG)"));
        assert_eq!(transfer_name(2), Some("Unspecified"));
        assert_eq!(transfer_name(3), None); // reserved
    }

    #[test]
    fn matrix_names_cover_ycbcr_and_identity() {
        assert_eq!(matrix_name(0), Some("Identity (RGB / YCgCo)"));
        assert_eq!(matrix_name(1), Some("BT.709"));
        assert_eq!(matrix_name(6), Some("BT.601"));
        assert_eq!(matrix_name(9), Some("BT.2020 NCL"));
        assert_eq!(matrix_name(14), Some("ICtCp"));
        assert_eq!(matrix_name(15), None); // reserved
    }

    /// `is_*_reserved` predicates flag the right ranges per H.273.
    #[test]
    fn reserved_predicates_match_h273_ranges() {
        // Primaries: reserved = 3, 13..=21, 23..=255 (and >255).
        assert!(is_primaries_reserved(3));
        assert!(is_primaries_reserved(13));
        assert!(is_primaries_reserved(21));
        assert!(is_primaries_reserved(23));
        assert!(is_primaries_reserved(100));
        assert!(is_primaries_reserved(256));
        assert!(!is_primaries_reserved(1));
        assert!(!is_primaries_reserved(2));
        assert!(!is_primaries_reserved(9));
        assert!(!is_primaries_reserved(12));
        assert!(!is_primaries_reserved(22));
        // Transfer: reserved = 3, 19..=255 (and >255).
        assert!(is_transfer_reserved(3));
        assert!(is_transfer_reserved(19));
        assert!(is_transfer_reserved(100));
        assert!(is_transfer_reserved(256));
        assert!(!is_transfer_reserved(1));
        assert!(!is_transfer_reserved(13));
        assert!(!is_transfer_reserved(18));
        // Matrix: reserved = 3, 15..=255 (and >255).
        assert!(is_matrix_reserved(3));
        assert!(is_matrix_reserved(15));
        assert!(is_matrix_reserved(100));
        assert!(is_matrix_reserved(256));
        assert!(!is_matrix_reserved(0));
        assert!(!is_matrix_reserved(1));
        assert!(!is_matrix_reserved(14));
    }

    /// `is_unspecified` only true when ALL three code points are 2.
    /// A partially-specified triple (e.g., primaries=1 + matrix=2)
    /// counts as specified — H.273 lets each axis carry its own
    /// "Unspecified".
    #[test]
    fn is_unspecified_requires_all_three() {
        assert!(CicpTriple::UNSPECIFIED.is_unspecified());
        let partial = CicpTriple {
            colour_primaries: 1,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            full_range: false,
        };
        assert!(!partial.is_unspecified());
    }

    /// `is_identity_matrix` reflects matrix=0 (RGB identity / YCgCo-R)
    /// — the canonical lossless RGB AVIF stream marker.
    #[test]
    fn identity_matrix_flagged_for_rgb_advanced_profile() {
        let rgb = CicpTriple {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            full_range: true,
        };
        assert!(rgb.is_identity_matrix());
        assert!(!rgb.is_unspecified());
        let bt709 = CicpTriple {
            colour_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            full_range: false,
        };
        assert!(!bt709.is_identity_matrix());
    }

    /// `for_alpha` / `ALPHA` return Unspecified primaries+transfer+matrix
    /// with `full_range=true`, per av1-avif §4.1.
    #[test]
    fn alpha_cicp_carries_full_range() {
        let alpha = CicpTriple::for_alpha();
        assert_eq!(alpha, CicpTriple::ALPHA);
        assert!(alpha.full_range);
        assert_eq!(alpha.colour_primaries, 2);
        assert_eq!(alpha.transfer_characteristics, 2);
        assert_eq!(alpha.matrix_coefficients, 2);
    }

    /// `has_reserved` flips whenever any axis falls in a reserved range.
    #[test]
    fn has_reserved_detects_each_axis() {
        let mut t = CicpTriple {
            colour_primaries: 3, // reserved
            transfer_characteristics: 13,
            matrix_coefficients: 6,
            full_range: false,
        };
        assert!(t.has_reserved());
        t.colour_primaries = 1;
        t.transfer_characteristics = 19; // reserved
        assert!(t.has_reserved());
        t.transfer_characteristics = 13;
        t.matrix_coefficients = 15; // reserved
        assert!(t.has_reserved());
        t.matrix_coefficients = 6;
        assert!(!t.has_reserved());
    }

    /// `is_libavif_srgb_default` only matches the exact (1, 13, 6)
    /// triple regardless of `full_range`. A close-but-not-identical
    /// triple ((1, 13, 5) or BT.709 matrix) doesn't trip it.
    #[test]
    fn libavif_default_triple_check_strict() {
        let exact = CicpTriple {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 6,
            full_range: false,
        };
        assert!(exact.is_libavif_srgb_default());
        let close = CicpTriple {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 5,
            full_range: false,
        };
        assert!(!close.is_libavif_srgb_default());
    }
}
