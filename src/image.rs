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
    /// 8-bit packed Y A interleaved. Produced by
    /// [`crate::alpha::composite_alpha`] when the colour primary is
    /// already monochrome and an alpha auxiliary is attached.
    Ya8,
}

impl AvifPixelFormat {
    /// Number of planes the format ships — single plane for `Gray8` /
    /// `Ya8` (the latter is interleaved into one buffer), three for
    /// planar YUV, four for `Yuva420P` (Y + U + V + A).
    pub fn plane_count(&self) -> usize {
        match self {
            Self::Gray8 | Self::Ya8 => 1,
            Self::Yuv420P | Self::Yuv422P | Self::Yuv444P => 3,
            Self::Yuva420P => 4,
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
            AvifPixelFormat::Ya8 => oxideav_core::PixelFormat::Ya8,
        }
    }
}

/// Bridge a framework [`oxideav_core::PixelFormat`] back into the
/// crate-local [`AvifPixelFormat`]. Only the variants the AVIF
/// pipeline emits are handled; anything else (HBD YUV, packed RGB,
/// audio formats wedged into the enum) returns an [`AvifError`].
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
            oxideav_core::PixelFormat::Ya8 => Ok(AvifPixelFormat::Ya8),
            other => Err(crate::error::AvifError::unsupported(format!(
                "avif: unsupported PixelFormat {other:?}"
            ))),
        }
    }
}
