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
//! The composite path supports every colour layout the underlying AV1
//! decoder emits, at every AV1 bit depth (8 / 10 / 12):
//!
//!   * `Yuv420P`/`Yuv422P`/`Yuv444P` (+ their `*10Le` / `*12Le`
//!     companions) + same-depth gray alpha -> the matching `Yuva*`
//!     layout (alpha appended as a fourth full-resolution plane).
//!   * `Gray8` + `Gray8` alpha -> packed `Ya8`; `Gray10Le` /
//!     `Gray12Le` with same-depth alpha -> packed `Ya16Le` (raw coded
//!     values in the low bits of each 16-bit LE word).
//!
//! The alpha auxiliary must carry the **same bit depth** as the master
//! (the av1-avif §4.1 `shall`: "An AV1 Alpha Image Item ... shall be
//! encoded with the same bit depth as the associated master AV1 Image
//! Item"); a depth mismatch returns `Error::InvalidData`.

use crate::error::{AvifError as Error, Result};
use crate::image::{
    AvifFrame as VideoFrame, AvifPixelFormat as PixelFormat, AvifPlane as VideoPlane,
};

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
/// frames must share `(width, height)`. The alpha frame must be a gray
/// layout of the **same bit depth** as the colour frame (the av1-avif
/// §4.1 `shall`); the colour frame must be planar YUV (any
/// subsampling, any AV1 bit depth) or gray.
///
/// `color_format` / `alpha_format` and the shared `(width, height)`
/// describe the per-stream metadata that no longer rides on
/// [`VideoFrame`]. The returned `(VideoFrame, PixelFormat)` carries the
/// composited pixels and the new packed format:
///
///   * the matching `Yuva*` layout when the colour frame is planar YUV
///     (alpha appended as a fourth full-resolution plane, same storage
///     width as the master).
///   * `Ya8` when the colour frame is `Gray8`; `Ya16Le` when it is
///     `Gray10Le` / `Gray12Le` (interleaved 16-bit LE words keeping
///     the raw coded values in the low bits).
pub fn composite_alpha(
    color: &VideoFrame,
    color_format: PixelFormat,
    width: u32,
    height: u32,
    alpha: &VideoFrame,
    alpha_format: PixelFormat,
) -> Result<(VideoFrame, PixelFormat)> {
    let expected_alpha = match color_format.bit_depth() {
        8 => PixelFormat::Gray8,
        10 => PixelFormat::Gray10Le,
        12 => PixelFormat::Gray12Le,
        other => {
            return Err(Error::unsupported(format!(
                "avif alpha: colour format {color_format:?} (depth {other}) cannot take an \
                 alpha auxiliary"
            )))
        }
    };
    if alpha_format != expected_alpha {
        return Err(Error::InvalidData(format!(
            "avif alpha: alpha plane format {alpha_format:?} != {expected_alpha:?} — the \
             auxiliary shall be encoded at the master's bit depth (av1-avif §4.1)"
        )));
    }
    let bps = color_format.bytes_per_sample();
    let out_format = color_format.with_alpha().ok_or_else(|| {
        Error::invalid(format!(
            "avif alpha: colour format {color_format:?} already carries alpha"
        ))
    })?;
    // Pack the alpha plane into a tightly-strided buffer — downstream
    // callers expect stride == width × bytes-per-sample.
    let alpha_packed = pack_plane(&alpha.planes[0], (width as usize) * bps, height as usize)?;

    if color_format.plane_count() == 3 {
        if color.planes.len() != 3 {
            return Err(Error::invalid(format!(
                "avif alpha: {color_format:?} frame has {} planes",
                color.planes.len()
            )));
        }
        let (sx, sy) = color_format.chroma_subsampling();
        let cw = (width as usize).div_ceil(1 << sx);
        let ch = (height as usize).div_ceil(1 << sy);
        let y = pack_plane(&color.planes[0], (width as usize) * bps, height as usize)?;
        let u = pack_plane(&color.planes[1], cw * bps, ch)?;
        let v = pack_plane(&color.planes[2], cw * bps, ch)?;
        Ok((
            VideoFrame {
                pts: color.pts,
                planes: vec![
                    VideoPlane {
                        stride: (width as usize) * bps,
                        data: y,
                    },
                    VideoPlane {
                        stride: cw * bps,
                        data: u,
                    },
                    VideoPlane {
                        stride: cw * bps,
                        data: v,
                    },
                    VideoPlane {
                        stride: (width as usize) * bps,
                        data: alpha_packed,
                    },
                ],
            },
            out_format,
        ))
    } else {
        // Gray master → packed Y A interleave (Ya8 / Ya16Le).
        if color.planes.len() != 1 {
            return Err(Error::invalid(format!(
                "avif alpha: {color_format:?} frame has {} planes",
                color.planes.len()
            )));
        }
        let y = pack_plane(&color.planes[0], (width as usize) * bps, height as usize)?;
        let mut ya = Vec::with_capacity(y.len() * 2);
        for i in 0..y.len() / bps {
            ya.extend_from_slice(&y[i * bps..(i + 1) * bps]);
            ya.extend_from_slice(&alpha_packed[i * bps..(i + 1) * bps]);
        }
        Ok((
            VideoFrame {
                pts: color.pts,
                planes: vec![VideoPlane {
                    stride: (width as usize) * 2 * bps,
                    data: ya,
                }],
            },
            out_format,
        ))
    }
}

