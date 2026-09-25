//! `iovl` image-overlay pixel composition — HEIF §6.6.2.2 (canvas +
//! per-input offsets + layering order) rendered with the §6.9.1
//! alpha-plane semantics, at 8, 10 and 12 bits, framework-free.
//!
//! The reconstructed image of an `iovl` derived item is a canvas of
//! `output_width × output_height` pixels, initialised to
//! `canvas_fill_value` (RGBA, sRGB, 16-bit per channel — §6.6.2.2.3)
//! and painted with every input image in `dimg` order, bottom-most
//! first (§6.6.2.2.1). An input lands with its top-left corner at
//! `(horizontal_offset, vertical_offset)`; canvas pixels outside
//! `[0, output_width) × [0, output_height)` are not part of the
//! reconstructed image (§6.6.2.2.3), so an input is clipped against
//! the canvas.
//!
//! When an input carries an alpha plane (an `auxl` auxiliary composited
//! into a `Yuva*` / `Ya*` layout upstream), it is rendered onto the
//! canvas with the §6.9.1 "visual context" update:
//!
//! * straight (no `prem` reference): `vu = m × α + vi × (1 − α)`
//! * pre-multiplied (`prem` present): `vu = m + vi × (1 − α)`
//!
//! with `α` the alpha sample scaled to `[0, 1]`. The canvas itself
//! carries an opacity channel seeded from the fill's `A` (0 = fully
//! transparent … 65535 = fully opaque, §6.6.2.2.3); it is updated
//! with the usual `α + a × (1 − α)`, and the colour channels are kept
//! pre-multiplied internally so that the §6.9.1 formula holds
//! exactly on an opaque context and generalises to a transparent
//! fill. The result is un-premultiplied at the end. When the fill is
//! fully opaque the output is the inputs' alpha-less layout; a
//! translucent or transparent fill yields the matching `Yuva*` /
//! `Ya*` layout carrying the canvas opacity at the coded depth.
//!
//! Every input must share one colour layout (bit depth, chroma
//! subsampling, colour-vs-monochrome) — HEIF §6.6.1 NOTE leaves the
//! colour spaces of overlay inputs unconstrained and defers to derived
//! specifications; this crate constrains them to a common coded
//! layout and reports anything else as unsupported rather than
//! converting.
//!
//! The composition itself is the container crate's
//! ([`oxideav_heif::compose::composite_overlay`]); this module maps
//! the crate's frame model onto it and keeps the AVIF-side guards
//! (canvas bound, one shared input layout).
//!
//! **Fill colour.** `canvas_fill_value` is sRGB RGBA; it is converted
//! to the output's coded colour space with the H.273 matrix the
//! `colr` names (MIAF §7.3.6.4 default when absent), at the coded
//! depth.
//!
//! **Chroma-subsampled inputs.** §6.6.2.2.3 places inputs in luma
//! pixel units and is silent about chroma planes. An input placed at a
//! sub-sample chroma position (odd offset on a subsampled axis), or an
//! input carrying an alpha plane, promotes the composition to 4:4:4
//! (MIAF §7.3.6.7's implicit upsampling rule applied to the overlay
//! canvas); the result then stays 4:4:4.

use oxideav_heif::compose;

use crate::cicp::CicpTriple;
use crate::derived::ImageOverlay;
use crate::error::{AvifError as Error, Result};
use crate::frame_bridge::{demote_to_mono, from_heif, to_heif};
use crate::image::{AvifFrame, AvifPixelFormat, AvifPlane};

/// Upper bound on an overlay canvas (`output_width × output_height`),
/// in pixels — 8192 × 8192. The compositor keeps a pre-multiplied
/// 32-bit colour + 16-bit opacity per canvas sample, so the bound
/// keeps a hostile descriptor from claiming gigabytes; larger
/// descriptors are rejected before any allocation.
pub const MAX_OVERLAY_CANVAS_PIXELS: u64 = 1 << 26;

