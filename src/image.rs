//! Crate-local frame / pixel-format types.
//!
//! Mirrors the `oxideav_core::frame::{VideoFrame, VideoPlane}` +
//! `oxideav_core::PixelFormat` surface the avif crate touches, with no
//! framework dependency. With the default-on `registry` feature these
//! types convert to / from the framework counterparts via
//! [`crate::registry`]; with the feature off they are the only image
//! representation the public API exposes.
//!
//! Only the variants the AV1 bitstream + AVIF post-processing pipeline
//! actually emit are modelled. The composition path (grid / alpha /
//! transform) consumes and produces these types directly so it stays
//! framework-free.

/// One plane of a planar video frame.
///
/// Mirrors `oxideav_core::frame::VideoPlane`: tightly-strided when
/// `stride == plane_width`, but the composition path tolerates any
/// `stride >= plane_width`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AvifPlane {
    /// Stride in bytes between adjacent rows. For 8-bit planes this
    /// equals the plane width when tightly packed.
    pub stride: usize,
    /// Plane data — `stride * row_count` bytes for tightly-strided
    /// planes; the trailing bytes of each row beyond the plane width
    /// are padding when `stride > plane_width`.
    pub data: Vec<u8>,
}

/// One decoded video frame, planar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AvifFrame {
    /// Presentation timestamp in the source TimeBase, when known.
    pub pts: Option<i64>,
    /// One [`AvifPlane`] per channel — single plane for monochrome,
    /// three for planar YUV, four for YUV+alpha.
    pub planes: Vec<AvifPlane>,
}

/// Pixel layout — only the variants the AV1-decoded primary item +
/// AVIF composition path actually emit.
///
/// The `*10Le` / `*12Le` variants carry each sample as a little-endian
/// 16-bit word using the low 10 / 12 bits (top bits zero) — the exact
/// layout the AV1 decode emits for a `high_bitdepth` stream (av1C
/// §5.5.2 flag pair) and the storage the matching `oxideav-core`
/// formats declare. Plane `stride` / `data` stay byte-addressed, so an
/// HBD plane's stride is `2 × plane_width`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvifPixelFormat {
    /// 8-bit planar 4:2:0 — what AV1's intra path emits for a typical
    /// AVIF colour image.
    Yuv420P,
    /// 8-bit planar 4:2:2.
    Yuv422P,
    /// 8-bit planar 4:4:4.
    Yuv444P,
    /// 8-bit single-plane greyscale (4:0:0).
    Gray8,
    /// 8-bit planar 4:2:0 with a full-resolution alpha plane (Y, U, V,
    /// A). Produced by [`crate::alpha::composite_alpha`] when the
    /// primary item carries an alpha auxiliary.
    Yuva420P,
    /// 8-bit planar 4:2:2 with a full-resolution alpha plane.
    Yuva422P,
    /// 8-bit planar 4:4:4 with a full-resolution alpha plane. The
    /// identity-matrix (`colr` `nclx` `matrix_coefficients = 0`) RGBA
    /// encode path decodes back to this layout.
    Yuva444P,
    /// 8-bit packed Y A interleaved. Produced by
    /// [`crate::alpha::composite_alpha`] when the colour primary is
    /// already monochrome and an alpha auxiliary is attached.
    Ya8,
    /// 10-bit planar 4:2:0, 16-bit LE storage.
    Yuv420P10Le,
    /// 10-bit planar 4:2:2, 16-bit LE storage.
    Yuv422P10Le,
    /// 10-bit planar 4:4:4, 16-bit LE storage.
    Yuv444P10Le,
    /// 12-bit planar 4:2:0, 16-bit LE storage.
    Yuv420P12Le,
    /// 12-bit planar 4:2:2, 16-bit LE storage.
    Yuv422P12Le,
    /// 12-bit planar 4:4:4, 16-bit LE storage.
    Yuv444P12Le,
    /// 10-bit single-plane greyscale (4:0:0), 16-bit LE storage.
    Gray10Le,
    /// 12-bit single-plane greyscale (4:0:0), 16-bit LE storage.
    Gray12Le,
    /// 10-bit planar 4:2:0 + full-resolution alpha, 16-bit LE storage.
    Yuva420P10Le,
    /// 10-bit planar 4:2:2 + full-resolution alpha, 16-bit LE storage.
    Yuva422P10Le,
    /// 10-bit planar 4:4:4 + full-resolution alpha, 16-bit LE storage.
    Yuva444P10Le,
    /// 12-bit planar 4:2:0 + full-resolution alpha, 16-bit LE storage.
    Yuva420P12Le,
    /// 12-bit planar 4:2:2 + full-resolution alpha, 16-bit LE storage.
    Yuva422P12Le,
    /// 12-bit planar 4:4:4 + full-resolution alpha, 16-bit LE storage.
    Yuva444P12Le,
    /// Packed 16-bit LE Y A interleaved (4 bytes per pixel). Produced
    /// by [`crate::alpha::composite_alpha`] for a 10/12-bit monochrome
    /// master + same-depth alpha; the samples keep their coded 10/12-bit
    /// values in the low bits (the decoder surfaces the effective depth
    /// via the `oxideav-core` per-plane significant-bits side channel).
    Ya16Le,
}

