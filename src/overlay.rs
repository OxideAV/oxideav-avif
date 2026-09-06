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
//! **Fill colour.** `canvas_fill_value` is sRGB RGBA. Converting it to
//! the inputs' coded colour space is exact for the H.273 identity
//! matrix (`matrix_coefficients = 0`, full range: `Y = G`, `Cb = B`,
//! `Cr = R`, the 16-bit code values narrowed to the coded depth) and for a
//! full-range monochrome master with a neutral (`R = G = B`) fill.
//! Any other pairing needs the H.273 RGB → YCbCr equations, which this
//! module does not carry; the fill is then only accepted when it can
//! never show — i.e. when the canvas is fully covered by alpha-less
//! inputs, or the fill is fully transparent (`A = 0`, colour
//! irrelevant). Otherwise composition fails with
//! [`AvifError::Unsupported`](crate::error::AvifError::Unsupported).
//!
//! **Chroma-subsampled inputs.** §6.6.2.2.3 places inputs in luma
//! pixel units and is silent about chroma planes. This module maps
//! every canvas chroma sample to the input chroma sample co-located
//! with the first covered luma sample of its `(1 << sx) × (1 << sy)`
//! luma block (nearest-neighbour), and blends it with the block's
//! mean alpha — the §6.9.1 "alpha plane resized to the master's
//! extents" rule applied at the chroma plane's extent.

use crate::cicp::CicpTriple;
use crate::derived::ImageOverlay;
use crate::error::{AvifError as Error, Result};
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

/// Narrow a 16-bit fill colour channel to `bit_depth` bits by dropping
/// the low bits — a writer that thinks in 8 bits pads its code values
/// with zeros (`0xC0` → `0xC000`), and the shift reproduces them
/// exactly (a linear rescale would turn `0xC000` into 191).
fn scale16(v: u16, bit_depth: u8) -> u16 {
    v >> (16 - bit_depth)
}

/// Convert the sRGB RGBA `canvas_fill_value` to the inputs' coded
/// colour representation, where the staged specification pins the
/// conversion exactly. Returns `(Y, Cb, Cr)` at `bit_depth` (`Cb`/`Cr`
/// unused for monochrome), or `None` when the pairing needs the
/// H.273 RGB → YCbCr equations.
fn fill_samples(
    fill: [u16; 4],
    cicp: &CicpTriple,
    gray: bool,
    bit_depth: u8,
) -> Option<(u16, u16, u16)> {
    let [r, g, b, _] = fill;
    if !cicp.full_range {
        return None;
    }
    if gray {
        // A neutral fill on a full-range monochrome master: the
        // achromatic axis carries the same code value under every
        // H.273 matrix (Y = R = G = B when Kr + Kg + Kb = 1).
        if r == g && g == b {
            return Some((scale16(r, bit_depth), 0, 0));
        }
        return None;
    }
    if cicp.matrix_coefficients == 0 {
        // Identity: Y = G, Cb = B, Cr = R (H.273 §8.3, GBR order).
        return Some((
            scale16(g, bit_depth),
            scale16(b, bit_depth),
            scale16(r, bit_depth),
        ));
    }
    None
}

/// True when the union of `rects` (`(x, y, w, h)`) covers the whole
/// `cw × ch` canvas. Exact sweep over the distinct x boundaries —
/// O(n² log n), no per-pixel allocation.
fn rects_cover_canvas(rects: &[(u32, u32, u32, u32)], cw: u32, ch: u32) -> bool {
    if cw == 0 || ch == 0 {
        return true;
    }
    let mut xs: Vec<u32> = vec![0, cw];
    for &(x, _, w, _) in rects {
        xs.push(x.min(cw));
        xs.push((x + w).min(cw));
    }
    xs.sort_unstable();
    xs.dedup();
    for pair in xs.windows(2) {
        let (x0, x1) = (pair[0], pair[1]);
        if x1 <= x0 {
            continue;
        }
        // Every rect spanning this x-strip contributes a y-interval.
        let mut ys: Vec<(u32, u32)> = rects
            .iter()
            .filter(|&&(x, _, w, _)| x <= x0 && x + w >= x1)
            .map(|&(_, y, _, h)| (y.min(ch), (y + h).min(ch)))
            .collect();
        ys.sort_unstable();
        let mut reach = 0u32;
        for (y0, y1) in ys {
            if y0 > reach {
                return false;
            }
            reach = reach.max(y1);
        }
        if reach < ch {
            return false;
        }
    }
    true
}