/// One input image of an overlay, in `dimg` order (bottom-most first).
#[derive(Clone, Copy, Debug)]
pub struct OverlayInput<'a> {
    /// The input's output image (HEIF §6.3) — its own transforms and
    /// alpha auxiliary already applied.
    pub frame: &'a AvifFrame,
    /// The frame's layout; a `Yuva*` / `Ya*` layout carries the
    /// input's alpha plane.
    pub format: AvifPixelFormat,
    /// Coded bit depth (8 / 10 / 12). Needed because the packed
    /// `Ya16Le` layout does not encode it.
    pub bit_depth: u8,
    /// Output-image width in luma pixels.
    pub width: u32,
    /// Output-image height in luma pixels.
    pub height: u32,
    /// `horizontal_offset` from the `iovl` descriptor.
    pub offset_x: i64,
    /// `vertical_offset` from the `iovl` descriptor.
    pub offset_y: i64,
    /// True when the input's colour samples are pre-multiplied by its
    /// alpha (`prem` item reference, §6.9.1).
    pub premultiplied: bool,
}

/// A frame unpacked into `u16` sample planes at the coded depth —
/// the working representation of the compositor. Public for the
/// decoder's other composition steps; not a stable surface.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SamplePlanes {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub sx: u8,
    pub sy: u8,
    pub gray: bool,
    pub y: Vec<u16>,
    pub u: Vec<u16>,
    pub v: Vec<u16>,
    /// Full-resolution alpha at the coded depth, when present.
    pub alpha: Option<Vec<u16>>,
}

impl SamplePlanes {
    /// Chroma plane dimensions (ceil-divided by the subsampling).
    pub fn chroma_dims(&self) -> (u32, u32) {
        chroma_dims(self.width, self.height, self.sx, self.sy)
    }

    /// The alpha-less layout these planes describe.
    pub fn base_format(&self) -> Result<AvifPixelFormat> {
        AvifPixelFormat::from_layout(self.bit_depth, self.sx, self.sy, self.gray, false)
            .ok_or_else(|| Error::unsupported("avif overlay: no pixel layout for these planes"))
    }
}

fn chroma_dims(width: u32, height: u32, sx: u8, sy: u8) -> (u32, u32) {
    let w = (width + (1 << sx) - 1) >> sx;
    let h = (height + (1 << sy) - 1) >> sy;
    (w.max(1), h.max(1))
}

fn read_sample(data: &[u8], at: usize, bps: usize) -> u16 {
    if bps == 1 {
        data[at] as u16
    } else {
        u16::from_le_bytes([data[at], data[at + 1]])
    }
}

fn read_plane(plane: &AvifPlane, w: u32, h: u32, bps: usize, label: &str) -> Result<Vec<u16>> {
    let (w, h) = (w as usize, h as usize);
    let row_bytes = w * bps;
    if plane.stride < row_bytes || plane.data.len() < plane.stride * (h - 1) + row_bytes {
        return Err(Error::invalid(format!(
            "avif overlay: {label} plane too short ({} bytes, stride {}, need {w}x{h}x{bps})",
            plane.data.len(),
            plane.stride
        )));
    }
    let mut out = Vec::with_capacity(w * h);
    for r in 0..h {
        let base = r * plane.stride;
        for c in 0..w {
            out.push(read_sample(&plane.data, base + c * bps, bps));
        }
    }
    Ok(out)
}

