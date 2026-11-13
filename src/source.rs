//! Where an encode reads its samples from.
//!
//! The AV1 coder takes each coded item as planes of `u16` samples, so
//! a picture always has to be copied once — but only *once*, and for a
//! `grid` only one tile at a time. A [`TileSource`] hands out exactly
//! that: the coding planes of one rectangle of the picture, cut
//! straight from wherever the caller keeps its samples — 8-bit planes,
//! little-endian word planes, packed RGB(A) / gray + alpha rows with a
//! stride, or the `u16` planes of a [`StillImage`] — with no
//! whole-picture intermediate in between.
//!
//! [`PictureView`] is the borrowed planar source (any sample storage
//! per plane, see [`PlaneRef`]); [`LossyRgb`] converts packed RGB rows
//! to Y′CbCr rectangle by rectangle.

use crate::api::{ColorInfo, StillChroma};
use crate::error::{AvifError as Error, Result};
use crate::still::{StillImage, pad_plane};

/// How the samples of one plane are stored.
#[derive(Clone, Copy, Debug)]
enum Store<'a> {
    /// One `u16` per sample; the stride counts `u16`s.
    Words(&'a [u16]),
    /// One byte per sample; the stride counts bytes.
    Bytes(&'a [u8]),
    /// Two little-endian bytes per sample; the stride counts bytes.
    WordsLe(&'a [u8]),
}

/// One plane of `w × h` samples inside a caller's buffer: row `r`
/// starts `r * stride` elements in, and sample `x` of a row is the
/// `(x * step + offset)`-th sample of it (`step = 1`, `offset = 0` for
/// a planar buffer; a channel of a packed pixel otherwise).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlaneRef<'a> {
    store: Store<'a>,
    stride: usize,
    step: usize,
    offset: usize,
    w: usize,
    h: usize,
}

impl<'a> PlaneRef<'a> {
    fn checked(
        store: Store<'a>,
        stride: usize,
        step: usize,
        offset: usize,
        w: usize,
        h: usize,
        what: &str,
    ) -> Result<Self> {
        let (len, unit) = match store {
            Store::Words(d) => (d.len(), 1),
            Store::Bytes(d) => (d.len(), 1),
            Store::WordsLe(d) => (d.len(), 2),
        };
        if w == 0 || h == 0 || step == 0 || offset >= step {
            return Err(Error::invalid(format!(
                "avif encode: {what} plane has no samples ({w}x{h})"
            )));
        }
        // Elements one row spans, and the whole plane with its last
        // row unpadded.
        let row = ((w - 1) * step + offset + 1) * unit;
        let need = (h - 1)
            .checked_mul(stride)
            .and_then(|n| n.checked_add(row))
            .ok_or_else(|| Error::invalid("avif encode: plane geometry overflows"))?;
        if stride < row || len < need {
            return Err(Error::invalid(format!(
                "avif encode: {what} plane too short ({len} elements, stride {stride}, \
                 need {h} rows of {row})"
            )));
        }
        Ok(Self {
            store,
            stride,
            step,
            offset,
            w,
            h,
        })
    }

    /// A tightly packed `u16` plane.
    pub(crate) fn words(data: &'a [u16], w: usize, h: usize, what: &str) -> Result<Self> {
        if data.len() != w * h {
            return Err(Error::invalid(format!(
                "avif encode: {what} plane length {} != {w}x{h}",
                data.len()
            )));
        }
        Self::checked(Store::Words(data), w, 1, 0, w, h, what)
    }

    /// A plane of bytes: channel `offset` of `step`-byte pixels, rows
    /// `stride` bytes apart.
    pub(crate) fn bytes(
        data: &'a [u8],
        stride: usize,
        step: usize,
        offset: usize,
        w: usize,
        h: usize,
        what: &str,
    ) -> Result<Self> {
        Self::checked(Store::Bytes(data), stride, step, offset, w, h, what)
    }

    /// A plane of little-endian 2-byte samples: sample `offset` of
    /// `step`-sample pixels, rows `stride` bytes apart.
    pub(crate) fn words_le(
        data: &'a [u8],
        stride: usize,
        step: usize,
        offset: usize,
        w: usize,
        h: usize,
        what: &str,
    ) -> Result<Self> {
        Self::checked(Store::WordsLe(data), stride, step, offset, w, h, what)
    }

    /// The `tw × th` rectangle at `(x0, y0)` as tightly packed `u16`
    /// samples, the plane's last column / row repeated past its
    /// extents. Every sample must fit `depth` bits.
    pub(crate) fn rect(
        &self,
        x0: usize,
        y0: usize,
        tw: usize,
        th: usize,
        depth: u8,
    ) -> Result<Vec<u16>> {
        if x0 >= self.w || y0 >= self.h {
            return Err(Error::invalid(
                "avif encode: tile origin outside the picture",
            ));
        }
        let copy = (self.w - x0).min(tw);
        let mut out: Vec<u16> = Vec::with_capacity(tw * th);
        for row in 0..th {
            if y0 + row >= self.h {
                // Below the picture: the row above, again.
                let prev = (row - 1) * tw;
                out.extend_from_within(prev..prev + tw);
                continue;
            }
            let base = (y0 + row) * self.stride;
            let first = x0 * self.step + self.offset;
            match self.store {
                Store::Words(d) if self.step == 1 => {
                    out.extend_from_slice(&d[base + first..base + first + copy]);
                }
                Store::Words(d) => out.extend(
                    d[base + first..]
                        .iter()
                        .step_by(self.step)
                        .take(copy)
                        .copied(),
                ),
                Store::Bytes(d) if self.step == 1 => out.extend(
                    d[base + first..base + first + copy]
                        .iter()
                        .map(|&b| u16::from(b)),
                ),
                Store::Bytes(d) => out.extend(
                    d[base + first..]
                        .iter()
                        .step_by(self.step)
                        .take(copy)
                        .map(|&b| u16::from(b)),
                ),
                Store::WordsLe(d) => out.extend(
                    d[base + first * 2..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .step_by(self.step)
                        .take(copy)
                        .map(|c| u16::from_le_bytes(*c)),
                ),
            }
            if copy < tw {
                let edge = out[out.len() - 1];
                out.extend(std::iter::repeat_n(edge, tw - copy));
            }
        }
        let wide = !matches!(self.store, Store::Bytes(_)) || depth < 8;
        if wide {
            let limit = 1u32 << depth;
            if out.iter().any(|&s| u32::from(s) >= limit) {
                return Err(Error::invalid(format!(
                    "avif still: sample exceeds bit_depth {depth} range"
                )));
            }
        }
        Ok(out)
    }
}

/// The coding planes of rectangles of one picture, cut on demand.
///
/// `x0` / `y0` are even on a sub-sampled axis (the grid's tile size is
/// a multiple of 8, a whole picture starts at 0); a rectangle that
/// runs past the picture repeats the picture's last column / row —
/// in each plane, after conversion.
pub(crate) trait TileSource: Sync {
    /// Picture width in luma samples.
    fn width(&self) -> u32;
    /// Picture height in luma samples.
    fn height(&self) -> u32;
    /// Sample depth: 8, 10 or 12.
    fn bit_depth(&self) -> u8;
    /// The layout the picture is coded in.
    fn chroma(&self) -> StillChroma;
    /// The picture carries an alpha plane.
    fn has_alpha(&self) -> bool;
    /// Y, Cb, Cr of the `tw × th` rectangle at `(x0, y0)` (the chroma
    /// planes at the rounded-up sub-sampled size, empty for a
    /// monochrome picture).
    fn colour(&self, x0: usize, y0: usize, tw: usize, th: usize) -> Result<[Vec<u16>; 3]>;
    /// The alpha plane of the same rectangle, when there is one.
    fn alpha(&self, x0: usize, y0: usize, tw: usize, th: usize) -> Result<Option<Vec<u16>>>;
}

/// The rounded-up size of a sub-sampled extent.
fn sub(extent: usize, shift: u32) -> usize {
    (extent + shift as usize) >> shift
}

/// A picture whose coding planes already exist in the caller's
/// buffers, in any sample storage.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PictureView<'a> {
    width: u32,
    height: u32,
    bit_depth: u8,
    chroma: StillChroma,
    y: PlaneRef<'a>,
    cb_cr: Option<(PlaneRef<'a>, PlaneRef<'a>)>,
    alpha: Option<PlaneRef<'a>>,
}

impl<'a> PictureView<'a> {
    /// Assemble a view; the planes must have the layout's geometry
    /// (chroma at the rounded-up sub-sampled size, none for a
    /// monochrome picture, alpha at the luma size).
    pub(crate) fn new(
        width: u32,
        height: u32,
        bit_depth: u8,
        chroma: StillChroma,
        y: PlaneRef<'a>,
        cb_cr: Option<(PlaneRef<'a>, PlaneRef<'a>)>,
        alpha: Option<PlaneRef<'a>>,
    ) -> Result<Self> {
        if !matches!(bit_depth, 8 | 10 | 12) {
            return Err(Error::invalid(format!(
                "avif still: bit_depth must be 8, 10 or 12, got {bit_depth}"
            )));
        }
        let (w, h) = (width as usize, height as usize);
        let (sx, sy) = chroma.subsampling();
        let (cw, ch) = (sub(w, sx), sub(h, sy));
        let fits = |p: &PlaneRef<'_>, pw: usize, ph: usize| (p.w, p.h) == (pw, ph);
        let chroma_ok = match (&cb_cr, chroma.has_chroma()) {
            (Some((u, v)), true) => fits(u, cw, ch) && fits(v, cw, ch),
            (None, false) => true,
            _ => false,
        };
        if !fits(&y, w, h) || !chroma_ok || alpha.as_ref().is_some_and(|a| !fits(a, w, h)) {
            return Err(Error::invalid(format!(
                "avif encode: plane geometry does not match {width}x{height} {chroma:?}"
            )));
        }
        Ok(Self {
            width,
            height,
            bit_depth,
            chroma,
            y,
            cb_cr,
            alpha,
        })
    }

    /// The planes of a [`StillImage`] (its depth map is not part of
    /// the view).
    pub(crate) fn of_still(img: &'a StillImage) -> Result<Self> {
        let (w, h) = (img.width as usize, img.height as usize);
        let (sx, sy) = img.chroma.subsampling();
        let (cw, ch) = (sub(w, sx), sub(h, sy));
        let cb_cr = if img.chroma.has_chroma() {
            Some((
                PlaneRef::words(&img.u, cw, ch, "Cb")?,
                PlaneRef::words(&img.v, cw, ch, "Cr")?,
            ))
        } else {
            None
        };
        let alpha = match &img.alpha {
            Some(a) => Some(PlaneRef::words(a, w, h, "alpha")?),
            None => None,
        };
        Self::new(
            img.width,
            img.height,
            img.bit_depth,
            img.chroma,
            PlaneRef::words(&img.y, w, h, "Y")?,
            cb_cr,
            alpha,
        )
    }
}

impl TileSource for PictureView<'_> {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn bit_depth(&self) -> u8 {
        self.bit_depth
    }
    fn chroma(&self) -> StillChroma {
        self.chroma
    }
    fn has_alpha(&self) -> bool {
        self.alpha.is_some()
    }
    fn colour(&self, x0: usize, y0: usize, tw: usize, th: usize) -> Result<[Vec<u16>; 3]> {
        let y = self.y.rect(x0, y0, tw, th, self.bit_depth)?;
        let Some((u, v)) = &self.cb_cr else {
            return Ok([y, Vec::new(), Vec::new()]);
        };
        let (sx, sy) = self.chroma.subsampling();
        let (cx, cy, cw, ch) = (x0 >> sx, y0 >> sy, sub(tw, sx), sub(th, sy));
        Ok([
            y,
            u.rect(cx, cy, cw, ch, self.bit_depth)?,
            v.rect(cx, cy, cw, ch, self.bit_depth)?,
        ])
    }
    fn alpha(&self, x0: usize, y0: usize, tw: usize, th: usize) -> Result<Option<Vec<u16>>> {
        match &self.alpha {
            Some(a) => Ok(Some(a.rect(x0, y0, tw, th, self.bit_depth)?)),
            None => Ok(None),
        }
    }
}

