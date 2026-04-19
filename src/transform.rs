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
//! Each entry point returns a freshly-allocated frame whose planes have
//! been rewritten — the source frame is left untouched. The `VideoFrame`
//! layout matches what `oxideav-av1` emits: one 8-bit plane per channel
//! (Y, U, V for YUV420P / Yuv422P / Yuv444P, single plane for Gray8).
//! Higher bit-depth + RGB-packed formats are not produced by the
//! underlying AV1 decoder today, so they land on a passthrough path.
//!
//! The transforms are strictly pixel-level operations — they do not
//! understand chroma siting or BT.709 vs. full-range semantics, both of
//! which are orthogonal to geometric manipulation.

use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{Error, PixelFormat, Result};

use crate::meta::{Clap, Imir, Irot};

/// Return the `(horizontal, vertical)` chroma subsampling shifts for a
/// pixel format. `0` means no subsampling on that axis. Errors on formats
/// the AV1 decoder does not produce.
fn subsampling(format: PixelFormat) -> Result<(u8, u8)> {
    match format {
        PixelFormat::Yuv420P => Ok((1, 1)),
        PixelFormat::Yuv422P => Ok((1, 0)),
        PixelFormat::Yuv444P => Ok((0, 0)),
        PixelFormat::Gray8 => Ok((0, 0)),
        other => Err(Error::unsupported(format!(
            "avif transform: unsupported pixel format {other:?}"
        ))),
    }
}

/// Return the number of planes that ride on this pixel format — one for
/// gray, three for the planar YUV variants the AV1 decoder emits.
fn plane_count(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Gray8 => 1,
        _ => 3,
    }
}

/// Per-plane pixel dimensions for a VideoFrame.
fn plane_dims(frame: &VideoFrame, plane: usize) -> Result<(u32, u32)> {
    let (sx, sy) = subsampling(frame.format)?;
    if plane == 0 {
        Ok((frame.width, frame.height))
    } else {
        let w = (frame.width + (1 << sx) - 1) >> sx;
        let h = (frame.height + (1 << sy) - 1) >> sy;
        Ok((w.max(1), h.max(1)))
    }
}

/// Crop every plane of `frame` to the top-left `out_w × out_h` pixels.
/// Used both by `clap` application and by the ispe-vs-coded-size clamp
/// on padded frames.
pub fn crop_top_left(frame: &VideoFrame, out_w: u32, out_h: u32) -> Result<VideoFrame> {
    if out_w == 0 || out_h == 0 {
        return Err(Error::invalid("avif: crop to zero dims"));
    }
    if out_w > frame.width || out_h > frame.height {
        return Err(Error::invalid(format!(
            "avif: crop {}x{} exceeds source {}x{}",
            out_w, out_h, frame.width, frame.height
        )));
    }
    if out_w == frame.width && out_h == frame.height {
        return Ok(frame.clone());
    }
    crop_rect(frame, 0, 0, out_w, out_h)
}

/// Generic rectangular crop. Offsets and size are expressed in luma
/// coordinates; chroma planes are scaled down by their subsampling.
/// Every dimension must respect the chroma subsampling (i.e. on Yuv420P
/// the offsets and sizes must be even).
fn crop_rect(frame: &VideoFrame, x: u32, y: u32, w: u32, h: u32) -> Result<VideoFrame> {
    let (sx, sy) = subsampling(frame.format)?;
    let planes = plane_count(frame.format);
    if frame.planes.len() != planes {
        return Err(Error::invalid(format!(
            "avif: frame has {} planes, expected {planes} for {:?}",
            frame.planes.len(),
            frame.format
        )));
    }
    let mut out = Vec::with_capacity(planes);
    for p in 0..planes {
        let (px, py, pw, ph) = if p == 0 {
            (x, y, w, h)
        } else {
            (x >> sx, y >> sy, (w >> sx).max(1), (h >> sy).max(1))
        };
        let src = &frame.planes[p];
        let src_stride = src.stride;
        let (plane_w, _plane_h) = plane_dims(frame, p)?;
        let mut data = Vec::with_capacity((pw as usize) * (ph as usize));
        for row in 0..ph as usize {
            let src_row = (py as usize + row) * src_stride + px as usize;
            let end = src_row + pw as usize;
            if end > src.data.len() {
                return Err(Error::invalid(format!(
                    "avif: crop row {row} reads past plane {p} of width {plane_w}"
                )));
            }
            data.extend_from_slice(&src.data[src_row..end]);
        }
        out.push(VideoPlane {
            stride: pw as usize,
            data,
        });
    }
    Ok(VideoFrame {
        format: frame.format,
        width: w,
        height: h,
        pts: frame.pts,
        time_base: frame.time_base,
        planes: out,
    })
}