/// Unpack `frame` (any layout this crate emits) into [`SamplePlanes`].
#[doc(hidden)]
pub fn unpack_planes(
    frame: &AvifFrame,
    format: AvifPixelFormat,
    bit_depth: u8,
    width: u32,
    height: u32,
) -> Result<SamplePlanes> {
    if width == 0 || height == 0 {
        return Err(Error::invalid("avif overlay: zero-sized input"));
    }
    let bps = format.bytes_per_sample();
    let (sx, sy) = format.chroma_subsampling();
    let expect_planes = format.plane_count();
    if frame.planes.len() != expect_planes {
        return Err(Error::invalid(format!(
            "avif overlay: {format:?} frame carries {} planes, expected {expect_planes}",
            frame.planes.len()
        )));
    }
    let depth = if format == AvifPixelFormat::Ya16Le {
        bit_depth
    } else {
        format.bit_depth()
    };
    if !matches!(depth, 8 | 10 | 12) {
        return Err(Error::unsupported(format!(
            "avif overlay: unsupported bit depth {depth}"
        )));
    }
    if format.is_packed_ya() {
        let p = &frame.planes[0];
        let (w, h) = (width as usize, height as usize);
        let px = 2 * bps;
        let row_bytes = w * px;
        if p.stride < row_bytes || p.data.len() < p.stride * (h - 1) + row_bytes {
            return Err(Error::invalid("avif overlay: packed YA plane too short"));
        }
        let mut y = Vec::with_capacity(w * h);
        let mut a = Vec::with_capacity(w * h);
        for r in 0..h {
            let base = r * p.stride;
            for c in 0..w {
                y.push(read_sample(&p.data, base + c * px, bps));
                a.push(read_sample(&p.data, base + c * px + bps, bps));
            }
        }
        return Ok(SamplePlanes {
            width,
            height,
            bit_depth: depth,
            sx: 0,
            sy: 0,
            gray: true,
            y,
            u: Vec::new(),
            v: Vec::new(),
            alpha: Some(a),
        });
    }
    let gray = expect_planes == 1;
    let y = read_plane(&frame.planes[0], width, height, bps, "Y")?;
    let (u, v) = if gray {
        (Vec::new(), Vec::new())
    } else {
        let (cw, ch) = chroma_dims(width, height, sx, sy);
        (
            read_plane(&frame.planes[1], cw, ch, bps, "U")?,
            read_plane(&frame.planes[2], cw, ch, bps, "V")?,
        )
    };
    let alpha = if expect_planes == 4 {
        Some(read_plane(&frame.planes[3], width, height, bps, "A")?)
    } else {
        None
    };
    Ok(SamplePlanes {
        width,
        height,
        bit_depth: depth,
        sx: if gray { 0 } else { sx },
        sy: if gray { 0 } else { sy },
        gray,
        y,
        u,
        v,
        alpha,
    })
}

fn write_plane(samples: &[u16], w: u32, bps: usize) -> AvifPlane {
    let stride = w as usize * bps;
    let mut data = Vec::with_capacity(samples.len() * bps);
    for &s in samples {
        if bps == 1 {
            data.push(s as u8);
        } else {
            data.extend_from_slice(&s.to_le_bytes());
        }
    }
    AvifPlane { stride, data }
}

/// Pack [`SamplePlanes`] back into an [`AvifFrame`]; returns the frame
/// and its layout (alpha appended when `planes.alpha` is present).
#[doc(hidden)]
pub fn pack_planes(planes: &SamplePlanes) -> Result<(AvifFrame, AvifPixelFormat)> {
    let has_alpha = planes.alpha.is_some();
    let format = AvifPixelFormat::from_layout(
        planes.bit_depth,
        planes.sx,
        planes.sy,
        planes.gray,
        has_alpha,
    )
    .ok_or_else(|| Error::unsupported("avif overlay: no pixel layout for these planes"))?;
    let bps = format.bytes_per_sample();
    let mut out = Vec::with_capacity(format.plane_count());
    if format.is_packed_ya() {
        let a = planes.alpha.as_ref().expect("packed YA needs alpha");
        let mut data = Vec::with_capacity(planes.y.len() * 2 * bps);
        for (&yv, &av) in planes.y.iter().zip(a) {
            if bps == 1 {
                data.push(yv as u8);
                data.push(av as u8);
            } else {
                data.extend_from_slice(&yv.to_le_bytes());
                data.extend_from_slice(&av.to_le_bytes());
            }
        }
        out.push(AvifPlane {
            stride: planes.width as usize * 2 * bps,
            data,
        });
    } else {
        out.push(write_plane(&planes.y, planes.width, bps));
        if !planes.gray {
            let (cw, _) = planes.chroma_dims();
            out.push(write_plane(&planes.u, cw, bps));
            out.push(write_plane(&planes.v, cw, bps));
        }
        if let Some(a) = &planes.alpha {
            out.push(write_plane(a, planes.width, bps));
        }
    }
    Ok((
        AvifFrame {
            pts: None,
            planes: out,
        },
        format,
    ))
}