/// Packed 8-bit RGB(A) rows converted to Y′CbCr as they are cut: each
/// rectangle goes through the one-pass converter
/// ([`crate::encoder::packed_rgb_to_ycbcr`]) on its own, which is the
/// whole-picture conversion restricted to it — a pixel's Y′CbCr
/// depends on that pixel alone, and a rectangle starting on an even
/// column / row holds whole chroma cells.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LossyRgb<'a> {
    data: &'a [u8],
    stride: usize,
    width: u32,
    height: u32,
    bpp: usize,
    rgb_at: [usize; 3],
    color: ColorInfo,
    chroma: StillChroma,
    alpha: Option<PlaneRef<'a>>,
}

impl<'a> LossyRgb<'a> {
    /// `data` holds `height` rows of `width` `bpp`-byte pixels,
    /// `stride` bytes apart; `rgb_at` are the byte offsets of R, G, B
    /// in a pixel and a fourth byte (`bpp == 4`) is alpha. The picture
    /// is converted with `color` into `chroma` (a monochrome request
    /// codes 4:4:4).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        data: &'a [u8],
        stride: usize,
        width: u32,
        height: u32,
        bpp: usize,
        rgb_at: [usize; 3],
        color: ColorInfo,
        chroma: StillChroma,
    ) -> Result<Self> {
        let (w, h) = (width as usize, height as usize);
        // The colour channels span whole pixels: check with the last
        // byte of a pixel.
        PlaneRef::bytes(data, stride, bpp, bpp - 1, w, h, "packed RGB")?;
        let alpha = if bpp == 4 {
            Some(PlaneRef::bytes(data, stride, 4, 3, w, h, "alpha")?)
        } else {
            None
        };
        Ok(Self {
            data,
            stride,
            width,
            height,
            bpp,
            rgb_at,
            color,
            chroma: match chroma {
                StillChroma::Monochrome => StillChroma::Yuv444,
                other => other,
            },
            alpha,
        })
    }

    /// The colour description the conversion produces samples in.
    pub(crate) fn color(&self) -> ColorInfo {
        self.color
    }
}