/// Apply a `clap` (clean-aperture) crop. Dimensions that fall outside
/// the source rectangle or whose denominators are zero return the input
/// unchanged (matches goavif's defensive behaviour).
///
/// `clap` crop width / height / horizontal / vertical offsets are signed
/// rationals. The spec defines the crop centre as
/// `((W - 1) / 2 + horizOff, (H - 1) / 2 + vertOff)`, and the crop is
/// `cleanApertureWidth × cleanApertureHeight` pixels.
pub fn apply_clap(frame: &VideoFrame, clap: &Clap) -> Result<VideoFrame> {
    if clap.clean_aperture_width_d == 0
        || clap.clean_aperture_height_d == 0
        || clap.horiz_off_d == 0
        || clap.vert_off_d == 0
    {
        return Ok(frame.clone());
    }
    let w = frame.width as i64;
    let h = frame.height as i64;
    // Crop width / height rounded to nearest integer.
    let cw_num = clap.clean_aperture_width_n as i64;
    let cw_den = clap.clean_aperture_width_d as i64;
    let ch_num = clap.clean_aperture_height_n as i64;
    let ch_den = clap.clean_aperture_height_d as i64;
    let cw = (cw_num + cw_den / 2) / cw_den;
    let ch = (ch_num + ch_den / 2) / ch_den;
    if cw <= 0 || ch <= 0 || cw > w || ch > h {
        return Ok(frame.clone());
    }
    // Centre, as a float (matches goavif's rounding exactly; denominators
    // are 32-bit so f64 has enough precision).
    let centre_x = (w - 1) as f64 / 2.0
        + clap.horiz_off_n as f64 / clap.horiz_off_d as f64;
    let centre_y = (h - 1) as f64 / 2.0
        + clap.vert_off_n as f64 / clap.vert_off_d as f64;
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
    let (sx, sy) = subsampling(frame.format)?;
    let align_x = 1i64 << sx;
    let align_y = 1i64 << sy;
    x0 -= x0 % align_x;
    y0 -= y0 % align_y;
    let cw_aligned = cw - (cw % align_x);
    let ch_aligned = ch - (ch % align_y);
    if cw_aligned <= 0 || ch_aligned <= 0 {
        return Ok(frame.clone());
    }
    crop_rect(
        frame,
        x0 as u32,
        y0 as u32,
        cw_aligned as u32,
        ch_aligned as u32,
    )
}

/// Apply an `irot` rotation (counter-clockwise, 0..3 × 90°). Rotating by
/// 90° or 270° swaps the width and height. Chroma subsampling stays the
/// same — a Yuv420P input returns a Yuv420P output with swapped chroma
/// dims.
pub fn apply_irot(frame: &VideoFrame, irot: &Irot) -> Result<VideoFrame> {
    let turns = irot.angle & 0x03;
    if turns == 0 {
        return Ok(frame.clone());
    }
    let (sx, sy) = subsampling(frame.format)?;
    let planes = plane_count(frame.format);
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
            frame.format
        )));
    }
    let mut out_planes = Vec::with_capacity(planes);
    for p in 0..planes {
        let (pw, ph) = plane_dims(frame, p)?;
        let src = &frame.planes[p];
        let (ow, oh) = if odd { (ph, pw) } else { (pw, ph) };
        let mut data = vec![0u8; (ow as usize) * (oh as usize)];
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
                let si = src_y * src.stride + src_x;
                data[oy * ow as usize + ox] = src.data[si];
            }
        }
        out_planes.push(VideoPlane {
            stride: ow as usize,
            data,
        });
    }
    let (new_w, new_h) = if odd {
        (frame.height, frame.width)
    } else {
        (frame.width, frame.height)
    };
    Ok(VideoFrame {
        format: frame.format,
        width: new_w,
        height: new_h,
        pts: frame.pts,
        time_base: frame.time_base,
        planes: out_planes,
    })
}

