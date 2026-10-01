//! Colour signalling of a decoded AVIF on the framework surface
//! (`oxideav-core` 0.1.37 [`ColorSignal`]) — registry-gated.
//!
//! An AVIF file is a MIAF file (av1-avif §7), so the colour of an
//! image item is the `colr` item property, never the bitstream's
//! `color_config` (MIAF §7.3.6.4 NOTE 1: *"Any colour information in
//! the bitstream is ignored by the MIAF reader and MIAF renderer
//! processing models. The colour information property whether
//! explicit or default, takes precedence"*). The default when a coded
//! image has no CICP property is `nclx` 1 / 13 / 5-or-6 with
//! `full_range_flag = 1` (MIAF §7.3.6.4) — so a file without `colr`
//! is **full range**, and a 10-bit still signalled full range must not
//! be read as limited downstream.
//!
//! The pixel-format label follows the container crate's reading: an
//! identity-matrix item (`matrix_coefficients = 0`, H.273 §8.3 —
//! Y = G, Cb = B, Cr = R) decoded in 4:4:4 is planar RGB (`Gbrp8` /
//! `Gbrp10Le` / `Gbrp12Le`, `Gbrap*` with alpha, plane order G B R),
//! a full-range 8-bit Y′CbCr still is one of the framework's `YuvJ*`
//! layouts, and everything else keeps its storage label with the
//! range on the [`ColorSignal`].

use oxideav_core::{ColorRange, ColorSignal, PixelFormat};

use crate::image::AvifPixelFormat;
use crate::meta::Colr;

/// The effective colour signal of an image item from its `colr`
/// (MIAF §7.3.6.4): the `nclx` code points as signalled, or the MIAF
/// default — BT.709 primaries (1), sRGB transfer (13), BT.601 matrix
/// (6), full range — when the item carries no CICP property (no
/// `colr`, or an ICC-only `colr`; the ICC profile itself stays on
/// [`crate::inspect::AvifInfo::colour`]).
pub fn color_signal_for(colr: Option<&Colr>) -> ColorSignal {
    match colr {
        Some(Colr::Nclx {
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range,
        }) => ColorSignal::from_code_points(
            clamp_code(*colour_primaries),
            clamp_code(*transfer_characteristics),
            clamp_code(*matrix_coefficients),
            *full_range,
        ),
        Some(Colr::Icc(_)) | Some(Colr::Unknown(_)) | None => miaf_default_signal(),
    }
}

/// MIAF §7.3.6.4's default colour property as a signal.
pub fn miaf_default_signal() -> ColorSignal {
    ColorSignal::from_code_points(1, 13, 6, true)
}

/// H.273 code points are 8-bit on the wire (`colr` stores them in 16
/// bits); a value past 255 is outside the table and reads as
/// unspecified (2).
fn clamp_code(v: u16) -> u8 {
    u8::try_from(v).unwrap_or(2)
}

/// Framework label for a decoded layout under its colour signal: the
/// planar-RGB `Gbrp*` / `Gbrap*` family for an identity-matrix 4:4:4
/// picture, the `YuvJ*` legacy full-range labels for 8-bit Y′CbCr
/// without alpha, the storage label otherwise.
pub fn labelled_pixel_format(format: AvifPixelFormat, signal: &ColorSignal) -> PixelFormat {
    let identity = signal.matrix.0 == 0;
    let full = signal.range == ColorRange::Full;
    match format {
        AvifPixelFormat::Yuv444P if identity => PixelFormat::Gbrp8,
        AvifPixelFormat::Yuva444P if identity => PixelFormat::Gbrap8,
        AvifPixelFormat::Yuv444P10Le if identity => PixelFormat::Gbrp10Le,
        AvifPixelFormat::Yuva444P10Le if identity => PixelFormat::Gbrap10Le,
        AvifPixelFormat::Yuv444P12Le if identity => PixelFormat::Gbrp12Le,
        AvifPixelFormat::Yuva444P12Le if identity => PixelFormat::Gbrap12Le,
        AvifPixelFormat::Yuv420P if full => PixelFormat::YuvJ420P,
        AvifPixelFormat::Yuv422P if full => PixelFormat::YuvJ422P,
        AvifPixelFormat::Yuv444P if full => PixelFormat::YuvJ444P,
        other => PixelFormat::from(other),
    }
}

