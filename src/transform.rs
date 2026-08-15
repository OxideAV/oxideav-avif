//! Post-decode geometric transforms for AVIF primary items.
//!
//! Covers HEIF §6.5.10 (`irot`, rotation), §6.5.12 (`imir`, mirror) and
//! §6.5.11 (`clap`, clean-aperture cropping). The canonical application
//! order once an AV1 frame has been reconstructed is:
//!
//!   1. Crop to the `ispe` declared size if the coded frame was padded
//!      to alignment.
//!   2. Apply `clap`.
//!   3. Apply `irot`.
//!   4. Apply `imir`.
//!
//! Each entry point takes the source frame plus the stream-level
//! `(format, width, height)` triple (the slim [`VideoFrame`] no longer
//! carries those fields) and returns a freshly-allocated frame plus its
//! new `(width, height)` (the format is preserved). The source frame is
//! left untouched. The `VideoFrame` layout matches what `oxideav-av1`
//! emits: one plane per channel (Y, U, V for the planar layouts, a
//! single plane for gray / packed-YA), one byte per sample for the
//! 8-bit formats and a little-endian 16-bit word per sample for the
//! 10/12-bit (`*10Le` / `*12Le`) and `Ya16Le` formats. All geometry is
//! expressed in pixels; the byte maths scales by the format's
//! per-pixel storage width internally.
//!
//! The transforms are strictly pixel-level operations — they do not
//! understand chroma siting or BT.709 vs. full-range semantics, both of
//! which are orthogonal to geometric manipulation.

use crate::error::{AvifError as Error, Result};
use crate::image::{
    AvifFrame as VideoFrame, AvifPixelFormat as PixelFormat, AvifPlane as VideoPlane,
};

use crate::meta::{Clap, Imir, Irot};

/// Return the `(horizontal, vertical)` chroma subsampling shifts for a
/// pixel format. `0` means no subsampling on that axis.
fn subsampling(format: PixelFormat) -> (u8, u8) {
    format.chroma_subsampling()
}

/// Return the number of planes that ride on this pixel format.
fn plane_count(format: PixelFormat) -> usize {
    format.plane_count()
}

/// Bytes one *pixel* occupies within a plane of this format: the
/// storage bytes per sample, doubled for the packed Y-A layouts whose
/// single plane interleaves two samples per pixel.
fn pixel_bytes(format: PixelFormat) -> usize {
    format.bytes_per_sample() * if format.is_packed_ya() { 2 } else { 1 }
}

/// True when `plane` carries full-resolution samples for this format:
/// the luma plane (0) always, and the alpha plane (3) of the `Yuva*`
/// layouts (alpha rides at luma resolution — av1-avif §4.1 codes it as
/// a monochrome AV1 stream at the master's extents).
fn plane_is_full_res(plane: usize) -> bool {
    plane == 0 || plane == 3
}

/// Per-plane pixel dimensions for a frame of the given format and dims.
fn plane_dims(format: PixelFormat, width: u32, height: u32, plane: usize) -> Result<(u32, u32)> {
    let (sx, sy) = subsampling(format);
    if plane_is_full_res(plane) {
        Ok((width, height))
    } else {
        let w = (width + (1 << sx) - 1) >> sx;
        let h = (height + (1 << sy) - 1) >> sy;
        Ok((w.max(1), h.max(1)))
    }
}

/// Crop every plane of `frame` to the top-left `out_w × out_h` pixels.
/// Used both by `clap` application and by the ispe-vs-coded-size clamp
/// on padded frames.
///
/// Returns the cropped frame; the format is preserved, the new
/// dimensions are `(out_w, out_h)`.
pub fn crop_top_left(
    frame: &VideoFrame,
    format: PixelFormat,
    width: u32,
    height: u32,
    out_w: u32,
    out_h: u32,
) -> Result<VideoFrame> {
    if out_w == 0 || out_h == 0 {
        return Err(Error::invalid("avif: crop to zero dims"));
    }
    if out_w > width || out_h > height {
        return Err(Error::invalid(format!(
            "avif: crop {}x{} exceeds source {}x{}",
            out_w, out_h, width, height
        )));
    }
    if out_w == width && out_h == height {
        return Ok(frame.clone());
    }
    crop_rect(frame, format, width, height, 0, 0, out_w, out_h)
}

