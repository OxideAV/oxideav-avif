//! AVIF alpha auxiliary-image handling.
//!
//! AVIF signals an alpha channel by storing it as a separate AV1-coded
//! monochrome item referenced from the primary item through a pair of
//! signals:
//!
//!   1. An `iref` entry of type `auxl` whose `from_id` is the alpha
//!      candidate item and whose `to_ids` contains the primary item id.
//!   2. The candidate item carries an `auxC` property whose `aux_type`
//!      URN starts with `urn:mpeg:mpegB:cicp:systems:auxiliary:alpha`.
//!
//! The helpers here locate the alpha item id, verify the URN match,
//! and composite a decoded alpha plane onto a decoded colour frame.
//! The composite path supports the two colour layouts the underlying
//! AV1 decoder emits today:
//!
//!   * `PixelFormat::Yuv420P` + 8-bit Gray alpha  -> `PixelFormat::Yuva420P`
//!   * `PixelFormat::Gray8`   + 8-bit Gray alpha  -> `PixelFormat::Ya8`
//!
//! Other layouts return `Error::Unsupported`.

use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{Error, PixelFormat, Result};

use crate::box_parser::{b, BoxType};
use crate::meta::{Meta, Property};

/// The CICP alpha-auxiliary URN. AVIF §7.3.3.
pub const ALPHA_URN_PREFIX: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";

const AUXL: BoxType = b(b"auxl");
const AUXC: BoxType = b(b"auxC");

/// Locate the alpha auxiliary item for the given primary item. Returns
/// `Some(item_id)` when both an `auxl` iref targeting `primary_id` and a
/// matching `auxC` URN are present; `None` otherwise.
pub fn find_alpha_item_id(meta: &Meta, primary_id: u32) -> Option<u32> {
    // Candidate: source of an auxl iref whose to_ids contains primary_id.
    let candidate = meta.iref_source_of(&AUXL, primary_id)?;
    // Verify the candidate's auxC property carries the alpha URN.
    if let Some(Property::AuxC(aux)) = meta.property_for(candidate, &AUXC) {
        if aux.aux_type.starts_with(ALPHA_URN_PREFIX) {
            return Some(candidate);
        }
    }
    None
}

/// Composite a decoded alpha frame onto a decoded colour frame. Both
/// frames must share `(width, height)`. The alpha frame must be
/// `Gray8`; the colour frame must be `Yuv420P` or `Gray8`.
///
/// The resulting frame's format is:
///
///   * `Yuva420P` when the colour frame is `Yuv420P`.
///   * `Ya8`     when the colour frame is `Gray8`.
pub fn composite_alpha(color: &VideoFrame, alpha: &VideoFrame) -> Result<VideoFrame> {
    if color.width != alpha.width || color.height != alpha.height {
        return Err(Error::invalid(format!(
            "avif alpha: colour {}x{} != alpha {}x{}",
            color.width, color.height, alpha.width, alpha.height
        )));
    }
    if alpha.format != PixelFormat::Gray8 {
        return Err(Error::unsupported(format!(
            "avif alpha: alpha plane format {:?} != Gray8 (HBD alpha not yet supported)",
            alpha.format
        )));
    }
    // Pack the alpha plane into a tightly-strided buffer — downstream
    // callers expect stride == width.
    let alpha_packed = pack_plane(
        &alpha.planes[0],
        alpha.width as usize,
        alpha.height as usize,
    )?;

    match color.format {
        PixelFormat::Yuv420P => {
            if color.planes.len() != 3 {
                return Err(Error::invalid(format!(
                    "avif alpha: Yuv420P frame has {} planes",
                    color.planes.len()
                )));
            }
            let cw = color.width.div_ceil(2) as usize;
            let ch = color.height.div_ceil(2) as usize;
            let y = pack_plane(
                &color.planes[0],
                color.width as usize,
                color.height as usize,
            )?;
            let u = pack_plane(&color.planes[1], cw, ch)?;
            let v = pack_plane(&color.planes[2], cw, ch)?;
            Ok(VideoFrame {
                format: PixelFormat::Yuva420P,
                width: color.width,
                height: color.height,
                pts: color.pts,
                time_base: color.time_base,
                planes: vec![
                    VideoPlane {
                        stride: color.width as usize,
                        data: y,
                    },
                    VideoPlane {
                        stride: cw,
                        data: u,
                    },
                    VideoPlane {
                        stride: cw,
                        data: v,
                    },
                    VideoPlane {
                        stride: color.width as usize,
                        data: alpha_packed,
                    },
                ],
            })
        }
        PixelFormat::Gray8 => {
            if color.planes.len() != 1 {
                return Err(Error::invalid(format!(
                    "avif alpha: Gray8 frame has {} planes",
                    color.planes.len()
                )));
            }
            let y = pack_plane(
                &color.planes[0],
                color.width as usize,
                color.height as usize,
            )?;
            // Ya8 is packed Y A Y A ...
            let mut ya = Vec::with_capacity(y.len() * 2);
            for i in 0..y.len() {
                ya.push(y[i]);
                ya.push(alpha_packed[i]);
            }
            Ok(VideoFrame {
                format: PixelFormat::Ya8,
                width: color.width,
                height: color.height,
                pts: color.pts,
                time_base: color.time_base,
                planes: vec![VideoPlane {
                    stride: (color.width as usize) * 2,
                    data: ya,
                }],
            })
        }
        other => Err(Error::unsupported(format!(
            "avif alpha: colour format {other:?} not supported by 8-bit composite path"
        ))),
    }
}