impl AvifPixelFormat {
    /// Number of planes the format ships — single plane for the gray
    /// and packed-YA layouts, three for planar YUV, four for the
    /// `Yuva*` layouts (Y + U + V + A).
    pub fn plane_count(&self) -> usize {
        match self {
            Self::Gray8 | Self::Ya8 | Self::Gray10Le | Self::Gray12Le | Self::Ya16Le => 1,
            Self::Yuv420P
            | Self::Yuv422P
            | Self::Yuv444P
            | Self::Yuv420P10Le
            | Self::Yuv422P10Le
            | Self::Yuv444P10Le
            | Self::Yuv420P12Le
            | Self::Yuv422P12Le
            | Self::Yuv444P12Le => 3,
            Self::Yuva420P
            | Self::Yuva422P
            | Self::Yuva444P
            | Self::Yuva420P10Le
            | Self::Yuva422P10Le
            | Self::Yuva444P10Le
            | Self::Yuva420P12Le
            | Self::Yuva422P12Le
            | Self::Yuva444P12Le => 4,
        }
    }

    /// Storage bytes per sample: 1 for the 8-bit variants, 2 for every
    /// 16-bit-LE-stored variant.
    pub fn bytes_per_sample(&self) -> usize {
        match self {
            Self::Yuv420P
            | Self::Yuv422P
            | Self::Yuv444P
            | Self::Gray8
            | Self::Yuva420P
            | Self::Yuva422P
            | Self::Yuva444P
            | Self::Ya8 => 1,
            _ => 2,
        }
    }

    /// Coded sample bit depth: 8, 10 or 12. [`Self::Ya16Le`] reports
    /// its 16-bit storage width — the effective coded depth of a
    /// composited monochrome+alpha frame rides on the significant-bits
    /// side channel instead (both components are 10 or 12 bits, the
    /// av1-avif §4.1 same-depth `shall`).
    pub fn bit_depth(&self) -> u8 {
        match self {
            Self::Yuv420P
            | Self::Yuv422P
            | Self::Yuv444P
            | Self::Gray8
            | Self::Yuva420P
            | Self::Yuva422P
            | Self::Yuva444P
            | Self::Ya8 => 8,
            Self::Yuv420P10Le
            | Self::Yuv422P10Le
            | Self::Yuv444P10Le
            | Self::Gray10Le
            | Self::Yuva420P10Le
            | Self::Yuva422P10Le
            | Self::Yuva444P10Le => 10,
            Self::Yuv420P12Le
            | Self::Yuv422P12Le
            | Self::Yuv444P12Le
            | Self::Gray12Le
            | Self::Yuva420P12Le
            | Self::Yuva422P12Le
            | Self::Yuva444P12Le => 12,
            Self::Ya16Le => 16,
        }
    }

    /// `(horizontal, vertical)` chroma-subsampling shifts. `0` means no
    /// subsampling on that axis; gray / packed-YA layouts report
    /// `(0, 0)` (they carry no chroma planes).
    pub fn chroma_subsampling(&self) -> (u8, u8) {
        match self {
            Self::Yuv420P
            | Self::Yuva420P
            | Self::Yuv420P10Le
            | Self::Yuv420P12Le
            | Self::Yuva420P10Le
            | Self::Yuva420P12Le => (1, 1),
            Self::Yuv422P
            | Self::Yuva422P
            | Self::Yuv422P10Le
            | Self::Yuv422P12Le
            | Self::Yuva422P10Le
            | Self::Yuva422P12Le => (1, 0),
            _ => (0, 0),
        }
    }

    /// `true` for the two packed Y-A interleaved layouts (`Ya8` /
    /// `Ya16Le`) whose single plane carries two samples per pixel.
    pub fn is_packed_ya(&self) -> bool {
        matches!(self, Self::Ya8 | Self::Ya16Le)
    }