/// Generic rectangular crop. Offsets and size are expressed in luma
/// coordinates; chroma planes are scaled down by their subsampling.
/// Every dimension must respect the chroma subsampling (i.e. on Yuv420P
/// the offsets and sizes must be even).
#[allow(clippy::too_many_arguments)]
fn crop_rect(
    frame: &VideoFrame,
    format: PixelFormat,
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<VideoFrame> {
    let (sx, sy) = subsampling(format);
    let planes = plane_count(format);
    let unit = pixel_bytes(format);
    if frame.planes.len() != planes {
        return Err(Error::invalid(format!(
            "avif: frame has {} planes, expected {planes} for {:?}",
            frame.planes.len(),
            format
        )));
    }
    let mut out = Vec::with_capacity(planes);
    for p in 0..planes {
        let (px, py, pw, ph) = if plane_is_full_res(p) {
            (x, y, w, h)
        } else {
            (x >> sx, y >> sy, (w >> sx).max(1), (h >> sy).max(1))
        };
        let src = &frame.planes[p];
        let src_stride = src.stride;
        let (plane_w, _plane_h) = plane_dims(format, width, height, p)?;
        let mut data = Vec::with_capacity((pw as usize) * (ph as usize) * unit);
        for row in 0..ph as usize {
            let src_row = (py as usize + row) * src_stride + (px as usize) * unit;
            let end = src_row + (pw as usize) * unit;
            if end > src.data.len() {
                return Err(Error::invalid(format!(
                    "avif: crop row {row} reads past plane {p} of width {plane_w}"
                )));
            }
            data.extend_from_slice(&src.data[src_row..end]);
        }
        out.push(VideoPlane {
            stride: (pw as usize) * unit,
            data,
        });
    }
    Ok(VideoFrame {
        pts: frame.pts,
        planes: out,
    })
}

/// Apply a `clap` (clean-aperture) crop. Dimensions that fall outside
/// the source rectangle or whose denominators are zero return the input
/// unchanged (defensive: a malformed `clap` is treated as a no-op rather
/// than an error so the rest of the image still renders).
///
/// `clap` crop width / height / horizontal / vertical offsets are signed
/// rationals. The spec defines the crop centre as
/// `((W - 1) / 2 + horizOff, (H - 1) / 2 + vertOff)`, and the crop is
/// `cleanApertureWidth × cleanApertureHeight` pixels.
///
/// Returns the cropped frame and its new `(width, height)`.
pub fn apply_clap(
    frame: &VideoFrame,
    format: PixelFormat,
    width: u32,
    height: u32,
    clap: &Clap,
) -> Result<(VideoFrame, u32, u32)> {
    if clap.clean_aperture_width_d == 0
        || clap.clean_aperture_height_d == 0
        || clap.horiz_off_d == 0
        || clap.vert_off_d == 0
    {
        return Ok((frame.clone(), width, height));
    }
    let w = width as i64;
    let h = height as i64;
    // Crop width / height rounded to nearest integer.
    let cw_num = clap.clean_aperture_width_n as i64;
    let cw_den = clap.clean_aperture_width_d as i64;
    let ch_num = clap.clean_aperture_height_n as i64;
    let ch_den = clap.clean_aperture_height_d as i64;
    let cw = (cw_num + cw_den / 2) / cw_den;
    let ch = (ch_num + ch_den / 2) / ch_den;
    if cw <= 0 || ch <= 0 || cw > w || ch > h {
        return Ok((frame.clone(), width, height));
    }
    // Centre, as a float per the §6.5.9 clean-aperture geometry;
    // denominators are 32-bit so f64 has enough precision.
    let centre_x = (w - 1) as f64 / 2.0 + clap.horiz_off_n as f64 / clap.horiz_off_d as f64;
    let centre_y = (h - 1) as f64 / 2.0 + clap.vert_off_n as f64 / clap.vert_off_d as f64;
    let mut x0 = (centre_x - (cw - 1) as f64 / 2.0 + 0.5).floor() as i64;
    let mut y0 = (centre_y - (ch - 1) as f64 / 2.0 + 0.5).floor() as i64;
    if x0 < 0 {
        x0 = 0;
    }
    if y0 < 0 {
        y0 = 0;
    }
    if x0 + cw > w {
        x0 = w - cw;
    }
    if y0 + ch > h {
        y0 = h - ch;
    }
    // Subsampling requires even offsets / sizes on subsampled planes —
    // snap defensively so chroma cropping matches luma.
    let (sx, sy) = subsampling(format);
    let align_x = 1i64 << sx;
    let align_y = 1i64 << sy;
    x0 -= x0 % align_x;
    y0 -= y0 % align_y;
    let cw_aligned = cw - (cw % align_x);
    let ch_aligned = ch - (ch % align_y);
    if cw_aligned <= 0 || ch_aligned <= 0 {
        return Ok((frame.clone(), width, height));
    }
    let cropped = crop_rect(
        frame,
        format,
        width,
        height,
        x0 as u32,
        y0 as u32,
        cw_aligned as u32,
        ch_aligned as u32,
    )?;
    Ok((cropped, cw_aligned as u32, ch_aligned as u32))
}

/// Apply an `irot` rotation (counter-clockwise, 0..3 × 90°). Rotating by
/// 90° or 270° swaps the width and height. Chroma subsampling stays the
/// same — a Yuv420P input returns a Yuv420P output with swapped chroma
/// dims.
///
/// Returns the rotated frame and its new `(width, height)`.
pub fn apply_irot(
    frame: &VideoFrame,
    format: PixelFormat,
    width: u32,
    height: u32,
    irot: &Irot,
) -> Result<(VideoFrame, u32, u32)> {
    let turns = irot.angle & 0x03;
    if turns == 0 {
        return Ok((frame.clone(), width, height));
    }
    let (sx, sy) = subsampling(format);
    let planes = plane_count(format);
    let unit = pixel_bytes(format);
    if frame.planes.len() != planes {
        return Err(Error::invalid(format!(
            "avif irot: frame has {} planes, expected {planes}",
            frame.planes.len()
        )));
    }
    // If the rotation parity is odd, the chroma dim swap must keep the
    // 4:2:0 / 4:2:2 property legal. For 4:2:2 (sx=1, sy=0) a 90° turn
    // produces 2:2:4 — which isn't a legal YUV layout — so reject that
    // combination explicitly.
    let odd = (turns & 1) == 1;
    if odd && sx != sy {
        return Err(Error::unsupported(format!(
            "avif irot: {}° rotation of {:?} requires symmetric subsampling",
            turns as u32 * 90,
            format
        )));
    }
    let mut out_planes = Vec::with_capacity(planes);
    for p in 0..planes {
        let (pw, ph) = plane_dims(format, width, height, p)?;
        let src = &frame.planes[p];
        let (ow, oh) = if odd { (ph, pw) } else { (pw, ph) };
        let mut data = vec![0u8; (ow as usize) * (oh as usize) * unit];
        // For each output pixel (ox, oy), compute its source (src_x,
        // src_y) under a `turns × 90°` counter-clockwise rotation. A
        // pixel at input (x, y) maps to output (y, W-1-x) for one CCW
        // turn; inverting that gives src_x = W-1-oy, src_y = ox.
        for oy in 0..oh as usize {
            for ox in 0..ow as usize {
                let (src_x, src_y) = match turns {
                    1 => (pw as usize - 1 - oy, ox),
                    2 => (pw as usize - 1 - ox, ph as usize - 1 - oy),
                    3 => (oy, ph as usize - 1 - ox),
                    _ => unreachable!(),
                };
                let si = src_y * src.stride + src_x * unit;
                let di = (oy * ow as usize + ox) * unit;
                data[di..di + unit].copy_from_slice(&src.data[si..si + unit]);
            }
        }
        out_planes.push(VideoPlane {
            stride: (ow as usize) * unit,
            data,
        });
    }
    let (new_w, new_h) = if odd {
        (height, width)
    } else {
        (width, height)
    };
    Ok((
        VideoFrame {
            pts: frame.pts,
            planes: out_planes,
        },
        new_w,
        new_h,
    ))
}

/// Apply an `imir` mirror. `axis == 0` flips top↔bottom, `axis == 1`
/// flips left↔right. This matches the AVIF 1.1 / HEIF convention.
///
/// Width and height are unchanged; returned for caller convenience.
pub fn apply_imir(
    frame: &VideoFrame,
    format: PixelFormat,
    width: u32,
    height: u32,
    imir: &Imir,
) -> Result<(VideoFrame, u32, u32)> {
    let axis = imir.axis & 0x01;
    let planes = plane_count(format);
    let unit = pixel_bytes(format);
    if frame.planes.len() != planes {
        return Err(Error::invalid(format!(
            "avif imir: frame has {} planes, expected {planes}",
            frame.planes.len()
        )));
    }
    let mut out_planes = Vec::with_capacity(planes);
    for p in 0..planes {
        let (pw, ph) = plane_dims(format, width, height, p)?;
        let src = &frame.planes[p];
        let mut data = vec![0u8; (pw as usize) * (ph as usize) * unit];
        for y in 0..ph as usize {
            for x in 0..pw as usize {
                let (sx, sy) = if axis == 1 {
                    (pw as usize - 1 - x, y)
                } else {
                    (x, ph as usize - 1 - y)
                };
                let si = sy * src.stride + sx * unit;
                let di = (y * pw as usize + x) * unit;
                data[di..di + unit].copy_from_slice(&src.data[si..si + unit]);
            }
        }
        out_planes.push(VideoPlane {
            stride: (pw as usize) * unit,
            data,
        });
    }
    Ok((
        VideoFrame {
            pts: frame.pts,
            planes: out_planes,
        },
        width,
        height,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_gray(w: u32, h: u32, fill: impl Fn(u32, u32) -> u8) -> VideoFrame {
        let mut data = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                data.push(fill(x, y));
            }
        }
        VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: w as usize,
                data,
            }],
        }
    }

    fn make_yuv420(w: u32, h: u32) -> VideoFrame {
        assert!(w % 2 == 0 && h % 2 == 0);
        let y: Vec<u8> = (0..w * h).map(|i| (i & 0xff) as u8).collect();
        let u: Vec<u8> = (0..(w / 2) * (h / 2))
            .map(|i| ((i + 40) & 0xff) as u8)
            .collect();
        let v: Vec<u8> = (0..(w / 2) * (h / 2))
            .map(|i| ((i + 80) & 0xff) as u8)
            .collect();
        VideoFrame {
            pts: None,
            planes: vec![
                VideoPlane {
                    stride: w as usize,
                    data: y,
                },
                VideoPlane {
                    stride: (w / 2) as usize,
                    data: u,
                },
                VideoPlane {
                    stride: (w / 2) as usize,
                    data: v,
                },
            ],
        }
    }

    #[test]
    fn irot_identity_on_zero_angle() {
        let f = make_gray(4, 2, |x, _| x as u8);
        let (out, ow, oh) = apply_irot(&f, PixelFormat::Gray8, 4, 2, &Irot { angle: 0 }).unwrap();
        assert_eq!(ow, 4);
        assert_eq!(oh, 2);
        assert_eq!(out.planes[0].data, f.planes[0].data);
    }

    #[test]
    fn irot_90_swaps_dims() {
        // 2x3 with distinct pixel values.
        //  0 1
        //  2 3
        //  4 5
        let f = make_gray(2, 3, |x, y| (y * 2 + x) as u8);
        let (out, ow, oh) = apply_irot(&f, PixelFormat::Gray8, 2, 3, &Irot { angle: 1 }).unwrap();
        assert_eq!(ow, 3);
        assert_eq!(oh, 2);
        // 90° CCW of 2x3 -> 3x2. Top-right (1) lands at top-left,
        // bottom-right (5) at top-right, top-left (0) at bottom-left,
        // bottom-left (4) at bottom-right:
        //   1 3 5
        //   0 2 4
        assert_eq!(out.planes[0].data, vec![1, 3, 5, 0, 2, 4]);
    }

    #[test]
    fn irot_180_flips_both() {
        let f = make_gray(2, 2, |x, y| (y * 2 + x) as u8);
        let (out, _, _) = apply_irot(&f, PixelFormat::Gray8, 2, 2, &Irot { angle: 2 }).unwrap();
        // original: 0 1 / 2 3   -> 180°: 3 2 / 1 0
        assert_eq!(out.planes[0].data, vec![3, 2, 1, 0]);
    }

    #[test]
    fn irot_270_swaps_dims_clockwise() {
        let f = make_gray(2, 3, |x, y| (y * 2 + x) as u8);
        let (out, ow, oh) = apply_irot(&f, PixelFormat::Gray8, 2, 3, &Irot { angle: 3 }).unwrap();
        assert_eq!(ow, 3);
        assert_eq!(oh, 2);
        // 270° CCW (= 90° CW):
        //   4 2 0
        //   5 3 1
        assert_eq!(out.planes[0].data, vec![4, 2, 0, 5, 3, 1]);
    }

    #[test]
    fn irot_90_yuv422_rejected() {
        // 4:2:2 has asymmetric subsampling (sx=1, sy=0) — 90° rotation
        // would turn it into 2:2:4, which isn't a legal layout.
        let mut f = make_yuv420(4, 4);
        // Repoint chroma planes to match 4:2:2 dims (2x4).
        f.planes[1].stride = 2;
        f.planes[1].data = vec![0u8; 2 * 4];
        f.planes[2].stride = 2;
        f.planes[2].data = vec![0u8; 2 * 4];
        let err = apply_irot(&f, PixelFormat::Yuv422P, 4, 4, &Irot { angle: 1 }).unwrap_err();
        match err {
            Error::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn imir_horizontal() {
        let f = make_gray(3, 2, |x, y| (y * 3 + x) as u8);
        let (out, _, _) = apply_imir(&f, PixelFormat::Gray8, 3, 2, &Imir { axis: 1 }).unwrap();
        // flip left↔right: each row reversed
        assert_eq!(out.planes[0].data, vec![2, 1, 0, 5, 4, 3]);
    }

    #[test]
    fn imir_vertical() {
        let f = make_gray(3, 2, |x, y| (y * 3 + x) as u8);
        let (out, _, _) = apply_imir(&f, PixelFormat::Gray8, 3, 2, &Imir { axis: 0 }).unwrap();
        // flip top↔bottom: rows swapped
        assert_eq!(out.planes[0].data, vec![3, 4, 5, 0, 1, 2]);
    }

    #[test]
    fn crop_top_left_yuv420() {
        let f = make_yuv420(4, 4);
        let out = crop_top_left(&f, PixelFormat::Yuv420P, 4, 4, 2, 2).unwrap();
        // Y: rows [0..2, 4..6]
        assert_eq!(out.planes[0].data, vec![0, 1, 4, 5]);
        // U/V: 1x1 chroma plane
        assert_eq!(out.planes[1].data.len(), 1);
        assert_eq!(out.planes[2].data.len(), 1);
    }

    #[test]
    fn clap_noop_when_denom_zero() {
        let f = make_gray(4, 4, |x, y| (y * 4 + x) as u8);
        let clap = Clap {
            clean_aperture_width_n: 2,
            clean_aperture_width_d: 0,
            clean_aperture_height_n: 2,
            clean_aperture_height_d: 1,
            horiz_off_n: 0,
            horiz_off_d: 1,
            vert_off_n: 0,
            vert_off_d: 1,
        };
        let (out, _, _) = apply_clap(&f, PixelFormat::Gray8, 4, 4, &clap).unwrap();
        assert_eq!(out.planes[0].data, f.planes[0].data);
    }

    #[test]
    fn clap_centre_crop() {
        // 4x4 image, crop 2x2 around the centre.
        let f = make_gray(4, 4, |x, y| (y * 4 + x) as u8);
        let clap = Clap {
            clean_aperture_width_n: 2,
            clean_aperture_width_d: 1,
            clean_aperture_height_n: 2,
            clean_aperture_height_d: 1,
            horiz_off_n: 0,
            horiz_off_d: 1,
            vert_off_n: 0,
            vert_off_d: 1,
        };
        let (out, ow, oh) = apply_clap(&f, PixelFormat::Gray8, 4, 4, &clap).unwrap();
        assert_eq!(ow, 2);
        assert_eq!(oh, 2);
        // Centre of 4x4 is (1.5, 1.5); crop top-left floor(1.5 - 0.5 + 0.5)=1.
        // So the crop is x=1, y=1, 2x2 -> pixels (1,1), (2,1), (1,2), (2,2).
        // Those are 5, 6, 9, 10.
        assert_eq!(out.planes[0].data, vec![5, 6, 9, 10]);
    }

    // ── HBD (16-bit-LE-stored) + packed-YA geometry ──

    /// Single-plane frame of little-endian 16-bit words generated per
    /// pixel; stride = 2 × width bytes.
    fn make_gray16(w: u32, h: u32, fill: impl Fn(u32, u32) -> u16) -> VideoFrame {
        let mut data = Vec::with_capacity((w * h) as usize * 2);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&fill(x, y).to_le_bytes());
            }
        }
        VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: (w as usize) * 2,
                data,
            }],
        }
    }

    fn words(p: &VideoPlane) -> Vec<u16> {
        p.data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    /// 90° CCW rotation of a 10-bit gray frame moves whole 16-bit LE
    /// words: same geometry as the 8-bit case, byte pairs intact.
    #[test]
    fn irot_90_gray10_moves_words() {
        // 2x3 with distinct 10-bit values 1000..1005.
        let f = make_gray16(2, 3, |x, y| 1000 + (y * 2 + x) as u16);
        let (out, ow, oh) =
            apply_irot(&f, PixelFormat::Gray10Le, 2, 3, &Irot { angle: 1 }).unwrap();
        assert_eq!((ow, oh), (3, 2));
        assert_eq!(out.planes[0].stride, 6);
        assert_eq!(
            words(&out.planes[0]),
            vec![1001, 1003, 1005, 1000, 1002, 1004]
        );
    }

    /// Mirror of a 12-bit gray frame reverses whole words per row.
    #[test]
    fn imir_horizontal_gray12_moves_words() {
        let f = make_gray16(3, 2, |x, y| 2048 + (y * 3 + x) as u16);
        let (out, _, _) = apply_imir(&f, PixelFormat::Gray12Le, 3, 2, &Imir { axis: 1 }).unwrap();
        assert_eq!(
            words(&out.planes[0]),
            vec![2050, 2049, 2048, 2053, 2052, 2051]
        );
    }

    /// `clap` centre crop on a 10-bit gray frame lands on the same
    /// pixel rectangle as the 8-bit case, carrying the 16-bit words.
    #[test]
    fn clap_centre_crop_gray10() {
        let f = make_gray16(4, 4, |x, y| 512 + (y * 4 + x) as u16);
        let clap = Clap {
            clean_aperture_width_n: 2,
            clean_aperture_width_d: 1,
            clean_aperture_height_n: 2,
            clean_aperture_height_d: 1,
            horiz_off_n: 0,
            horiz_off_d: 1,
            vert_off_n: 0,
            vert_off_d: 1,
        };
        let (out, ow, oh) = apply_clap(&f, PixelFormat::Gray10Le, 4, 4, &clap).unwrap();
        assert_eq!((ow, oh), (2, 2));
        assert_eq!(words(&out.planes[0]), vec![517, 518, 521, 522]);
    }

    /// Crop of a 4-plane 10-bit 4:2:0 frame scales the chroma planes
    /// and keeps the full-resolution alpha plane (plane 3) at luma
    /// extents — all in 16-bit words.
    #[test]
    fn crop_top_left_yuva420p10() {
        let mk = |w: u32, h: u32, base: u16| {
            let mut data = Vec::new();
            for i in 0..w * h {
                data.extend_from_slice(&(base + i as u16).to_le_bytes());
            }
            VideoPlane {
                stride: (w as usize) * 2,
                data,
            }
        };
        let f = VideoFrame {
            pts: None,
            planes: vec![mk(4, 4, 0), mk(2, 2, 100), mk(2, 2, 200), mk(4, 4, 300)],
        };
        let out = crop_top_left(&f, PixelFormat::Yuva420P10Le, 4, 4, 2, 2).unwrap();
        assert_eq!(words(&out.planes[0]), vec![0, 1, 4, 5]);
        assert_eq!(words(&out.planes[1]), vec![100]);
        assert_eq!(words(&out.planes[2]), vec![200]);
        assert_eq!(words(&out.planes[3]), vec![300, 301, 304, 305]);
    }

    /// Packed `Ya8` rotation moves 2-byte Y-A pixels intact — the
    /// packed-YA layouts route through the same geometry with a
    /// 2-samples-per-pixel unit.
    #[test]
    fn irot_90_ya8_moves_pixel_pairs() {
        // 2x2 Ya8: pixels (Y=0,A=10), (1,11) / (2,12), (3,13).
        let f = VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: 4,
                data: vec![0, 10, 1, 11, 2, 12, 3, 13],
            }],
        };
        let (out, ow, oh) = apply_irot(&f, PixelFormat::Ya8, 2, 2, &Irot { angle: 1 }).unwrap();
        assert_eq!((ow, oh), (2, 2));
        // 90° CCW: top row becomes (1,11), (3,13); bottom (0,10), (2,12).
        assert_eq!(out.planes[0].data, vec![1, 11, 3, 13, 0, 10, 2, 12]);
    }

    /// Packed `Ya16Le` mirror moves 4-byte Y-A pixels intact.
    #[test]
    fn imir_ya16le_moves_pixel_quads() {
        // 2x1 Ya16Le: pixels (Y=1000, A=1), (Y=2000, A=2).
        let mut data = Vec::new();
        for w in [1000u16, 1, 2000, 2] {
            data.extend_from_slice(&w.to_le_bytes());
        }
        let f = VideoFrame {
            pts: None,
            planes: vec![VideoPlane { stride: 8, data }],
        };
        let (out, _, _) = apply_imir(&f, PixelFormat::Ya16Le, 2, 1, &Imir { axis: 1 }).unwrap();
        assert_eq!(words(&out.planes[0]), vec![2000, 2, 1000, 1]);
    }
}