/// Apply an `imir` mirror. `axis == 0` flips top↔bottom, `axis == 1`
/// flips left↔right. This matches the AVIF 1.1 / HEIF convention.
pub fn apply_imir(frame: &VideoFrame, imir: &Imir) -> Result<VideoFrame> {
    let axis = imir.axis & 0x01;
    let _ = subsampling(frame.format)?; // validate format
    let planes = plane_count(frame.format);
    if frame.planes.len() != planes {
        return Err(Error::invalid(format!(
            "avif imir: frame has {} planes, expected {planes}",
            frame.planes.len()
        )));
    }
    let mut out_planes = Vec::with_capacity(planes);
    for p in 0..planes {
        let (pw, ph) = plane_dims(frame, p)?;
        let src = &frame.planes[p];
        let mut data = vec![0u8; (pw as usize) * (ph as usize)];
        for y in 0..ph as usize {
            for x in 0..pw as usize {
                let (sx, sy) = if axis == 1 {
                    (pw as usize - 1 - x, y)
                } else {
                    (x, ph as usize - 1 - y)
                };
                let si = sy * src.stride + sx;
                data[y * pw as usize + x] = src.data[si];
            }
        }
        out_planes.push(VideoPlane {
            stride: pw as usize,
            data,
        });
    }
    Ok(VideoFrame {
        format: frame.format,
        width: frame.width,
        height: frame.height,
        pts: frame.pts,
        time_base: frame.time_base,
        planes: out_planes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::TimeBase;

    fn make_gray(w: u32, h: u32, fill: impl Fn(u32, u32) -> u8) -> VideoFrame {
        let mut data = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                data.push(fill(x, y));
            }
        }
        VideoFrame {
            format: PixelFormat::Gray8,
            width: w,
            height: h,
            pts: None,
            time_base: TimeBase::new(1, 1),
            planes: vec![VideoPlane {
                stride: w as usize,
                data,
            }],
        }
    }

    fn make_yuv420(w: u32, h: u32) -> VideoFrame {
        assert!(w % 2 == 0 && h % 2 == 0);
        let y: Vec<u8> = (0..w * h).map(|i| (i & 0xff) as u8).collect();
        let u: Vec<u8> = (0..(w / 2) * (h / 2)).map(|i| ((i + 40) & 0xff) as u8).collect();
        let v: Vec<u8> = (0..(w / 2) * (h / 2)).map(|i| ((i + 80) & 0xff) as u8).collect();
        VideoFrame {
            format: PixelFormat::Yuv420P,
            width: w,
            height: h,
            pts: None,
            time_base: TimeBase::new(1, 1),
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
        let out = apply_irot(&f, &Irot { angle: 0 }).unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 2);
        assert_eq!(out.planes[0].data, f.planes[0].data);
    }

    #[test]
    fn irot_90_swaps_dims() {
        // 2x3 with distinct pixel values.
        //  0 1
        //  2 3
        //  4 5
        let f = make_gray(2, 3, |x, y| (y * 2 + x) as u8);
        let out = apply_irot(&f, &Irot { angle: 1 }).unwrap();
        assert_eq!(out.width, 3);
        assert_eq!(out.height, 2);
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
        let out = apply_irot(&f, &Irot { angle: 2 }).unwrap();
        // original: 0 1 / 2 3   -> 180°: 3 2 / 1 0
        assert_eq!(out.planes[0].data, vec![3, 2, 1, 0]);
    }

    #[test]
    fn irot_270_swaps_dims_clockwise() {
        let f = make_gray(2, 3, |x, y| (y * 2 + x) as u8);
        let out = apply_irot(&f, &Irot { angle: 3 }).unwrap();
        assert_eq!(out.width, 3);
        assert_eq!(out.height, 2);
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
        f.format = PixelFormat::Yuv422P;
        // Repoint chroma planes to match 4:2:2 dims (2x4).
        f.planes[1].stride = 2;
        f.planes[1].data = vec![0u8; 2 * 4];
        f.planes[2].stride = 2;
        f.planes[2].data = vec![0u8; 2 * 4];
        let err = apply_irot(&f, &Irot { angle: 1 }).unwrap_err();
        match err {
            Error::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn imir_horizontal() {
        let f = make_gray(3, 2, |x, y| (y * 3 + x) as u8);
        let out = apply_imir(&f, &Imir { axis: 1 }).unwrap();
        // flip left↔right: each row reversed
        assert_eq!(out.planes[0].data, vec![2, 1, 0, 5, 4, 3]);
    }

    #[test]
    fn imir_vertical() {
        let f = make_gray(3, 2, |x, y| (y * 3 + x) as u8);
        let out = apply_imir(&f, &Imir { axis: 0 }).unwrap();
        // flip top↔bottom: rows swapped
        assert_eq!(out.planes[0].data, vec![3, 4, 5, 0, 1, 2]);
    }

    #[test]
    fn crop_top_left_yuv420() {
        let f = make_yuv420(4, 4);
        let out = crop_top_left(&f, 2, 2).unwrap();
        assert_eq!(out.width, 2);
        assert_eq!(out.height, 2);
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
        let out = apply_clap(&f, &clap).unwrap();
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
        let out = apply_clap(&f, &clap).unwrap();
        assert_eq!(out.width, 2);
        assert_eq!(out.height, 2);
        // Centre of 4x4 is (1.5, 1.5); crop top-left floor(1.5 - 0.5 + 0.5)=1.
        // So the crop is x=1, y=1, 2x2 -> pixels (1,1), (2,1), (1,2), (2,2).
        // Those are 5, 6, 9, 10.
        assert_eq!(out.planes[0].data, vec![5, 6, 9, 10]);
    }
}