/// Alpha sample at `bit_depth` → 16-bit opacity (`0..=65535`).
fn alpha16(a: u16, bit_depth: u8) -> u32 {
    let max = (1u32 << bit_depth) - 1;
    (u32::from(a) * 65535 + max / 2) / max
}

/// One "over" step on a pre-multiplied canvas colour sample. `prem`
/// is the canvas colour × opacity (65535-scaled); `m` the input
/// sample, `alpha` its 16-bit opacity. The canvas opacity is advanced
/// separately by [`over_alpha`] (once per pixel, however many colour
/// planes share it).
#[inline]
fn over(prem: &mut u32, m: u32, alpha: u32, premultiplied: bool) {
    let keep = 65535 - alpha;
    let carried = (u64::from(*prem) * u64::from(keep) + 32767) / 65535;
    let added = if premultiplied {
        u64::from(m) * 65535
    } else {
        u64::from(m) * u64::from(alpha)
    };
    *prem = (added + carried).min(u64::from(u32::MAX)) as u32;
}

/// Canvas opacity update: `α + a × (1 − α)`.
#[inline]
fn over_alpha(a: &mut u16, alpha: u32) {
    let keep = 65535 - alpha;
    let a_new = u64::from(alpha) + (u64::from(*a) * u64::from(keep) + 32767) / 65535;
    *a = a_new.min(65535) as u16;
}

/// Resolve a pre-multiplied canvas plane back to straight samples at
/// `bit_depth`; fully transparent samples take `fallback`.
fn unpremultiply(prem: &[u32], a: &[u16], bit_depth: u8, fallback: u16) -> Vec<u16> {
    let max = (1u32 << bit_depth) - 1;
    prem.iter()
        .zip(a)
        .map(|(&p, &a)| {
            if a == 0 {
                fallback
            } else {
                (((u64::from(p) + u64::from(a) / 2) / u64::from(a)) as u32).min(max) as u16
            }
        })
        .collect()
}