impl TileSource for LossyRgb<'_> {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn bit_depth(&self) -> u8 {
        8
    }
    fn chroma(&self) -> StillChroma {
        self.chroma
    }
    fn has_alpha(&self) -> bool {
        self.alpha.is_some()
    }
    fn colour(&self, x0: usize, y0: usize, tw: usize, th: usize) -> Result<[Vec<u16>; 3]> {
        let (w, h) = (self.width as usize, self.height as usize);
        if x0 >= w || y0 >= h {
            return Err(Error::invalid(
                "avif encode: tile origin outside the picture",
            ));
        }
        // The part of the rectangle inside the picture, converted;
        // then each plane replicated out to the rectangle.
        let (iw, ih) = ((w - x0).min(tw), (h - y0).min(th));
        let inside = crate::encoder::packed_rgb_to_ycbcr(
            &self.data[y0 * self.stride + x0 * self.bpp..],
            self.stride,
            iw,
            ih,
            self.bpp,
            self.rgb_at,
            self.color,
            self.chroma,
        );
        if (iw, ih) == (tw, th) {
            return Ok([inside.y, inside.u, inside.v]);
        }
        let (sx, sy) = self.chroma.subsampling();
        let (icw, ich) = (sub(iw, sx), sub(ih, sy));
        let (cw, ch) = (sub(tw, sx), sub(th, sy));
        Ok([
            pad_plane(&inside.y, iw, ih, tw, th),
            pad_plane(&inside.u, icw, ich, cw, ch),
            pad_plane(&inside.v, icw, ich, cw, ch),
        ])
    }
    fn alpha(&self, x0: usize, y0: usize, tw: usize, th: usize) -> Result<Option<Vec<u16>>> {
        match &self.alpha {
            Some(a) => Ok(Some(a.rect(x0, y0, tw, th, 8)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(n: usize, k: usize) -> Vec<u16> {
        (0..n).map(|i| ((i * k + 3) % 251) as u16).collect()
    }

    /// Every storage yields the same rectangle as the plain `u16`
    /// plane, replication past the right / bottom edge included.
    #[test]
    fn every_storage_cuts_the_same_rectangles() {
        let (w, h) = (13usize, 7usize);
        let words = pattern(w * h, 7);
        let reference = PlaneRef::words(&words, w, h, "ref").unwrap();
        // Bytes with a padded stride.
        let stride = w + 5;
        let mut bytes = vec![0xEEu8; stride * h];
        for r in 0..h {
            for x in 0..w {
                bytes[r * stride + x] = words[r * w + x] as u8;
            }
        }
        // Channel 2 of 4-byte pixels.
        let pstride = w * 4 + 3;
        let mut packed = vec![0xEEu8; pstride * h];
        for r in 0..h {
            for x in 0..w {
                packed[r * pstride + x * 4 + 2] = words[r * w + x] as u8;
            }
        }
        // Little-endian words, and sample 1 of 2-sample pixels.
        let lstride = w * 2 + 4;
        let mut le = vec![0xEEu8; lstride * h];
        let ystride = w * 4 + 2;
        let mut ya = vec![0x01u8; ystride * h];
        for r in 0..h {
            for x in 0..w {
                let b = words[r * w + x].to_le_bytes();
                le[r * lstride + x * 2..][..2].copy_from_slice(&b);
                ya[r * ystride + x * 4 + 2..][..2].copy_from_slice(&b);
            }
        }
        let others = [
            PlaneRef::bytes(&bytes, stride, 1, 0, w, h, "bytes").unwrap(),
            PlaneRef::bytes(&packed, pstride, 4, 2, w, h, "packed").unwrap(),
            PlaneRef::words_le(&le, lstride, 1, 0, w, h, "le").unwrap(),
            PlaneRef::words_le(&ya, ystride, 2, 1, w, h, "ya").unwrap(),
        ];
        for (x0, y0, tw, th) in [
            (0, 0, w, h),
            (0, 0, 16, 8),
            (8, 0, 8, 8),
            (4, 4, 3, 2),
            (12, 6, 8, 8),
            (0, 6, 13, 5),
        ] {
            let want = reference.rect(x0, y0, tw, th, 8).unwrap();
            assert_eq!(want.len(), tw * th);
            // Replication: the last column / row repeated.
            for row in 0..th {
                for col in 0..tw {
                    let (sx, sy) = ((x0 + col).min(w - 1), (y0 + row).min(h - 1));
                    assert_eq!(want[row * tw + col], words[sy * w + sx]);
                }
            }
            for (i, other) in others.iter().enumerate() {
                assert_eq!(
                    other.rect(x0, y0, tw, th, 8).unwrap(),
                    want,
                    "storage {i} at ({x0}, {y0}) {tw}x{th}"
                );
            }
        }
    }

    #[test]
    fn short_planes_and_out_of_range_samples_are_refused() {
        assert!(PlaneRef::bytes(&[0; 10], 4, 1, 0, 4, 3, "p").is_err());
        assert!(PlaneRef::bytes(&[0; 11], 4, 1, 0, 3, 3, "p").is_ok());
        assert!(PlaneRef::bytes(&[0; 12], 2, 1, 0, 3, 3, "p").is_err());
        assert!(PlaneRef::words_le(&[0; 7], 4, 1, 0, 2, 2, "p").is_err());
        assert!(PlaneRef::words(&[0; 5], 2, 3, "p").is_err());
        assert!(PlaneRef::bytes(&[0; 4], 2, 1, 0, 0, 2, "p").is_err());
        let hot = [0u16, 1023, 1024, 5];
        let plane = PlaneRef::words(&hot, 2, 2, "p").unwrap();
        assert!(plane.rect(0, 0, 2, 2, 10).is_err());
        assert!(plane.rect(0, 0, 2, 1, 10).is_ok());
        assert!(plane.rect(0, 0, 2, 2, 12).is_ok());
        assert!(plane.rect(2, 0, 1, 1, 12).is_err());
    }

    /// Converting packed RGB rectangle by rectangle equals cutting the
    /// rectangles out of the whole-picture conversion — at odd picture
    /// extents and past the right / bottom edge too.
    #[test]
    fn lossy_rgb_rectangles_equal_the_whole_picture_conversion() {
        for (w, h) in [(40usize, 24usize), (37, 21), (9, 30)] {
            for bpp in [3usize, 4] {
                let stride = w * bpp + 6;
                let data: Vec<u8> = (0..stride * h)
                    .map(|i| ((i * 31 + i / 7) % 256) as u8)
                    .collect();
                for chroma in [
                    StillChroma::Yuv420,
                    StillChroma::Yuv422,
                    StillChroma::Yuv444,
                ] {
                    let color = ColorInfo::miaf_default();
                    let whole = crate::encoder::packed_rgb_to_ycbcr(
                        &data,
                        stride,
                        w,
                        h,
                        bpp,
                        [2, 1, 0],
                        color,
                        chroma,
                    );
                    let view = PictureView::of_still(&whole).unwrap();
                    let src = LossyRgb::new(
                        &data,
                        stride,
                        w as u32,
                        h as u32,
                        bpp,
                        [2, 1, 0],
                        color,
                        chroma,
                    )
                    .unwrap();
                    assert_eq!(src.has_alpha(), bpp == 4);
                    for (x0, y0, tw, th) in [
                        (0, 0, w, h),
                        (0, 0, 16, 16),
                        (16, 16, 16, 16),
                        (32, 16, 16, 16),
                        (8, 24, 8, 8),
                    ] {
                        if x0 >= w || y0 >= h {
                            continue;
                        }
                        assert_eq!(
                            src.colour(x0, y0, tw, th).unwrap(),
                            view.colour(x0, y0, tw, th).unwrap(),
                            "{w}x{h} bpp {bpp} {chroma:?} at ({x0}, {y0}) {tw}x{th}"
                        );
                    }
                }
            }
        }
    }
}
