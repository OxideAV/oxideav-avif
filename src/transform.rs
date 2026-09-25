//! Post-decode geometric transforms for AVIF image items — HEIF §6.5.9
//! (`clap`), §6.5.10 (`irot`) and §6.5.12 (`imir`) — applied by the
//! container crate's composition layer ([`oxideav_heif::compose`]).
//!
//! The canonical order once an AV1 frame has been reconstructed is:
//!
//!   1. Crop to the `ispe` declared size if the coded frame was padded
//!      to alignment ([`crop_top_left`] — AV1 codes odd 4:2:0 / 4:2:2
//!      pictures with ceiling chroma extents, so this trim keeps the
//!      subsampled layout; av1-avif §2.2.2).
//!   2. Apply `clap`.
//!   3. Apply `irot`.
//!   4. Apply `imir`.
//!
//! Each entry point takes the source frame plus the stream-level
//! `(format, width, height)` triple and returns a freshly-allocated
//! frame plus its new `(width, height)`; the source is left untouched.
//! MIAF §7.3.6.7 makes an odd clean aperture (or an odd crop edge) on
//! a chroma-subsampled picture implicitly upsample to 4:4:4, and the
//! container applies the same promotion when a rotation or mirror
//! would need a chroma sample that does not exist; these three entry
//! points cannot express a layout change through their `(frame, width,
//! height)` result, so such a call is refused with
//! [`AvifError::Unsupported`](crate::error::AvifError::Unsupported)
//! — the decoder composes on the container frame directly and does
//! carry the promoted layout through.
//!
//! A degenerate `clap` (zero denominator, or a clean aperture that does
//! not fit the picture) is treated as absent (pass-through) rather
//! than an error, as this crate always has.

use oxideav_heif::compose;
use oxideav_heif::props as hprops;

use crate::error::{AvifError as Error, Result};
use crate::frame_bridge::{from_heif, to_heif};
use crate::image::{
    AvifFrame as VideoFrame, AvifPixelFormat as PixelFormat, AvifPlane as VideoPlane,
};

use crate::meta::{Clap, Imir, Irot};

fn subsampling(format: PixelFormat) -> (u8, u8) {
    format.chroma_subsampling()
}

fn plane_count(format: PixelFormat) -> usize {
    format.plane_count()
}

fn pixel_bytes(format: PixelFormat) -> usize {
    format.bytes_per_sample() * if format.is_packed_ya() { 2 } else { 1 }
}

fn plane_is_full_res(plane: usize) -> bool {
    plane == 0 || plane == 3
}

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