fn pack_plane(plane: &VideoPlane, w: usize, h: usize) -> Result<Vec<u8>> {
    if plane.stride == w && plane.data.len() == w * h {
        return Ok(plane.data.clone());
    }
    if plane.data.len() < plane.stride * h {
        return Err(Error::invalid(format!(
            "avif alpha: plane truncated (stride={} rows={} have={})",
            plane.stride,
            h,
            plane.data.len()
        )));
    }
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        let s = row * plane.stride;
        out.extend_from_slice(&plane.data[s..s + w]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::TimeBase;

    fn make_gray(w: u32, h: u32, fill: u8) -> VideoFrame {
        VideoFrame {
            format: PixelFormat::Gray8,
            width: w,
            height: h,
            pts: None,
            time_base: TimeBase::new(1, 1),
            planes: vec![VideoPlane {
                stride: w as usize,
                data: vec![fill; (w * h) as usize],
            }],
        }
    }

    fn make_yuv420(w: u32, h: u32) -> VideoFrame {
        assert!(w % 2 == 0 && h % 2 == 0);
        VideoFrame {
            format: PixelFormat::Yuv420P,
            width: w,
            height: h,
            pts: None,
            time_base: TimeBase::new(1, 1),
            planes: vec![
                VideoPlane {
                    stride: w as usize,
                    data: vec![100u8; (w * h) as usize],
                },
                VideoPlane {
                    stride: (w / 2) as usize,
                    data: vec![128u8; ((w / 2) * (h / 2)) as usize],
                },
                VideoPlane {
                    stride: (w / 2) as usize,
                    data: vec![128u8; ((w / 2) * (h / 2)) as usize],
                },
            ],
        }
    }

    #[test]
    fn composite_yuv420_with_alpha() {
        let color = make_yuv420(4, 4);
        let alpha = make_gray(4, 4, 200);
        let out = composite_alpha(&color, &alpha).unwrap();
        assert_eq!(out.format, PixelFormat::Yuva420P);
        assert_eq!(out.planes.len(), 4);
        assert_eq!(out.planes[3].data.len(), 16);
        assert!(out.planes[3].data.iter().all(|&v| v == 200));
    }

    #[test]
    fn composite_gray_with_alpha_makes_ya8() {
        let color = make_gray(2, 2, 50);
        let alpha = make_gray(2, 2, 150);
        let out = composite_alpha(&color, &alpha).unwrap();
        assert_eq!(out.format, PixelFormat::Ya8);
        // Interleaved Y A Y A …
        assert_eq!(out.planes[0].data, vec![50, 150, 50, 150, 50, 150, 50, 150]);
    }

    #[test]
    fn composite_mismatched_dims_errors() {
        let c = make_yuv420(4, 4);
        let a = make_gray(2, 2, 0);
        let err = composite_alpha(&c, &a).unwrap_err();
        matches!(err, Error::InvalidData(_));
    }
}