/// Composite an `iovl` overlay. `inputs` are in `dimg` order and must
/// match `desc.entries` one-to-one; `cicp` is the colour signalling
/// the fill colour is converted against (the overlay item's `colr`,
/// or the first input's). Returns the reconstructed canvas, its
/// layout, and `(output_width, output_height)`.
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
    let mut unpacked = Vec::with_capacity(inputs.len());
    for (i, inp) in inputs.iter().enumerate() {
        let p = unpack_planes(inp.frame, inp.format, inp.bit_depth, inp.width, inp.height)?;
        if i > 0 {
            let first = &unpacked[0];
            let (f, p): (&SamplePlanes, &SamplePlanes) = (first, &p);
            if (f.bit_depth, f.sx, f.sy, f.gray) != (p.bit_depth, p.sx, p.sy, p.gray) {
                return Err(Error::unsupported(format!(
                    "avif overlay: input {i} layout ({}-bit sx={} sy={} gray={}) differs from \
                     input 0 ({}-bit sx={} sy={} gray={}) — mixed layouts are not composited",
                    p.bit_depth, p.sx, p.sy, p.gray, f.bit_depth, f.sx, f.sy, f.gray
                )));
            }
        }
        unpacked.push(p);
    }
    let (bit_depth, sx, sy, gray) = {
        let f = &unpacked[0];
        (f.bit_depth, f.sx, f.sy, f.gray)
    };
    let max = (1u32 << bit_depth) - 1;

    // Visible rectangles (canvas space) per input; opaque ones drive
    // the "does the fill ever show" test.
    let mut visible: Vec<Option<(u32, u32, u32, u32)>> = Vec::with_capacity(inputs.len());
    let mut opaque_rects = Vec::new();
    for (inp, p) in inputs.iter().zip(&unpacked) {
        let left = inp.offset_x.max(0);
        let top = inp.offset_y.max(0);
        let right = (inp.offset_x + i64::from(p.width)).min(i64::from(cw));
        let bottom = (inp.offset_y + i64::from(p.height)).min(i64::from(ch));
        let rect = if right > left && bottom > top {
            Some((
                left as u32,
                top as u32,
                (right - left) as u32,
                (bottom - top) as u32,
            ))
        } else {
            None
        };
        if let (Some(r), None) = (rect, &p.alpha) {
            opaque_rects.push(r);
        }
        visible.push(rect);
    }
    let fill_alpha = u32::from(desc.canvas_fill_value[3]);
    let fill_shows = fill_alpha > 0 && !rects_cover_canvas(&opaque_rects, cw, ch);
    let fill = fill_samples(desc.canvas_fill_value, cicp, gray, bit_depth);
    if fill_shows && fill.is_none() {
        return Err(Error::unsupported(format!(
            "avif overlay: canvas_fill_value {:?} shows through but cannot be converted to \
             the inputs' coded colour space (matrix_coefficients={}, full_range={}, gray={}) — \
             only the H.273 identity matrix / neutral monochrome fills are converted",
            desc.canvas_fill_value, cicp.matrix_coefficients, cicp.full_range, gray
        )));
    }
    let (fy, fu, fv) = fill.unwrap_or((0, max as u16 / 2 + 1, max as u16 / 2 + 1));

    // Canvas: pre-multiplied colour + opacity, luma and chroma tracked
    // separately (chroma alpha is the block mean).
    let n = cw as usize * ch as usize;
    let mut prem_y = vec![u32::from(fy) * fill_alpha; n];
    let mut a_y = vec![fill_alpha as u16; n];
    let (ccw, cch) = chroma_dims(cw, ch, sx, sy);
    let cn = if gray { 0 } else { ccw as usize * cch as usize };
    let mut prem_u = vec![u32::from(fu) * fill_alpha; cn];
    let mut prem_v = vec![u32::from(fv) * fill_alpha; cn];
    let mut a_c = vec![fill_alpha as u16; cn];

    for ((inp, p), rect) in inputs.iter().zip(&unpacked).zip(&visible) {
        let Some((vx, vy, vw, vh)) = *rect else {
            continue;
        };
        let (ox, oy) = (inp.offset_x, inp.offset_y);
        let iw = p.width as usize;
        // Luma.
        for y in vy..vy + vh {
            let iy = (i64::from(y) - oy) as usize;
            let dst_row = y as usize * cw as usize;
            let src_row = iy * iw;
            for x in vx..vx + vw {
                let ix = (i64::from(x) - ox) as usize;
                let m = u32::from(p.y[src_row + ix]);
                let alpha = match &p.alpha {
                    Some(a) => alpha16(a[src_row + ix], bit_depth),
                    None => 65535,
                };
                over(
                    &mut prem_y[dst_row + x as usize],
                    m,
                    alpha,
                    inp.premultiplied,
                );
                over_alpha(&mut a_y[dst_row + x as usize], alpha);
            }
        }
        if gray {
            continue;
        }
        // Chroma: every canvas chroma sample whose luma block meets
        // the visible rect.
        let (icw, _) = p.chroma_dims();
        let bx = 1u32 << sx;
        let by = 1u32 << sy;
        let cx0 = vx >> sx;
        let cx1 = (vx + vw - 1) >> sx;
        let cy0 = vy >> sy;
        let cy1 = (vy + vh - 1) >> sy;
        for cy in cy0..=cy1 {
            for cx in cx0..=cx1 {
                // Luma block of this chroma sample ∩ visible rect.
                let lx0 = (cx * bx).max(vx);
                let lx1 = ((cx + 1) * bx).min(vx + vw);
                let ly0 = (cy * by).max(vy);
                let ly1 = ((cy + 1) * by).min(vy + vh);
                if lx1 <= lx0 || ly1 <= ly0 {
                    continue;
                }
                let alpha = match &p.alpha {
                    Some(a) => {
                        let mut sum = 0u64;
                        let mut count = 0u64;
                        for ly in ly0..ly1 {
                            let iy = (i64::from(ly) - oy) as usize;
                            for lx in lx0..lx1 {
                                let ix = (i64::from(lx) - ox) as usize;
                                sum += u64::from(alpha16(a[iy * iw + ix], bit_depth));
                                count += 1;
                            }
                        }
                        ((sum + count / 2) / count) as u32
                    }
                    None => 65535,
                };
                let ix = (i64::from(lx0) - ox) as usize >> sx;
                let iy = (i64::from(ly0) - oy) as usize >> sy;
                let src = iy * icw as usize + ix;
                let dst = cy as usize * ccw as usize + cx as usize;
                over(
                    &mut prem_u[dst],
                    u32::from(p.u[src]),
                    alpha,
                    inp.premultiplied,
                );
                over(
                    &mut prem_v[dst],
                    u32::from(p.v[src]),
                    alpha,
                    inp.premultiplied,
                );
                over_alpha(&mut a_c[dst], alpha);
            }
        }
    }

    let out_alpha = fill_alpha < 65535;
    let y = unpremultiply(&prem_y, &a_y, bit_depth, fy);
    let (u, v) = if gray {
        (Vec::new(), Vec::new())
    } else {
        (
            unpremultiply(&prem_u, &a_c, bit_depth, fu),
            unpremultiply(&prem_v, &a_c, bit_depth, fv),
        )
    };
    let alpha = out_alpha.then(|| {
        a_y.iter()
            .map(|&a| ((u32::from(a) * max + 32767) / 65535) as u16)
            .collect::<Vec<u16>>()
    });
    let planes = SamplePlanes {
        width: cw,
        height: ch,
        bit_depth,
        sx,
        sy,
        gray,
        y,
        u,
        v,
        alpha,
    };
    let (frame, format) = pack_planes(&planes)?;
    Ok((frame, format, cw, ch))
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
    fn unconvertible_visible_fill_is_unsupported() {
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
        let d = desc(4, 4, [1, 2, 3, 65535], &[(0, 0)]);
        let err = composite_overlay(&d, &[inp], &bt709_limited()).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
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

    #[test]
    fn rect_cover_sweep() {
        assert!(rects_cover_canvas(&[(0, 0, 4, 4)], 4, 4));
        assert!(rects_cover_canvas(&[(0, 0, 2, 4), (2, 0, 2, 4)], 4, 4));
        assert!(rects_cover_canvas(&[(0, 0, 4, 2), (0, 2, 4, 2)], 4, 4));
        assert!(!rects_cover_canvas(&[(0, 0, 2, 4), (3, 0, 1, 4)], 4, 4));
        assert!(!rects_cover_canvas(&[(0, 0, 4, 3)], 4, 4));
        assert!(!rects_cover_canvas(&[], 1, 1));
        // Overlapping, out-of-order rects.
        assert!(rects_cover_canvas(
            &[(1, 1, 3, 3), (0, 0, 2, 2), (0, 2, 2, 2), (2, 0, 2, 1)],
            4,
            4
        ));
    }

    /// 4:2:0 input at an odd offset: chroma is nearest-neighbour mapped
    /// and luma is exact.
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
        let o = unpack_planes(&out, ofmt, 8, 6, 6).unwrap();
        for y in 0..4usize {
            for x in 0..4usize {
                assert_eq!(o.y[(y + 1) * 6 + x + 1], p.y[y * 4 + x]);
            }
        }
        // Canvas chroma (1,1) covers luma (2..4, 2..4) → input luma
        // (1..3) → input chroma (0,0) for the first covered luma (1,1).
        assert_eq!(o.u[4], p.u[0]);
        assert_eq!(o.v[4], p.v[0]);
        // Canvas chroma (0,0) covers luma (0..2)² of which (1,1) is
        // covered → input luma (0,0) → chroma (0,0); opacity 1/4.
        assert_eq!(o.u[0], p.u[0]);
    }
}