/// Overlay composition on container frames (every input already one
/// shared chroma / depth layout): the container paints the canvas;
/// when every input is monochrome the result is kept monochrome — the
/// container promotes alpha-carrying monochrome inputs to 4:4:4 with
/// neutral chroma, which carries no colour and would turn a
/// monochrome AVIF into a colour one.
pub(crate) fn composite_overlay_frames(
    desc: &ImageOverlay,
    inputs: &[compose::OverlayInput<'_>],
    colr: &oxideav_heif::props::Colr,
) -> Result<oxideav_heif::HeifFrame> {
    let composed = compose::composite_overlay(&desc.descriptor(), inputs, Some(colr))?;
    let all_mono = inputs
        .iter()
        .all(|i| i.frame.format.chroma == oxideav_heif::Chroma::Mono);
    if all_mono && composed.format.chroma != oxideav_heif::Chroma::Mono {
        return demote_to_mono(&composed);
    }
    Ok(composed)
}

/// One decoded overlay input as the container crate composes it: its
/// frame, the layout the offsets of the descriptor place it at, and
/// whether its samples are pre-multiplied by its alpha (`prem`).
fn heif_input(inp: &OverlayInput<'_>, i: usize) -> Result<oxideav_heif::HeifFrame> {
    to_heif(inp.frame, inp.format, inp.bit_depth, inp.width, inp.height)
        .map_err(|e| Error::invalid(format!("avif overlay: input {i}: {e}")))
}

/// §6.6.2.2 overlay composition on this crate's frame model: `inputs`
/// in `dimg` order (bottom-most first), `cicp` the output's colour
/// information (the canvas fill is converted with its H.273 matrix).
/// Every input must share one layout (chroma, depth); the descriptor's
/// offsets place them. Returns the composed frame, its layout (with an
/// alpha plane when the fill is not fully opaque; promoted to 4:4:4
/// when an input carries alpha or sits at a sub-sample chroma
/// position) and the canvas extents.
pub fn composite_overlay(
    desc: &ImageOverlay,
    inputs: &[OverlayInput<'_>],
    cicp: &CicpTriple,
) -> Result<(AvifFrame, AvifPixelFormat, u32, u32)> {
    let (cw, ch) = (desc.output_width, desc.output_height);
    if cw == 0 || ch == 0 {
        return Err(Error::invalid("avif overlay: zero canvas dimensions"));
    }
    if u64::from(cw) * u64::from(ch) > MAX_OVERLAY_CANVAS_PIXELS {
        return Err(Error::invalid(format!(
            "avif overlay: canvas {cw}x{ch} exceeds {MAX_OVERLAY_CANVAS_PIXELS} pixels"
        )));
    }
    if inputs.len() != desc.entries.len() {
        return Err(Error::invalid(format!(
            "avif overlay: descriptor lists {} inputs, got {}",
            desc.entries.len(),
            inputs.len()
        )));
    }
    if inputs.is_empty() {
        return Err(Error::invalid("avif overlay: no input images"));
    }
    let mut frames = Vec::with_capacity(inputs.len());
    for (i, inp) in inputs.iter().enumerate() {
        let f = heif_input(inp, i)?;
        if i > 0 {
            let first: &oxideav_heif::HeifFrame = &frames[0];
            if (first.format.chroma, first.format.bit_depth)
                != (f.format.chroma, f.format.bit_depth)
            {
                return Err(Error::unsupported(format!(
                    "avif overlay: input {i} layout ({}-bit {:?}) differs from input 0 \
                     ({}-bit {:?}) — mixed layouts are not composited",
                    f.format.bit_depth,
                    f.format.chroma,
                    first.format.bit_depth,
                    first.format.chroma
                )));
            }
        }
        frames.push(f);
    }
    let heif_inputs: Vec<compose::OverlayInput<'_>> = frames
        .iter()
        .zip(inputs)
        .map(|(frame, inp)| compose::OverlayInput {
            frame,
            premultiplied: inp.premultiplied,
        })
        .collect();
    let colr = cicp.to_colr();
    let composed = composite_overlay_frames(desc, &heif_inputs, &colr)?;
    let (mut out, format) = from_heif(&composed)?;
    out.pts = inputs[0].frame.pts;
    Ok((out, format, composed.width, composed.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::OverlayEntry;

    #[allow(clippy::too_many_arguments)]
    fn planes(
        w: u32,
        h: u32,
        depth: u8,
        sx: u8,
        sy: u8,
        gray: bool,
        seed: u16,
        alpha: Option<u16>,
    ) -> SamplePlanes {
        let max = (1u16 << depth) - 1;
        let n = (w * h) as usize;
        let y: Vec<u16> = (0..n).map(|i| (i as u16 * 7 + seed) & max).collect();
        let (cw, ch) = chroma_dims(w, h, sx, sy);
        let cn = if gray { 0 } else { (cw * ch) as usize };
        let u: Vec<u16> = (0..cn).map(|i| (i as u16 * 3 + seed + 11) & max).collect();
        let v: Vec<u16> = (0..cn).map(|i| (i as u16 * 5 + seed + 23) & max).collect();
        SamplePlanes {
            width: w,
            height: h,
            bit_depth: depth,
            sx: if gray { 0 } else { sx },
            sy: if gray { 0 } else { sy },
            gray,
            y,
            u,
            v,
            alpha: alpha.map(|a| vec![a; n]),
        }
    }

    fn identity_full() -> CicpTriple {
        CicpTriple {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            full_range: true,
        }
    }

    fn bt709_limited() -> CicpTriple {
        CicpTriple {
            colour_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            full_range: false,
        }
    }

    fn desc(w: u32, h: u32, fill: [u16; 4], offsets: &[(i32, i32)]) -> ImageOverlay {
        ImageOverlay {
            canvas_fill_value: fill,
            output_width: w,
            output_height: h,
            entries: offsets
                .iter()
                .map(|&(x, y)| OverlayEntry {
                    horizontal_offset: x,
                    vertical_offset: y,
                })
                .collect(),
        }
    }

    /// Round-trip every layout through unpack/pack.
    #[test]
    fn pack_unpack_round_trips_every_layout() {
        for depth in [8u8, 10, 12] {
            for (sx, sy, gray) in [(1, 1, false), (1, 0, false), (0, 0, false), (0, 0, true)] {
                for alpha in [None, Some(77u16)] {
                    let p = planes(7, 5, depth, sx, sy, gray, 3, alpha);
                    let (frame, fmt) = pack_planes(&p).expect("pack");
                    assert!(fmt.bit_depth() == depth || fmt == AvifPixelFormat::Ya16Le);
                    let back = unpack_planes(&frame, fmt, depth, 7, 5).expect("unpack");
                    assert_eq!(
                        back, p,
                        "{depth}-bit sx={sx} sy={sy} gray={gray} alpha={alpha:?}"
                    );
                }
            }
        }
    }

    /// One opaque input at (0,0) covering the canvas: output == input.
    #[test]
    fn single_covering_input_is_copied_exactly() {
        for depth in [8u8, 10, 12] {
            let p = planes(8, 6, depth, 1, 1, false, 5, None);
            let (frame, fmt) = pack_planes(&p).unwrap();
            let inp = OverlayInput {
                frame: &frame,
                format: fmt,
                bit_depth: depth,
                width: 8,
                height: 6,
                offset_x: 0,
                offset_y: 0,
                premultiplied: false,
            };
            // BT.709 limited fill would be unconvertible — but it never
            // shows, so composition succeeds.
            let d = desc(8, 6, [1000, 2000, 3000, 65535], &[(0, 0)]);
            let (out, ofmt, w, h) = composite_overlay(&d, &[inp], &bt709_limited()).expect("ok");
            assert_eq!((w, h), (8, 6));
            assert_eq!(ofmt, fmt);
            let back = unpack_planes(&out, ofmt, depth, 8, 6).unwrap();
            assert_eq!(back, p, "{depth}-bit");
        }
    }

    /// Identity-matrix fill converts exactly (Y=G, Cb=B, Cr=R scaled),
    /// and a placed opaque input overrides it inside its rectangle.
    #[test]
    fn identity_fill_and_placement_8bit_444() {
        let p = planes(2, 2, 8, 0, 0, false, 40, None);
        let (frame, fmt) = pack_planes(&p).unwrap();
        let inp = OverlayInput {
            frame: &frame,
            format: fmt,
            bit_depth: 8,
            width: 2,
            height: 2,
            offset_x: 1,
            offset_y: 1,
            premultiplied: false,
        };
        // R=0xFFFF G=0x8000 B=0x0000 → Y(G)=128, Cb(B)=0, Cr(R)=255.
        let d = desc(4, 3, [0xFFFF, 0x8000, 0x0000, 65535], &[(1, 1)]);
        let (out, ofmt, w, h) = composite_overlay(&d, &[inp], &identity_full()).expect("ok");
        assert_eq!((w, h), (4, 3));
        assert_eq!(ofmt, AvifPixelFormat::Yuv444P);
        let o = unpack_planes(&out, ofmt, 8, 4, 3).unwrap();
        for y in 0..3u32 {
            for x in 0..4u32 {
                let i = (y * 4 + x) as usize;
                if (1..3).contains(&x) && (1..3).contains(&y) {
                    let j = ((y - 1) * 2 + (x - 1)) as usize;
                    assert_eq!(o.y[i], p.y[j], "Y at {x},{y}");
                    assert_eq!(o.u[i], p.u[j], "U at {x},{y}");
                    assert_eq!(o.v[i], p.v[j], "V at {x},{y}");
                } else {
                    assert_eq!((o.y[i], o.u[i], o.v[i]), (128, 0, 255), "fill at {x},{y}");
                }
            }
        }
    }

    /// A fill that shows and cannot be converted is refused.
    #[test]
    fn visible_fill_is_converted_with_the_output_matrix() {
        let p = planes(2, 2, 8, 1, 1, false, 1, None);
        let (frame, fmt) = pack_planes(&p).unwrap();
        let inp = OverlayInput {
            frame: &frame,
            format: fmt,
            bit_depth: 8,
            width: 2,
            height: 2,
            offset_x: 0,
            offset_y: 0,
            premultiplied: false,
        };
        // An opaque fill showing beside a BT.709 limited-range input is
        // converted with that matrix (H.273) — a dark near-black fill
        // lands on the limited-range black level, distinct from the
        // input's samples.
        let d = desc(4, 4, [1, 2, 3, 65535], &[(0, 0)]);
        let (out, ofmt, _, _) = composite_overlay(&d, &[inp], &bt709_limited()).expect("ok");
        assert_eq!(ofmt, AvifPixelFormat::Yuv420P);
        let o = unpack_planes(&out, ofmt, 8, 4, 4).unwrap();
        assert_eq!(o.y[0], p.y[0], "input copied at (0,0)");
        assert_eq!(
            o.y[15], 16,
            "fill converted to the limited-range black level"
        );
        assert_eq!((o.u[3], o.v[3]), (128, 128), "fill chroma neutral");
        // Fully transparent fill: colour irrelevant → accepted, output
        // carries alpha.
        let d = desc(4, 4, [1, 2, 3, 0], &[(0, 0)]);
        let (out, ofmt, _, _) = composite_overlay(&d, &[inp], &bt709_limited()).expect("ok");
        assert_eq!(ofmt, AvifPixelFormat::Yuva420P);
        let o = unpack_planes(&out, ofmt, 8, 4, 4).unwrap();
        let a = o.alpha.unwrap();
        assert_eq!(a[0], 255);
        assert_eq!(a[15], 0);
    }

    /// Straight alpha over an opaque neutral fill on a monochrome
    /// master: vu = m·α + vi·(1−α) (HEIF §6.9.1), checked at three
    /// depths against an independent float evaluation.
    #[test]
    fn straight_alpha_over_matches_section_6_9_1() {
        for depth in [8u8, 10, 12] {
            let max = (1u32 << depth) - 1;
            let mut p = planes(4, 1, depth, 0, 0, true, 9, None);
            let alphas: Vec<u16> = vec![0, (max / 4) as u16, (max / 2) as u16, max as u16];
            p.alpha = Some(alphas.clone());
            p.y = vec![max as u16, max as u16, max as u16, max as u16];
            let (frame, fmt) = pack_planes(&p).unwrap();
            let inp = OverlayInput {
                frame: &frame,
                format: fmt,
                bit_depth: depth,
                width: 4,
                height: 1,
                offset_x: 0,
                offset_y: 0,
                premultiplied: false,
            };
            // Neutral 25% grey fill, opaque.
            let d = desc(4, 1, [16384, 16384, 16384, 65535], &[(0, 0)]);
            let (out, ofmt, _, _) = composite_overlay(&d, &[inp], &identity_full()).expect("ok");
            assert_eq!(ofmt.plane_count(), 1);
            assert!(!ofmt.has_alpha());
            let o = unpack_planes(&out, ofmt, depth, 4, 1).unwrap();
            let fill = f64::from(16384u16 >> (16 - depth));
            for (i, &a) in alphas.iter().enumerate() {
                let alpha = a as f64 / max as f64;
                let expect = (max as f64 * alpha + fill * (1.0 - alpha)).round() as i64;
                assert!(
                    (o.y[i] as i64 - expect).abs() <= 1,
                    "{depth}-bit sample {i}: got {} expected {expect}",
                    o.y[i]
                );
            }
        }
    }

    /// Pre-multiplied input: vu = m + vi·(1−α).
    #[test]
    fn premultiplied_alpha_over() {
        let mut p = planes(2, 1, 8, 0, 0, true, 0, None);
        p.y = vec![100, 100];
        p.alpha = Some(vec![128, 255]);
        let (frame, fmt) = pack_planes(&p).unwrap();
        let inp = OverlayInput {
            frame: &frame,
            format: fmt,
            bit_depth: 8,
            width: 2,
            height: 1,
            offset_x: 0,
            offset_y: 0,
            premultiplied: true,
        };
        let d = desc(2, 1, [65535, 65535, 65535, 65535], &[(0, 0)]);
        let (out, ofmt, _, _) = composite_overlay(&d, &[inp], &identity_full()).expect("ok");
        let o = unpack_planes(&out, ofmt, 8, 2, 1).unwrap();
        // 100 + 255·(1 − 128/255) = 227; 100 + 0 = 100.
        assert_eq!(o.y[1], 100);
        assert!((o.y[0] as i32 - 227).abs() <= 1, "{}", o.y[0]);
    }

    /// Layering order: the later (top-most) input wins where inputs
    /// overlap; negative and beyond-canvas offsets clip.
    #[test]
    fn layering_and_clipping() {
        let a = planes(3, 3, 8, 1, 1, false, 10, None);
        let b = planes(3, 3, 8, 1, 1, false, 200, None);
        let (fa, fmt) = pack_planes(&a).unwrap();
        let (fb, _) = pack_planes(&b).unwrap();
        let mk = |frame, x, y| OverlayInput {
            frame,
            format: fmt,
            bit_depth: 8,
            width: 3,
            height: 3,
            offset_x: x,
            offset_y: y,
            premultiplied: false,
        };
        // a at (-1,-1) covers canvas [0,2)²; b at (1,1) covers [1,3)².
        let d = desc(3, 3, [0, 0, 0, 0], &[(-1, -1), (1, 1)]);
        let (out, ofmt, _, _) =
            composite_overlay(&d, &[mk(&fa, -1, -1), mk(&fb, 1, 1)], &bt709_limited()).expect("ok");
        let o = unpack_planes(&out, ofmt, 8, 3, 3).unwrap();
        let al = o.alpha.as_ref().unwrap();
        // (0,0): a's (1,1).
        assert_eq!(o.y[0], a.y[4]);
        // (1,1): b's (0,0) on top.
        assert_eq!(o.y[4], b.y[0]);
        // (2,0): uncovered → transparent.
        assert_eq!(al[2], 0);
        assert_eq!(al[0], 255);
        // Off-canvas input contributes nothing.
        let d = desc(3, 3, [0, 0, 0, 0], &[(5, 5)]);
        let (out, ofmt, _, _) =
            composite_overlay(&d, &[mk(&fa, 5, 5)], &bt709_limited()).expect("ok");
        let o = unpack_planes(&out, ofmt, 8, 3, 3).unwrap();
        assert!(o.alpha.unwrap().iter().all(|&v| v == 0));
    }

    /// Mixed layouts are refused; hostile canvas / count mismatches too.
    #[test]
    fn guards() {
        let a = planes(2, 2, 8, 1, 1, false, 1, None);
        let b = planes(2, 2, 10, 1, 1, false, 1, None);
        let (fa, fmta) = pack_planes(&a).unwrap();
        let (fb, fmtb) = pack_planes(&b).unwrap();
        let ia = OverlayInput {
            frame: &fa,
            format: fmta,
            bit_depth: 8,
            width: 2,
            height: 2,
            offset_x: 0,
            offset_y: 0,
            premultiplied: false,
        };
        let ib = OverlayInput {
            frame: &fb,
            format: fmtb,
            bit_depth: 10,
            width: 2,
            height: 2,
            offset_x: 0,
            offset_y: 0,
            premultiplied: false,
        };
        let d = desc(2, 2, [0, 0, 0, 65535], &[(0, 0), (0, 0)]);
        assert!(composite_overlay(&d, &[ia, ib], &identity_full()).is_err());
        let d = desc(2, 2, [0, 0, 0, 65535], &[(0, 0)]);
        assert!(composite_overlay(&d, &[ia, ia], &identity_full()).is_err());
        let d = desc(0, 2, [0, 0, 0, 65535], &[(0, 0)]);
        assert!(composite_overlay(&d, &[ia], &identity_full()).is_err());
        let d = desc(65535, 65535, [0, 0, 0, 65535], &[(0, 0)]);
        assert!(composite_overlay(&d, &[ia], &identity_full()).is_err());
        assert!(composite_overlay(&desc(2, 2, [0; 4], &[]), &[], &identity_full()).is_err());
    }

    /// 4:2:0 input at an odd offset: the canvas is promoted to 4:4:4
    /// (the input would land on a sub-sample chroma position), luma is
    /// exact and the input's chroma is replicated to its luma positions.
    #[test]
    fn odd_offset_subsampled_input() {
        let p = planes(4, 4, 8, 1, 1, false, 33, None);
        let (frame, fmt) = pack_planes(&p).unwrap();
        let inp = OverlayInput {
            frame: &frame,
            format: fmt,
            bit_depth: 8,
            width: 4,
            height: 4,
            offset_x: 1,
            offset_y: 1,
            premultiplied: false,
        };
        let d = desc(6, 6, [0, 0, 0, 0], &[(1, 1)]);
        let (out, ofmt, _, _) = composite_overlay(&d, &[inp], &bt709_limited()).expect("ok");
        assert_eq!(
            ofmt,
            AvifPixelFormat::Yuva444P,
            "promoted; transparent fill → alpha"
        );
        let o = unpack_planes(&out, ofmt, 8, 6, 6).unwrap();
        let a = o.alpha.as_ref().unwrap();
        for y in 0..4usize {
            for x in 0..4usize {
                let i = (y + 1) * 6 + x + 1;
                assert_eq!(o.y[i], p.y[y * 4 + x]);
                // Input chroma (x/2, y/2) replicated to every covered
                // luma position.
                assert_eq!(o.u[i], p.u[(y / 2) * 2 + x / 2]);
                assert_eq!(o.v[i], p.v[(y / 2) * 2 + x / 2]);
                assert_eq!(a[i], 255);
            }
        }
        // Outside the input the transparent fill shows.
        assert_eq!(a[0], 0);
        assert_eq!(a[35], 0);
    }
}