    /// `true` when the format carries an alpha channel (either the
    /// `Yuva*` plane-3 layouts or the packed YA layouts).
    pub fn has_alpha(&self) -> bool {
        self.is_packed_ya() || self.plane_count() == 4
    }

    /// The alpha-composited companion of an alpha-less colour layout:
    /// `Yuv* → Yuva*` (same depth / subsampling) and `Gray8 → Ya8` /
    /// `Gray10Le`/`Gray12Le → Ya16Le`. Returns `None` for layouts that
    /// already carry alpha.
    pub fn with_alpha(&self) -> Option<Self> {
        match self {
            Self::Yuv420P => Some(Self::Yuva420P),
            Self::Yuv422P => Some(Self::Yuva422P),
            Self::Yuv444P => Some(Self::Yuva444P),
            Self::Gray8 => Some(Self::Ya8),
            Self::Yuv420P10Le => Some(Self::Yuva420P10Le),
            Self::Yuv422P10Le => Some(Self::Yuva422P10Le),
            Self::Yuv444P10Le => Some(Self::Yuva444P10Le),
            Self::Yuv420P12Le => Some(Self::Yuva420P12Le),
            Self::Yuv422P12Le => Some(Self::Yuva422P12Le),
            Self::Yuv444P12Le => Some(Self::Yuva444P12Le),
            Self::Gray10Le | Self::Gray12Le => Some(Self::Ya16Le),
            _ => None,
        }
    }
}

// ---- Framework-bridge conversions, gated behind `registry` ----
//
// When the `registry` feature is on the framework `oxideav_core` types
// are in scope; provide `From` conversions so callers can fluently move
// frames between the framework decoder surface and the crate-local
// composition layer. These are the same conversions the registry-side
// `decoder` module performs internally, exposed publicly for test code
// and external integrators that mix both worlds.

#[cfg(feature = "registry")]
impl From<AvifPlane> for oxideav_core::frame::VideoPlane {
    fn from(p: AvifPlane) -> Self {
        oxideav_core::frame::VideoPlane {
            stride: p.stride,
            data: p.data,
        }
    }
}

#[cfg(feature = "registry")]
impl From<oxideav_core::frame::VideoPlane> for AvifPlane {
    fn from(p: oxideav_core::frame::VideoPlane) -> Self {
        AvifPlane {
            stride: p.stride,
            data: p.data,
        }
    }
}