/// Row-pack a plane to a tight stride. `row_bytes` is the packed row
/// width in **bytes** (pixel width × bytes-per-sample).
fn pack_plane(plane: &VideoPlane, row_bytes: usize, h: usize) -> Result<Vec<u8>> {
    if plane.stride == row_bytes && plane.data.len() == row_bytes * h {
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
    let mut out = Vec::with_capacity(row_bytes * h);
    for row in 0..h {
        let s = row * plane.stride;
        out.extend_from_slice(&plane.data[s..s + row_bytes]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_gray(w: u32, h: u32, fill: u8) -> VideoFrame {
        VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: w as usize,
                data: vec![fill; (w * h) as usize],
            }],
        }
    }

    fn make_yuv420(w: u32, h: u32) -> VideoFrame {
        assert!(w % 2 == 0 && h % 2 == 0);
        VideoFrame {
            pts: None,
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
        let (out, fmt) = composite_alpha(
            &color,
            PixelFormat::Yuv420P,
            4,
            4,
            &alpha,
            PixelFormat::Gray8,
        )
        .unwrap();
        assert_eq!(fmt, PixelFormat::Yuva420P);
        assert_eq!(out.planes.len(), 4);
        assert_eq!(out.planes[3].data.len(), 16);
        assert!(out.planes[3].data.iter().all(|&v| v == 200));
    }

    #[test]
    fn composite_gray_with_alpha_makes_ya8() {
        let color = make_gray(2, 2, 50);
        let alpha = make_gray(2, 2, 150);
        let (out, fmt) =
            composite_alpha(&color, PixelFormat::Gray8, 2, 2, &alpha, PixelFormat::Gray8).unwrap();
        assert_eq!(fmt, PixelFormat::Ya8);
        // Interleaved Y A Y A …
        assert_eq!(out.planes[0].data, vec![50, 150, 50, 150, 50, 150, 50, 150]);
    }

    #[test]
    fn composite_yuv444_with_alpha() {
        let color = VideoFrame {
            pts: None,
            planes: (0..3)
                .map(|i| VideoPlane {
                    stride: 4,
                    data: vec![(i * 40) as u8; 16],
                })
                .collect(),
        };
        let alpha = make_gray(4, 4, 77);
        let (out, fmt) = composite_alpha(
            &color,
            PixelFormat::Yuv444P,
            4,
            4,
            &alpha,
            PixelFormat::Gray8,
        )
        .unwrap();
        assert_eq!(fmt, PixelFormat::Yuva444P);
        assert_eq!(out.planes.len(), 4);
        // 4:4:4 chroma stays full extent.
        assert_eq!(out.planes[1].data.len(), 16);
        assert!(out.planes[3].data.iter().all(|&v| v == 77));
    }

    #[test]
    fn composite_yuv422_with_alpha() {
        let color = VideoFrame {
            pts: None,
            planes: vec![
                VideoPlane {
                    stride: 4,
                    data: vec![10u8; 16],
                },
                VideoPlane {
                    stride: 2,
                    data: vec![20u8; 8],
                },
                VideoPlane {
                    stride: 2,
                    data: vec![30u8; 8],
                },
            ],
        };
        let alpha = make_gray(4, 4, 200);
        let (out, fmt) = composite_alpha(
            &color,
            PixelFormat::Yuv422P,
            4,
            4,
            &alpha,
            PixelFormat::Gray8,
        )
        .unwrap();
        assert_eq!(fmt, PixelFormat::Yuva422P);
        assert_eq!(out.planes.len(), 4);
        // 4:2:2 chroma: half width, full height.
        assert_eq!(out.planes[1].data.len(), 8);
        assert_eq!(out.planes[3].data.len(), 16);
    }

    #[test]
    fn composite_alpha_format_mismatch_errors() {
        let c = make_yuv420(4, 4);
        let a = make_gray(4, 4, 0);
        // Pretend the alpha is not Gray8 to exercise the validation branch.
        let err =
            composite_alpha(&c, PixelFormat::Yuv420P, 4, 4, &a, PixelFormat::Yuv420P).unwrap_err();
        match err {
            Error::InvalidData(_) => {}
            other => panic!("expected InvalidData, got {other:?}"),
        }
    }

    /// Build a gray HBD plane of little-endian 16-bit words from `u16`
    /// samples.
    fn make_gray16(w: u32, h: u32, fill: u16) -> VideoFrame {
        let mut data = Vec::with_capacity((w * h) as usize * 2);
        for _ in 0..w * h {
            data.extend_from_slice(&fill.to_le_bytes());
        }
        VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: (w as usize) * 2,
                data,
            }],
        }
    }

    /// 10-bit 4:2:0 master + 10-bit gray alpha → `Yuva420P10Le` with
    /// the alpha appended as a full-resolution 16-bit LE plane.
    #[test]
    fn composite_yuv420_10bit_with_alpha() {
        let mk = |w: u32, h: u32, fill: u16| {
            let mut data = Vec::with_capacity((w * h) as usize * 2);
            for _ in 0..w * h {
                data.extend_from_slice(&fill.to_le_bytes());
            }
            VideoPlane {
                stride: (w as usize) * 2,
                data,
            }
        };
        let color = VideoFrame {
            pts: None,
            planes: vec![mk(4, 4, 600), mk(2, 2, 512), mk(2, 2, 700)],
        };
        let alpha = make_gray16(4, 4, 1023);
        let (out, fmt) = composite_alpha(
            &color,
            PixelFormat::Yuv420P10Le,
            4,
            4,
            &alpha,
            PixelFormat::Gray10Le,
        )
        .unwrap();
        assert_eq!(fmt, PixelFormat::Yuva420P10Le);
        assert_eq!(out.planes.len(), 4);
        assert_eq!(out.planes[0].stride, 8);
        assert_eq!(out.planes[3].data.len(), 32);
        for pair in out.planes[3].data.chunks_exact(2) {
            assert_eq!(u16::from_le_bytes([pair[0], pair[1]]), 1023);
        }
    }

    /// 12-bit gray master + 12-bit alpha → packed `Ya16Le` with raw
    /// values interleaved as Y A Y A … 16-bit LE words.
    #[test]
    fn composite_gray12_with_alpha_makes_ya16le() {
        let color = make_gray16(2, 2, 4000);
        let alpha = make_gray16(2, 2, 123);
        let (out, fmt) = composite_alpha(
            &color,
            PixelFormat::Gray12Le,
            2,
            2,
            &alpha,
            PixelFormat::Gray12Le,
        )
        .unwrap();
        assert_eq!(fmt, PixelFormat::Ya16Le);
        assert_eq!(out.planes.len(), 1);
        assert_eq!(out.planes[0].stride, 8);
        let words: Vec<u16> = out.planes[0]
            .data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(words, vec![4000, 123, 4000, 123, 4000, 123, 4000, 123]);
    }

    /// A depth mismatch between master and alpha violates the av1-avif
    /// §4.1 same-bit-depth `shall` and must be rejected as invalid.
    #[test]
    fn composite_alpha_depth_mismatch_rejected() {
        let color = make_gray16(2, 2, 500);
        let alpha = make_gray(2, 2, 100);
        let err = composite_alpha(
            &color,
            PixelFormat::Gray10Le,
            2,
            2,
            &alpha,
            PixelFormat::Gray8,
        )
        .unwrap_err();
        match err {
            Error::InvalidData(msg) => assert!(msg.contains("§4.1"), "{msg}"),
            other => panic!("expected InvalidData, got {other:?}"),
        }
    }
}