/// Crop the top-left `out_w × out_h` window out of a coded picture —
/// the trim from the AV1 coded extents to the `ispe` extents. The
/// layout is preserved: subsampled chroma planes keep their ceiling
/// extents exactly as AV1 codes an odd-sized 4:2:0 / 4:2:2 picture.
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
        let (pw, ph) = if plane_is_full_res(p) {
            (out_w, out_h)
        } else {
            (
                ((out_w + (1 << sx) - 1) >> sx).max(1),
                ((out_h + (1 << sy) - 1) >> sy).max(1),
            )
        };
        let src = &frame.planes[p];
        let (plane_w, _plane_h) = plane_dims(format, width, height, p)?;
        let mut data = Vec::with_capacity((pw as usize) * (ph as usize) * unit);
        for row in 0..ph as usize {
            let src_row = row * src.stride;
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

/// The clean-aperture size a `clap` asks for, or `None` when the
/// property is degenerate (zero denominator) or does not fit the
/// `width × height` picture — cases this crate passes through.
pub(crate) fn clap_extent(clap: &Clap, width: u32, height: u32) -> Option<(u32, u32)> {
    if clap.clean_aperture_width_d == 0
        || clap.clean_aperture_height_d == 0
        || clap.horiz_off_d == 0
        || clap.vert_off_d == 0
    {
        return None;
    }
    let cw_num = clap.clean_aperture_width_n as i64;
    let cw_den = clap.clean_aperture_width_d as i64;
    let ch_num = clap.clean_aperture_height_n as i64;
    let ch_den = clap.clean_aperture_height_d as i64;
    let cw = (cw_num + cw_den / 2) / cw_den;
    let ch = (ch_num + ch_den / 2) / ch_den;
    if cw <= 0 || ch <= 0 || cw > i64::from(width) || ch > i64::from(height) {
        return None;
    }
    Some((cw as u32, ch as u32))
}

/// Run one container transform on a crate-local frame and hand the
/// result back in the same layout, refusing a layout change (the
/// implicit 4:4:4 promotion) that the three-tuple API cannot carry.
fn same_layout_transform(
    frame: &VideoFrame,
    format: PixelFormat,
    width: u32,
    height: u32,
    what: &str,
    op: impl FnOnce(&oxideav_heif::HeifFrame) -> oxideav_heif::Result<oxideav_heif::HeifFrame>,
) -> Result<(VideoFrame, u32, u32)> {
    let depth = format.bit_depth();
    let input = to_heif(frame, format, depth, width, height)?;
    let output = op(&input)?;
    if output.format != input.format {
        return Err(Error::unsupported(format!(
            "avif {what}: {format:?} {width}x{height} needs the implicit 4:4:4 promotion \
             (MIAF §7.3.6.7) — the result layout {:?} cannot be returned through this call; \
             decode through the decoder, which carries the promoted layout",
            output.format
        )));
    }
    let (w, h) = (output.width, output.height);
    let (mut out, _) = from_heif(&output)?;
    out.pts = frame.pts;
    Ok((out, w, h))
}

/// Apply a `clap` (HEIF §6.5.9). Degenerate / non-fitting apertures
/// pass the frame through unchanged.
pub fn apply_clap(
    frame: &VideoFrame,
    format: PixelFormat,
    width: u32,
    height: u32,
    clap: &Clap,
) -> Result<(VideoFrame, u32, u32)> {
    if clap_extent(clap, width, height).is_none() {
        return Ok((frame.clone(), width, height));
    }
    let hclap = hprops::Clap::from(clap);
    same_layout_transform(frame, format, width, height, "clap", |f| {
        compose::apply_clap(f, &hclap)
    })
}

/// A quarter turn of a picture with asymmetric chroma subsampling
/// (4:2:2) would need chroma subsampled vertically instead — no such
/// layout exists; MIAF §7.3.6.7 NOTE 2 leaves the chroma re-sampling
/// after rotation to the reader, and the container rotates the chroma
/// planes in place without promoting, so this crate refuses the case
/// (`(sx, sy)` chroma shifts, `turns` in `1..=3`).
pub(crate) fn rotation_keeps_layout((sx, sy): (u8, u8), turns: u8) -> Result<()> {
    if (turns & 1) == 1 && sx != sy {
        return Err(Error::unsupported(format!(
            "avif irot: {}° rotation of a 4:2:2 picture requires symmetric subsampling",
            u32::from(turns) * 90
        )));
    }
    Ok(())
}

/// Apply an `irot` (HEIF §6.5.10): `angle × 90°` anti-clockwise.
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
    rotation_keeps_layout(format.chroma_subsampling(), turns)?;
    let hirot = hprops::Irot { angle: turns };
    same_layout_transform(frame, format, width, height, "irot", |f| {
        compose::apply_irot(f, &hirot)
    })
}

/// Apply an `imir` (HEIF §6.5.12): axis 0 exchanges top and bottom,
/// axis 1 exchanges left and right.
pub fn apply_imir(
    frame: &VideoFrame,
    format: PixelFormat,
    width: u32,
    height: u32,
    imir: &Imir,
) -> Result<(VideoFrame, u32, u32)> {
    let himir = hprops::Imir {
        axis: imir.axis & 0x01,
    };
    same_layout_transform(frame, format, width, height, "imir", |f| {
        compose::apply_imir(f, &himir)
    })
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

    /// Fuzz regression (derived_graph_decode crash): cropping an odd
    /// luma extent out of a 4:2:0 frame must keep the ceiling chroma
    /// extent (3 luma columns → 2 chroma columns), so the following
    /// mirror reads inside the plane instead of panicking.
    #[test]
    fn odd_crop_keeps_ceiling_chroma_then_mirrors() {
        let frame = VideoFrame {
            pts: None,
            planes: vec![
                VideoPlane {
                    stride: 8,
                    data: (0..64).map(|v| v as u8).collect(),
                },
                VideoPlane {
                    stride: 4,
                    data: vec![1; 16],
                },
                VideoPlane {
                    stride: 4,
                    data: vec![2; 16],
                },
            ],
        };
        let cropped = crop_top_left(&frame, PixelFormat::Yuv420P, 8, 8, 3, 1).expect("crop");
        assert_eq!(cropped.planes[0].data.len(), 3);
        assert_eq!(
            cropped.planes[1].data.len(),
            2,
            "ceil(3 / 2) chroma columns"
        );
        assert_eq!(cropped.planes[1].stride, 2);
        let (mirrored, w, h) =
            apply_imir(&cropped, PixelFormat::Yuv420P, 3, 1, &Imir { axis: 1 }).expect("imir");
        assert_eq!((w, h), (3, 1));
        assert_eq!(mirrored.planes[0].data, vec![2, 1, 0]);
        assert_eq!(mirrored.planes[1].data.len(), 2);
    }
}