#[cfg(feature = "registry")]
impl From<AvifFrame> for oxideav_core::frame::VideoFrame {
    fn from(af: AvifFrame) -> Self {
        oxideav_core::frame::VideoFrame {
            pts: af.pts,
            planes: af.planes.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(feature = "registry")]
impl From<oxideav_core::frame::VideoFrame> for AvifFrame {
    fn from(vf: oxideav_core::frame::VideoFrame) -> Self {
        AvifFrame {
            pts: vf.pts,
            planes: vf.planes.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(feature = "registry")]
impl From<AvifPixelFormat> for oxideav_core::PixelFormat {
    fn from(fmt: AvifPixelFormat) -> Self {
        match fmt {
            AvifPixelFormat::Yuv420P => oxideav_core::PixelFormat::Yuv420P,
            AvifPixelFormat::Yuv422P => oxideav_core::PixelFormat::Yuv422P,
            AvifPixelFormat::Yuv444P => oxideav_core::PixelFormat::Yuv444P,
            AvifPixelFormat::Gray8 => oxideav_core::PixelFormat::Gray8,
            AvifPixelFormat::Yuva420P => oxideav_core::PixelFormat::Yuva420P,
            AvifPixelFormat::Yuva422P => oxideav_core::PixelFormat::Yuva422P,
            AvifPixelFormat::Yuva444P => oxideav_core::PixelFormat::Yuva444P,
            AvifPixelFormat::Ya8 => oxideav_core::PixelFormat::Ya8,
            AvifPixelFormat::Yuv420P10Le => oxideav_core::PixelFormat::Yuv420P10Le,
            AvifPixelFormat::Yuv422P10Le => oxideav_core::PixelFormat::Yuv422P10Le,
            AvifPixelFormat::Yuv444P10Le => oxideav_core::PixelFormat::Yuv444P10Le,
            AvifPixelFormat::Yuv420P12Le => oxideav_core::PixelFormat::Yuv420P12Le,
            AvifPixelFormat::Yuv422P12Le => oxideav_core::PixelFormat::Yuv422P12Le,
            AvifPixelFormat::Yuv444P12Le => oxideav_core::PixelFormat::Yuv444P12Le,
            AvifPixelFormat::Gray10Le => oxideav_core::PixelFormat::Gray10Le,
            AvifPixelFormat::Gray12Le => oxideav_core::PixelFormat::Gray12Le,
            AvifPixelFormat::Yuva420P10Le => oxideav_core::PixelFormat::Yuva420P10Le,
            AvifPixelFormat::Yuva422P10Le => oxideav_core::PixelFormat::Yuva422P10Le,
            AvifPixelFormat::Yuva444P10Le => oxideav_core::PixelFormat::Yuva444P10Le,
            AvifPixelFormat::Yuva420P12Le => oxideav_core::PixelFormat::Yuva420P12Le,
            AvifPixelFormat::Yuva422P12Le => oxideav_core::PixelFormat::Yuva422P12Le,
            AvifPixelFormat::Yuva444P12Le => oxideav_core::PixelFormat::Yuva444P12Le,
            AvifPixelFormat::Ya16Le => oxideav_core::PixelFormat::Ya16Le,
        }
    }
}

/// Bridge a framework [`oxideav_core::PixelFormat`] back into the
/// crate-local [`AvifPixelFormat`]. Only the variants the AVIF
/// pipeline emits are handled; anything else (packed RGB, audio
/// formats wedged into the enum) returns an [`AvifError`].
#[cfg(feature = "registry")]
impl TryFrom<oxideav_core::PixelFormat> for AvifPixelFormat {
    type Error = crate::error::AvifError;

    fn try_from(fmt: oxideav_core::PixelFormat) -> Result<Self, Self::Error> {
        match fmt {
            oxideav_core::PixelFormat::Yuv420P => Ok(AvifPixelFormat::Yuv420P),
            oxideav_core::PixelFormat::Yuv422P => Ok(AvifPixelFormat::Yuv422P),
            oxideav_core::PixelFormat::Yuv444P => Ok(AvifPixelFormat::Yuv444P),
            oxideav_core::PixelFormat::Gray8 => Ok(AvifPixelFormat::Gray8),
            oxideav_core::PixelFormat::Yuva420P => Ok(AvifPixelFormat::Yuva420P),
            oxideav_core::PixelFormat::Yuva422P => Ok(AvifPixelFormat::Yuva422P),
            oxideav_core::PixelFormat::Yuva444P => Ok(AvifPixelFormat::Yuva444P),
            oxideav_core::PixelFormat::Ya8 => Ok(AvifPixelFormat::Ya8),
            oxideav_core::PixelFormat::Yuv420P10Le => Ok(AvifPixelFormat::Yuv420P10Le),
            oxideav_core::PixelFormat::Yuv422P10Le => Ok(AvifPixelFormat::Yuv422P10Le),
            oxideav_core::PixelFormat::Yuv444P10Le => Ok(AvifPixelFormat::Yuv444P10Le),
            oxideav_core::PixelFormat::Yuv420P12Le => Ok(AvifPixelFormat::Yuv420P12Le),
            oxideav_core::PixelFormat::Yuv422P12Le => Ok(AvifPixelFormat::Yuv422P12Le),
            oxideav_core::PixelFormat::Yuv444P12Le => Ok(AvifPixelFormat::Yuv444P12Le),
            oxideav_core::PixelFormat::Gray10Le => Ok(AvifPixelFormat::Gray10Le),
            oxideav_core::PixelFormat::Gray12Le => Ok(AvifPixelFormat::Gray12Le),
            oxideav_core::PixelFormat::Yuva420P10Le => Ok(AvifPixelFormat::Yuva420P10Le),
            oxideav_core::PixelFormat::Yuva422P10Le => Ok(AvifPixelFormat::Yuva422P10Le),
            oxideav_core::PixelFormat::Yuva444P10Le => Ok(AvifPixelFormat::Yuva444P10Le),
            oxideav_core::PixelFormat::Yuva420P12Le => Ok(AvifPixelFormat::Yuva420P12Le),
            oxideav_core::PixelFormat::Yuva422P12Le => Ok(AvifPixelFormat::Yuva422P12Le),
            oxideav_core::PixelFormat::Yuva444P12Le => Ok(AvifPixelFormat::Yuva444P12Le),
            oxideav_core::PixelFormat::Ya16Le => Ok(AvifPixelFormat::Ya16Le),
            other => Err(crate::error::AvifError::unsupported(format!(
                "avif: unsupported PixelFormat {other:?}"
            ))),
        }
    }
}