/// The storage layout an AV1 stream decodes to, from its `av1C`
/// record — for the sequence path, whose frames come straight from
/// the AV1 decoder (no alpha composition).
pub(crate) fn layout_of_record(cfg: &crate::av1_config::Av1CodecConfig) -> Option<AvifPixelFormat> {
    AvifPixelFormat::from_layout(
        cfg.bit_depth(),
        u8::from(cfg.chroma_subsampling_x),
        u8::from(cfg.chroma_subsampling_y),
        cfg.monochrome,
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{ColorPrimaries, MatrixCoefficients, TransferCharacteristics};

    #[test]
    fn nclx_code_points_ride_through() {
        let s = color_signal_for(Some(&Colr::Nclx {
            colour_primaries: 9,
            transfer_characteristics: 16,
            matrix_coefficients: 9,
            full_range: true,
        }));
        assert_eq!(s.primaries, ColorPrimaries(9));
        assert_eq!(s.transfer, TransferCharacteristics(16));
        assert_eq!(s.matrix, MatrixCoefficients(9));
        assert_eq!(s.range, ColorRange::Full);
        let limited = color_signal_for(Some(&Colr::Nclx {
            colour_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            full_range: false,
        }));
        assert_eq!(limited, ColorSignal::bt709_limited());
    }

    #[test]
    fn absent_and_icc_take_the_miaf_default() {
        let d = miaf_default_signal();
        assert_eq!(color_signal_for(None), d);
        assert_eq!(color_signal_for(Some(&Colr::Icc(vec![0; 4]))), d);
        assert_eq!(color_signal_for(Some(&Colr::Unknown(*b"zzzz"))), d);
        assert_eq!(d.range, ColorRange::Full);
        assert_eq!(d.matrix, MatrixCoefficients(6));
        assert_eq!(d.primaries, ColorPrimaries(1));
        assert_eq!(d.transfer, TransferCharacteristics(13));
    }

    #[test]
    fn out_of_table_code_point_is_unspecified() {
        let s = color_signal_for(Some(&Colr::Nclx {
            colour_primaries: 300,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            full_range: true,
        }));
        assert_eq!(s.primaries, ColorPrimaries(2));
        assert_eq!(s.matrix, MatrixCoefficients(0));
    }

    #[test]
    fn identity_444_is_planar_rgb_and_full_8bit_is_yuvj() {
        let id = ColorSignal::srgb();
        assert_eq!(
            labelled_pixel_format(AvifPixelFormat::Yuv444P, &id),
            PixelFormat::Gbrp8
        );
        assert_eq!(
            labelled_pixel_format(AvifPixelFormat::Yuva444P, &id),
            PixelFormat::Gbrap8
        );
        assert_eq!(
            labelled_pixel_format(AvifPixelFormat::Yuv444P10Le, &id),
            PixelFormat::Gbrp10Le
        );
        assert_eq!(
            labelled_pixel_format(AvifPixelFormat::Yuva444P12Le, &id),
            PixelFormat::Gbrap12Le
        );
        // Identity on a subsampled layout is not RGB (AV1 forbids it,
        // the label stays the storage one).
        assert_eq!(
            labelled_pixel_format(AvifPixelFormat::Yuv420P, &id),
            PixelFormat::YuvJ420P
        );
        let bt601_full = ColorSignal::from_code_points(1, 13, 6, true);
        assert_eq!(
            labelled_pixel_format(AvifPixelFormat::Yuv444P, &bt601_full),
            PixelFormat::YuvJ444P
        );
        assert_eq!(
            labelled_pixel_format(AvifPixelFormat::Yuv420P10Le, &bt601_full),
            PixelFormat::Yuv420P10Le
        );
        let limited = ColorSignal::bt709_limited();
        assert_eq!(
            labelled_pixel_format(AvifPixelFormat::Yuv420P, &limited),
            PixelFormat::Yuv420P
        );
        assert_eq!(
            labelled_pixel_format(AvifPixelFormat::Gray8, &bt601_full),
            PixelFormat::Gray8
        );
    }
}
