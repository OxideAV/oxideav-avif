//! Pixel → AVIF **still-image encoder** — registry-gated.
//!
//! This module closes the gap the container muxer ([`crate::mux`])
//! deliberately left open: it turns raw pixels into the coded AV1
//! Image Item Data by driving `oxideav_av1`'s conformance-grade
//! KEY-frame encoder, then wraps the result through [`crate::AvifMuxer`]
//! / [`crate::AvifGridMuxer`] into a conformant AVIF file.
//!
//! # Coverage
//!
//! * Every conformant (bit depth, chroma format) pairing the AV1
//!   spec admits: 8 / 10 / 12-bit samples in 4:2:0, 4:2:2, 4:4:4 or
//!   monochrome layout (§6.4.1 `seq_profile` is elected per pairing by
//!   the AV1 encoder; the emitted `av1C` record mirrors the sequence
//!   header per av1-avif §2.2.1 "The values of the fields in the
//!   AV1ItemConfigurationProperty shall match those of the Sequence
//!   Header OBU in the AV1 Image Item Data").
//! * **RGB(A)** input via the H.273 identity matrix
//!   (`matrix_coefficients = 0`) in 4:4:4 — the mapping is exact
//!   (Y = G, Cb = B, Cr = R; see [`crate::cicp::CicpTriple::is_identity_matrix`]),
//!   so a lossless encode round-trips RGB byte-for-byte. The `colr`
//!   `nclx` property signals the identity triple with full range.
//! * **Alpha** as a separate AV1-coded monochrome auxiliary item
//!   (av1-avif §4.1): same bit depth as the master (the §4.1 `shall`),
//!   no `colr` (the §4.1 `should`), hidden item + `auxC` alpha URN +
//!   `auxl` iref, `prem` iref when premultiplied.
//! * **Arbitrary dimensions.** The AV1 encoder codes multiples of 8
//!   per axis (min 8); other extents are edge-replicated up to the
//!   next multiple and cropped back with an essential `clap` property
//!   anchored top-left (av1-avif §2.2.3). `ispe` always documents the
//!   coded frame extents per the §2.2.2 `shall` (`image_width` /
//!   `image_height` equal `UpscaledWidth` / `FrameHeight`).
//! * **Grid encode** ([`encode_still_grid`]) for canvases beyond the
//!   single-item limit: the canvas splits into equal coded tiles
//!   (HEIF §6.6.2 `grid` derived item, `dimg` irefs), the declared
//!   output extents trim the right/bottom overflow, and out-of-canvas
//!   tile content is edge-replicated.
//! * The `ftyp` profile brand follows the coded `seq_profile`
//!   (av1-avif §8): Main → `MA1B`, High → `MA1A`, Professional
//!   (4:2:2 / 12-bit) → general brands only (§8.1).
//!
//! # Exactness
//!
//! At `base_q_idx = 0` the AV1 encode is lossless — the AVIF file
//! decodes back (through this crate's own [`crate::AvifDecoder`] or
//! any conformant reader) to the input samples exactly. Lossy encodes
//! are validated PSNR-gated in the test suite.

use crate::error::{AvifError as Error, Result};
use crate::meta::{Clap, Colr};
use crate::mux::{
    AvifGridMuxer, AvifMuxer, AvifOverlayMuxer, GridTile, IdentityDerivation, OverlayLayer,
};

use oxideav_av1::encoder::key_frame::encode_key_frame_yuv_with_q;
use oxideav_av1::encoder::scalable_gop::encode_spatial_layered_gop_yuv_with_q;
use oxideav_av1::encoder::temporal_unit::encode_sequence_header_obu;
use oxideav_av1::encoder::yuv_frame::{ChromaFormat, YuvFrame};
use oxideav_av1::{parse_obu, ObuType, SequenceHeader};

/// Chroma layout of a [`StillImage`]. Mirrors the AV1 §6.4.2
/// subsampling pairings; conversion to the AV1 encoder's own layout
/// enum is internal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StillChroma {
    /// Chroma at half extent on both axes (`subsampling_x =
    /// subsampling_y = 1`). Requires even luma extents.
    Yuv420,
    /// Chroma at half horizontal extent (`subsampling_x = 1`,
    /// `subsampling_y = 0`). Requires an even luma width. Codes as AV1
    /// Professional profile.
    Yuv422,
    /// Chroma at full extent.
    Yuv444,
    /// Luma only (4:0:0).
    Monochrome,
}

impl StillChroma {
    pub(crate) fn to_av1(self) -> ChromaFormat {
        match self {
            StillChroma::Yuv420 => ChromaFormat::Yuv420,
            StillChroma::Yuv422 => ChromaFormat::Yuv422,
            StillChroma::Yuv444 => ChromaFormat::Yuv444,
            StillChroma::Monochrome => ChromaFormat::Monochrome,
        }
    }

    /// `(subsampling_x, subsampling_y)` as shift amounts.
    pub(crate) fn subsampling(self) -> (u32, u32) {
        match self {
            StillChroma::Yuv420 => (1, 1),
            StillChroma::Yuv422 => (1, 0),
            StillChroma::Yuv444 => (0, 0),
            StillChroma::Monochrome => (0, 0), // no chroma planes
        }
    }

    pub(crate) fn has_chroma(self) -> bool {
        self != StillChroma::Monochrome
    }
}

/// One still image handed to [`encode_still`] / [`encode_still_grid`].
///
/// Samples are carried as `u16` regardless of bit depth (values must
/// fit `bit_depth` bits); the 8-bit constructors widen for you. Planes
/// are tightly packed row-major at their natural (subsampled) extents.
#[derive(Clone, Debug)]
pub struct StillImage {
    /// Luma width in pixels (≥ 1).
    pub width: u32,
    /// Luma height in pixels (≥ 1).
    pub height: u32,
    /// 8, 10 or 12.
    pub bit_depth: u8,
    /// Chroma layout.
    pub chroma: StillChroma,
    /// Luma plane, `width × height`.
    pub y: Vec<u16>,
    /// Cb plane at the subsampled extent (empty for monochrome).
    pub u: Vec<u16>,
    /// Cr plane at the subsampled extent (empty for monochrome).
    pub v: Vec<u16>,
    /// Optional full-resolution alpha plane (`width × height`, same
    /// bit depth as the colour planes — the av1-avif §4.1 `shall`).
    pub alpha: Option<Vec<u16>>,
    /// Optional full-resolution depth map (`width × height`, coded as a
    /// monochrome auxiliary at the master's bit depth — av1-avif §4.1 /
    /// HEIF §6.9.2 `urn:mpeg:mpegB:cicp:systems:auxiliary:depth`).
    pub depth_map: Option<Vec<u16>>,
    /// Optional `colr` colour-information property for the primary
    /// item. The RGB(A) constructors pre-fill the identity-matrix
    /// `nclx` triple; YUV constructors leave it `None`.
    pub colr: Option<Colr>,
    /// Optional pass-through container properties (Exif / XMP /
    /// HDR metadata / orientation), muxed alongside the coded item.
    pub props: StillProperties,
}

/// Pass-through container properties for [`encode_still`] — carried
/// straight to the muxer, no pixel-path involvement.
///
/// * `exif` — an `ExifDataBlock` (4-byte `exif_tiff_header_offset` +
///   TIFF-structured bytes), linked via a `cdsc` iref (av1-avif §5.2).
/// * `xmp` — an XMP packet as a `mime` item (`application/rdf+xml`),
///   linked via `cdsc` (av1-avif §5.3).
/// * `mdcv` / `clli` / `amve` — HDR descriptive properties.
/// * `irot` / `imir` — essential transformative orientation
///   properties (HEIF §6.5.10 application order `clap` → `irot` →
///   `imir`; the encoder's own padding `clap` composes with them).
/// * `pasp` — pixel aspect ratio.
#[derive(Clone, Debug, Default)]
pub struct StillProperties {
    /// Exif payload (av1-avif §5.2).
    pub exif: Option<Vec<u8>>,
    /// XMP payload (av1-avif §5.3).
    pub xmp: Option<Vec<u8>>,
    /// Mastering display colour volume (ISO/IEC 14496-12 §12.1.5.3).
    pub mdcv: Option<crate::meta::Mdcv>,
    /// Content light level (ISO/IEC 14496-12 §12.1.5.4).
    pub clli: Option<crate::meta::Clli>,
    /// Ambient viewing environment (AVIF §6.5.36).
    pub amve: Option<crate::meta::Amve>,
    /// Rotation, anti-clockwise quarter turns 0..=3.
    pub irot: Option<u8>,
    /// Mirror axis (0 = vertical flip, 1 = horizontal flip).
    pub imir: Option<u8>,
    /// Pixel aspect ratio.
    pub pasp: Option<crate::meta::Pasp>,
    /// Make the primary an `iden` identity derivation of the coded
    /// image carrying these transformative properties (HEIF
    /// §6.6.2.1) — the coded item stays exposed untransformed
    /// alongside. See [`AvifMuxer::with_identity_derivation`].
    pub identity_derivation: Option<IdentityDerivation>,
}

impl StillImage {
    /// General constructor: any (bit depth, chroma) pairing with
    /// `u16`-carried samples.
    pub fn yuv(
        width: u32,
        height: u32,
        bit_depth: u8,
        chroma: StillChroma,
        y: Vec<u16>,
        u: Vec<u16>,
        v: Vec<u16>,
    ) -> Result<Self> {
        let img = Self {
            width,
            height,
            bit_depth,
            chroma,
            y,
            u,
            v,
            alpha: None,
            depth_map: None,
            colr: None,
            props: StillProperties::default(),
        };
        img.validate()?;
        Ok(img)
    }

    /// 8-bit 4:2:0 constructor (even `width` / `height` required).
    pub fn yuv420_8(width: u32, height: u32, y: &[u8], u: &[u8], v: &[u8]) -> Result<Self> {
        Self::yuv(
            width,
            height,
            8,
            StillChroma::Yuv420,
            widen(y),
            widen(u),
            widen(v),
        )
    }

    /// 8-bit 4:2:2 constructor (even `width` required).
    pub fn yuv422_8(width: u32, height: u32, y: &[u8], u: &[u8], v: &[u8]) -> Result<Self> {
        Self::yuv(
            width,
            height,
            8,
            StillChroma::Yuv422,
            widen(y),
            widen(u),
            widen(v),
        )
    }

    /// 8-bit 4:4:4 constructor.
    pub fn yuv444_8(width: u32, height: u32, y: &[u8], u: &[u8], v: &[u8]) -> Result<Self> {
        Self::yuv(
            width,
            height,
            8,
            StillChroma::Yuv444,
            widen(y),
            widen(u),
            widen(v),
        )
    }

    /// 8-bit monochrome (4:0:0) constructor.
    pub fn gray_8(width: u32, height: u32, y: &[u8]) -> Result<Self> {
        Self::yuv(
            width,
            height,
            8,
            StillChroma::Monochrome,
            widen(y),
            Vec::new(),
            Vec::new(),
        )
    }

    /// Interleaved 8-bit RGB (3 bytes per pixel) via the H.273
    /// **identity matrix** in 4:4:4: `Y = G`, `Cb = B`, `Cr = R` —
    /// mathematically exact, so a lossless encode round-trips the RGB
    /// bytes exactly. Pre-fills the `colr` `nclx` identity triple
    /// (BT.709 primaries, sRGB transfer, `matrix_coefficients = 0`,
    /// full range).
    pub fn rgb8(width: u32, height: u32, rgb: &[u8]) -> Result<Self> {
        let n = (width as usize) * (height as usize);
        if rgb.len() != n * 3 {
            return Err(Error::invalid(format!(
                "avif still: rgb8 expects {} bytes for {width}x{height}, got {}",
                n * 3,
                rgb.len()
            )));
        }
        let mut y = Vec::with_capacity(n);
        let mut u = Vec::with_capacity(n);
        let mut v = Vec::with_capacity(n);
        for px in rgb.chunks_exact(3) {
            v.push(px[0] as u16); // R → Cr
            y.push(px[1] as u16); // G → Y
            u.push(px[2] as u16); // B → Cb
        }
        let mut img = Self::yuv(width, height, 8, StillChroma::Yuv444, y, u, v)?;
        img.colr = Some(identity_full_range_colr());
        Ok(img)
    }

    /// Interleaved 8-bit RGBA (4 bytes per pixel) — the RGB channels
    /// as [`Self::rgb8`] plus the A channel as an alpha auxiliary.
    pub fn rgba8(width: u32, height: u32, rgba: &[u8]) -> Result<Self> {
        let n = (width as usize) * (height as usize);
        if rgba.len() != n * 4 {
            return Err(Error::invalid(format!(
                "avif still: rgba8 expects {} bytes for {width}x{height}, got {}",
                n * 4,
                rgba.len()
            )));
        }
        let mut y = Vec::with_capacity(n);
        let mut u = Vec::with_capacity(n);
        let mut v = Vec::with_capacity(n);
        let mut a = Vec::with_capacity(n);
        for px in rgba.chunks_exact(4) {
            v.push(px[0] as u16);
            y.push(px[1] as u16);
            u.push(px[2] as u16);
            a.push(px[3] as u16);
        }
        let mut img = Self::yuv(width, height, 8, StillChroma::Yuv444, y, u, v)?;
        img.colr = Some(identity_full_range_colr());
        img.alpha = Some(a);
        img.validate()?;
        Ok(img)
    }

    /// Attach a full-resolution alpha plane (`width × height` samples
    /// at the image's bit depth).
    /// Attach a depth map (`width × height` samples at the image's
    /// bit depth), encoded as a hidden monochrome auxiliary item.
    pub fn with_depth_map(mut self, depth_map: Vec<u16>) -> Result<Self> {
        if depth_map.len() != self.width as usize * self.height as usize {
            return Err(Error::invalid(format!(
                "avif still: depth map has {} samples, expected {}x{}",
                depth_map.len(),
                self.width,
                self.height
            )));
        }
        self.depth_map = Some(depth_map);
        Ok(self)
    }

    pub fn with_alpha(mut self, alpha: Vec<u16>) -> Result<Self> {
        self.alpha = Some(alpha);
        self.validate()?;
        Ok(self)
    }

    /// Attach a `colr` property for the primary item (overrides any
    /// constructor-provided value).
    pub fn with_colr(mut self, colr: Colr) -> Self {
        self.colr = Some(colr);
        self
    }

    /// Attach pass-through container properties (Exif / XMP / HDR /
    /// orientation / aspect); see [`StillProperties`].
    pub fn with_props(mut self, props: StillProperties) -> Self {
        self.props = props;
        self
    }

    /// Chroma plane extents implied by `(width, height, chroma)`.
    pub(crate) fn chroma_dims(&self) -> (u32, u32) {
        if !self.chroma.has_chroma() {
            return (0, 0);
        }
        let (sx, sy) = self.chroma.subsampling();
        (self.width >> sx, self.height >> sy)
    }

    /// Shape / depth / range validation.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::invalid(
                "avif still: dimensions must be at least 1x1",
            ));
        }
        if !matches!(self.bit_depth, 8 | 10 | 12) {
            return Err(Error::invalid(format!(
                "avif still: bit_depth must be 8, 10 or 12, got {}",
                self.bit_depth
            )));
        }
        let (sx, sy) = self.chroma.subsampling();
        if self.chroma.has_chroma()
            && ((sx == 1 && self.width % 2 != 0) || (sy == 1 && self.height % 2 != 0))
        {
            return Err(Error::invalid(format!(
                "avif still: {:?} requires even subsampled extents, got {}x{}",
                self.chroma, self.width, self.height
            )));
        }
        let luma_len = (self.width as usize) * (self.height as usize);
        if self.y.len() != luma_len {
            return Err(Error::invalid(format!(
                "avif still: Y plane length {} != {}x{}",
                self.y.len(),
                self.width,
                self.height
            )));
        }
        let (cw, ch) = self.chroma_dims();
        let chroma_len = (cw as usize) * (ch as usize);
        if self.u.len() != chroma_len || self.v.len() != chroma_len {
            return Err(Error::invalid(format!(
                "avif still: chroma plane lengths ({}, {}) != expected {chroma_len}",
                self.u.len(),
                self.v.len()
            )));
        }
        if let Some(a) = &self.alpha {
            if a.len() != luma_len {
                return Err(Error::invalid(format!(
                    "avif still: alpha plane length {} != {}x{} (alpha is full resolution)",
                    a.len(),
                    self.width,
                    self.height
                )));
            }
        }
        let ceil = 1u32 << self.bit_depth;
        let in_range = |p: &[u16]| p.iter().all(|&s| (s as u32) < ceil);
        if !in_range(&self.y)
            || !in_range(&self.u)
            || !in_range(&self.v)
            || !self.alpha.as_deref().map(in_range).unwrap_or(true)
            || !self.depth_map.as_deref().map(in_range).unwrap_or(true)
        {
            return Err(Error::invalid(format!(
                "avif still: sample exceeds bit_depth {} range",
                self.bit_depth
            )));
        }
        Ok(())
    }
}

/// Tuning for [`encode_still`] / [`encode_still_grid`]. The default
/// is a lossless colour encode (`base_q_idx = 0`), lossless alpha,
/// straight (non-premultiplied) alpha signalling.
#[derive(Clone, Copy, Debug, Default)]
pub struct StillEncodeOptions {
    /// AV1 `base_q_idx` for the colour planes. `0` = lossless
    /// (default); higher = lossier / smaller.
    pub base_q_idx: u8,
    /// AV1 `base_q_idx` for the alpha auxiliary. Defaults to `0`
    /// (lossless) — alpha artefacts are far more visible than colour
    /// ones, and flat alpha planes are cheap.
    pub alpha_q_idx: u8,
    /// Emit the `prem` iref declaring the colour planes premultiplied
    /// by alpha (HEIF §6.10.1.1). Signalling only — the samples are
    /// stored as given.
    pub premultiplied_alpha: bool,
}

/// Per-axis coded-extent bound of the single-item encode (mirrors the
/// AV1 KEY-frame encoder's bound). Larger canvases go through
/// [`encode_still_grid`].
pub const STILL_MAX_CODED_DIM: u32 = 4096;

/// Per-axis floor on a grid tile's coded extents. External AVIF readers
/// reject grids whose tiles are narrower or shorter than 64 pixels
/// (the MIAF grid-tile constraint their diagnostics cite as ISO/IEC
/// 23000-22 §7.3.11.4.2), so the tile election never goes below it.
pub const GRID_MIN_TILE_DIM: u32 = 64;

/// Elect a `(columns, rows)` grid tiling for a `width × height` image
/// whose tiles must fit `max_tile` coded pixels per axis (and stay at
/// least [`GRID_MIN_TILE_DIM`]): the fewest tiles per axis that bring
/// the coded tile extent under the bound. Returns `None` when no
/// tiling satisfies both bounds (an axis shorter than the floor that
/// still needs splitting cannot happen for `max_tile ≥ 64`, so this
/// is only `None` for a zero extent or a bound below the floor).
pub fn elect_grid_tiling(width: u32, height: u32, max_tile: u32) -> Option<(u16, u16)> {
    if width == 0 || height == 0 || max_tile < GRID_MIN_TILE_DIM {
        return None;
    }
    let per_axis = |extent: u32| -> Option<u16> {
        let mut count = 1u32;
        loop {
            let tile = coded_extent(extent.div_ceil(count));
            if tile <= max_tile {
                // Never leave a fully-trimmed tile and never go below
                // the floor.
                if (count - 1) * tile >= extent || (count > 1 && tile < GRID_MIN_TILE_DIM) {
                    return None;
                }
                return u16::try_from(count).ok();
            }
            count += 1;
            if count > 256 {
                return None;
            }
        }
    };
    Some((per_axis(width)?, per_axis(height)?))
}

/// Encode `img` as a single coded item when its coded extents fit
/// [`STILL_MAX_CODED_DIM`], otherwise as a grid with the tiling from
/// [`elect_grid_tiling`]. The one-call entry point for arbitrary
/// canvases.
pub fn encode_still_auto(img: &StillImage, opts: &StillEncodeOptions) -> Result<Vec<u8>> {
    let (pw, ph) = (coded_extent(img.width), coded_extent(img.height));
    if pw <= STILL_MAX_CODED_DIM && ph <= STILL_MAX_CODED_DIM {
        return encode_still(img, opts);
    }
    let (columns, rows) = elect_grid_tiling(img.width, img.height, STILL_MAX_CODED_DIM)
        .ok_or_else(|| {
            Error::unsupported(format!(
                "avif still: no grid tiling for {}x{} within {STILL_MAX_CODED_DIM}-pixel tiles",
                img.width, img.height
            ))
        })?;
    encode_still_grid(img, opts, columns, rows)
}

fn widen(p: &[u8]) -> Vec<u16> {
    p.iter().map(|&s| s as u16).collect()
}

/// The `colr` `nclx` triple the RGB constructors signal: BT.709
/// primaries, sRGB transfer, H.273 identity matrix, full range.
fn identity_full_range_colr() -> Colr {
    Colr::Nclx {
        colour_primaries: 1,
        transfer_characteristics: 13,
        matrix_coefficients: 0,
        full_range: true,
    }
}

/// Round `v` up to the coded-extent grid (multiple of 8, min 8).
pub(crate) fn coded_extent(v: u32) -> u32 {
    v.max(8).div_ceil(8) * 8
}

/// Edge-replicate `src` (`w × h`, row-major) out to `pw × ph`.
pub(crate) fn pad_plane(src: &[u16], w: usize, h: usize, pw: usize, ph: usize) -> Vec<u16> {
    debug_assert!(pw >= w && ph >= h && src.len() == w * h);
    if (pw, ph) == (w, h) {
        return src.to_vec();
    }
    let mut out = Vec::with_capacity(pw * ph);
    for row in 0..ph {
        let sr = row.min(h - 1);
        let base = sr * w;
        out.extend_from_slice(&src[base..base + w]);
        let edge = src[base + w - 1];
        out.extend(std::iter::repeat(edge).take(pw - w));
    }
    out
}

/// Copy the `tw × th` rectangle at `(x0, y0)` out of `src`
/// (`w × h`, row-major), edge-replicating anything beyond the source
/// extents (used by the grid splitter for right/bottom overflow
/// tiles).
fn extract_rect(
    src: &[u16],
    w: usize,
    h: usize,
    x0: usize,
    y0: usize,
    tw: usize,
    th: usize,
) -> Vec<u16> {
    debug_assert!(src.len() == w * h && x0 < w && y0 < h);
    let mut out = Vec::with_capacity(tw * th);
    for row in 0..th {
        let sr = (y0 + row).min(h - 1);
        let base = sr * w;
        let avail = w - x0;
        let copy = avail.min(tw);
        out.extend_from_slice(&src[base + x0..base + x0 + copy]);
        if copy < tw {
            let edge = src[base + w - 1];
            out.extend(std::iter::repeat(edge).take(tw - copy));
        }
    }
    out
}

/// Build the 4-byte `av1C` record (av1-isobmff §2.3, no `configOBUs`)
/// mirroring the emitted sequence header — the av1-avif §2.2.1 `shall`
/// ("the values of the fields ... shall match those of the Sequence
/// Header OBU in the AV1 Image Item Data").
pub(crate) fn av1c_from_seq(seq: &SequenceHeader) -> Vec<u8> {
    let op = &seq.operating_points[0];
    let cc = &seq.color_config;
    let b0 = 0x80 | 0x01; // marker=1, version=1
    let b1 = (seq.seq_profile << 5) | (op.seq_level_idx & 0x1f);
    let b2 = ((op.seq_tier & 0x1) << 7)
        | ((cc.high_bitdepth as u8) << 6)
        | ((cc.twelve_bit as u8) << 5)
        | ((cc.mono_chrome as u8) << 4)
        | ((cc.subsampling_x as u8) << 3)
        | ((cc.subsampling_y as u8) << 2)
        | (cc.chroma_sample_position & 0x3);
    vec![b0, b1, b2, 0x00]
}

/// Map an AV1-encoder error into the crate error type with context.
pub(crate) fn av1_err(stage: &str, e: oxideav_av1::Error) -> Error {
    Error::invalid(format!("avif still: AV1 {stage} encode failed: {e}"))
}

/// The `clap` property cropping a `pw × ph` coded frame back to the
/// `w × h` input extents, anchored top-left per the av1-avif §2.2.3
/// `shall` ("the origin of the 'clap' item property shall be anchored
/// to 0,0 (top-left) of the input image"). With the HEIF §6.5.11
/// centre geometry, a top-left anchor means
/// `horizOff = (cleanApertureWidth − frameWidth) / 2` (denominator 2
/// keeps the half-pixel exact), and likewise vertically.
pub(crate) fn top_left_clap(w: u32, h: u32, pw: u32, ph: u32) -> Clap {
    Clap {
        clean_aperture_width_n: w as i32,
        clean_aperture_width_d: 1,
        clean_aperture_height_n: h as i32,
        clean_aperture_height_d: 1,
        horiz_off_n: w as i32 - pw as i32,
        horiz_off_d: 2,
        vert_off_n: h as i32 - ph as i32,
        vert_off_d: 2,
    }
}

/// One coded plane set: the AV1 Image Item Data payload plus its
/// `av1C` record and the elected `seq_profile`.
pub(crate) struct CodedItem {
    pub(crate) payload: Vec<u8>,
    pub(crate) av1c: Vec<u8>,
    pub(crate) seq_profile: u8,
}

/// Re-signal §5.5.2 `color_range = 1` (full range) in the temporal
/// unit's Sequence Header OBU by re-encoding the header descriptor
/// with the flag set and splicing it over the original.
///
/// `color_range` is display metadata — a fixed-width `f(1)` field that
/// no reconstruction step reads — so the spliced stream decodes to
/// identical samples; only the signalled interpretation changes. The
/// samples this module codes are always full-range (they come in as
/// raw pixel values), and the flag matters in two places:
///
/// * the **alpha** auxiliary — av1-avif §4.1 makes it a `shall`: "The
///   color_range field in the Sequence Header OBU shall be set to 1"
///   for every AV1 Auxiliary Image Item (readers also ignore any
///   `colr` on the alpha item per §4.1, so the stream flag is the
///   only range signal — a studio-range flag would make conformant
///   readers rescale the plane);
/// * a full-range `colr` `nclx` on the colour item (e.g. the identity
///   RGB path) — MIAF gives the container property precedence, but a
///   matching in-stream flag keeps both signals consistent.
pub(crate) fn full_range_temporal_unit(unit: &[u8], seq: &SequenceHeader) -> Result<Vec<u8>> {
    let mut seq_full = seq.clone();
    seq_full.color_config.color_range = true;
    let new_sh = encode_sequence_header_obu(&seq_full);
    let mut out = Vec::with_capacity(unit.len());
    let mut off = 0usize;
    let mut replaced = false;
    while off < unit.len() {
        let (desc, consumed) = parse_obu(&unit[off..])
            .map_err(|e| Error::invalid(format!("avif still: OBU walk failed: {e}")))?;
        if desc.obu_type == ObuType::SequenceHeader {
            out.extend_from_slice(&new_sh);
            replaced = true;
        } else {
            out.extend_from_slice(&unit[off..off + consumed]);
        }
        off += consumed;
    }
    if !replaced {
        return Err(Error::invalid(
            "avif still: temporal unit carries no Sequence Header OBU",
        ));
    }
    Ok(out)
}

/// Encode one coded item from (already padded) planes. `full_range`
/// re-signals §5.5.2 `color_range = 1` in the emitted sequence header
/// (see [`full_range_temporal_unit`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_coded_item(
    pw: u32,
    ph: u32,
    bit_depth: u8,
    format: ChromaFormat,
    y: Vec<u16>,
    u: Vec<u16>,
    v: Vec<u16>,
    q: u8,
    full_range: bool,
    stage: &str,
) -> Result<CodedItem> {
    let frame = YuvFrame {
        width: pw,
        height: ph,
        bit_depth,
        format,
        y,
        u,
        v,
    };
    let coded = encode_key_frame_yuv_with_q(&frame, q).map_err(|e| av1_err(stage, e))?;
    // The AV1 Image Item Data is the content of a sync AV1 sample
    // (av1-avif §2.1) — the encoder's §7.5 temporal-unit bytes
    // (TD + SH + frame OBU; exactly one Sequence Header OBU, the
    // §2.1 `shall`).
    let payload = if full_range {
        full_range_temporal_unit(&coded.temporal_unit_bytes, &coded.seq)?
    } else {
        coded.temporal_unit_bytes
    };
    let av1c = av1c_from_seq(&coded.seq);
    Ok(CodedItem {
        payload,
        av1c,
        seq_profile: coded.seq.seq_profile,
    })
}

/// True when the image's `colr` property declares full-range `nclx`
/// samples — the in-stream §5.5.2 `color_range` flag then mirrors it.
pub(crate) fn colr_is_full_range(img: &StillImage) -> bool {
    matches!(
        img.colr,
        Some(Colr::Nclx {
            full_range: true,
            ..
        })
    )
}

/// Pad the image's planes to the coded extents and encode the primary
/// coded item.
fn encode_primary(img: &StillImage, pw: u32, ph: u32, q: u8) -> Result<CodedItem> {
    let (w, h) = (img.width as usize, img.height as usize);
    let y = pad_plane(&img.y, w, h, pw as usize, ph as usize);
    let (u, v) = if img.chroma.has_chroma() {
        let (sx, sy) = img.chroma.subsampling();
        let (cw, ch) = img.chroma_dims();
        let (pcw, pch) = ((pw >> sx) as usize, (ph >> sy) as usize);
        (
            pad_plane(&img.u, cw as usize, ch as usize, pcw, pch),
            pad_plane(&img.v, cw as usize, ch as usize, pcw, pch),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    encode_coded_item(
        pw,
        ph,
        img.bit_depth,
        img.chroma.to_av1(),
        y,
        u,
        v,
        q,
        colr_is_full_range(img),
        "primary",
    )
}

/// Encode the alpha plane as a monochrome coded item at the same
/// coded extents and bit depth as the master (av1-avif §4.1).
fn encode_alpha(img: &StillImage, alpha: &[u16], pw: u32, ph: u32, q: u8) -> Result<CodedItem> {
    let (w, h) = (img.width as usize, img.height as usize);
    let a = pad_plane(alpha, w, h, pw as usize, ph as usize);
    // av1-avif §4.1 `shall`s on the auxiliary stream: `mono_chrome = 1`
    // (satisfied by the Monochrome encode below) and `color_range = 1`
    // (the full-range splice — alpha samples are raw coverage values
    // and readers ignore any `colr` on the alpha item).
    encode_coded_item(
        pw,
        ph,
        img.bit_depth,
        ChromaFormat::Monochrome,
        a,
        Vec::new(),
        Vec::new(),
        q,
        true,
        "alpha",
    )
}

/// `pixi` bits-per-channel list for the image's colour item.
pub(crate) fn pixi_bits(img: &StillImage) -> Vec<u8> {
    let channels = if img.chroma.has_chroma() { 3 } else { 1 };
    vec![img.bit_depth; channels]
}

/// Encode `img` into a complete single-item AVIF file.
///
/// The primary is one AV1 KEY-frame coded `av01` item; alpha (when
/// present) is a second, hidden monochrome item. See the module docs
/// for the property/brand mapping. Coded extents are bounded by
/// [`STILL_MAX_CODED_DIM`] per axis — larger canvases go through
/// [`encode_still_grid`].
pub fn encode_still(img: &StillImage, opts: &StillEncodeOptions) -> Result<Vec<u8>> {
    img.validate()?;
    let (pw, ph) = (coded_extent(img.width), coded_extent(img.height));
    if pw > STILL_MAX_CODED_DIM || ph > STILL_MAX_CODED_DIM {
        return Err(Error::unsupported(format!(
            "avif still: coded extents {pw}x{ph} exceed the single-item bound \
             {STILL_MAX_CODED_DIM}; use encode_still_grid",
        )));
    }
    let primary = encode_primary(img, pw, ph, opts.base_q_idx)?;
    let mut max_profile = primary.seq_profile;

    // ispe documents the coded extents (av1-avif §2.2.2); clap crops
    // back to the requested extents when padding was needed.
    let mut mux = AvifMuxer::new(pw, ph, primary.payload, primary.av1c).with_pixi(pixi_bits(img));
    if (pw, ph) != (img.width, img.height) {
        mux = mux.with_clap(top_left_clap(img.width, img.height, pw, ph));
    }
    if let Some(colr) = &img.colr {
        mux = mux.with_colr(colr.clone());
    }
    let props = &img.props;
    if let Some(exif) = &props.exif {
        mux = mux.with_exif(exif.clone());
    }
    if let Some(xmp) = &props.xmp {
        mux = mux.with_xmp(xmp.clone());
    }
    if let Some(mdcv) = props.mdcv {
        mux = mux.with_mdcv(mdcv);
    }
    if let Some(clli) = props.clli {
        mux = mux.with_clli(clli);
    }
    if let Some(amve) = props.amve {
        mux = mux.with_amve(amve);
    }
    if let Some(pasp) = props.pasp {
        mux = mux.with_pasp(pasp);
    }
    if let Some(angle) = props.irot {
        mux = mux.with_irot(angle);
    }
    if let Some(axis) = props.imir {
        mux = mux.with_imir(axis);
    }
    if let Some(iden) = &props.identity_derivation {
        mux = mux.with_identity_derivation(iden.clone());
    }
    if let Some(alpha) = &img.alpha {
        let coded_alpha = encode_alpha(img, alpha, pw, ph, opts.alpha_q_idx)?;
        max_profile = max_profile.max(coded_alpha.seq_profile);
        mux = mux
            .with_alpha(
                coded_alpha.payload,
                coded_alpha.av1c,
                opts.premultiplied_alpha,
            )
            .with_alpha_pixi(vec![img.bit_depth]);
    }
    if let Some(depth) = &img.depth_map {
        let coded_depth = encode_alpha(img, depth, pw, ph, opts.alpha_q_idx)?;
        max_profile = max_profile.max(coded_depth.seq_profile);
        mux = mux.with_depth(coded_depth.payload, coded_depth.av1c);
    }
    mux = apply_profile_brand_mux(mux, max_profile);
    mux.build()
}

pub(crate) fn apply_profile_brand_mux(mux: AvifMuxer, seq_profile: u8) -> AvifMuxer {
    match seq_profile {
        0 => mux, // Main → Baseline (MA1B) — the muxer default
        1 => mux.advanced_profile(),
        _ => mux.no_profile_brand(),
    }
}

/// Encode `img` as a **grid-derived** AVIF: the canvas splits into
/// `columns × rows` equal coded tiles (HEIF §6.6.2), each an
/// independently coded AV1 KEY frame. Tile extents are
/// `ceil(extent / count)` rounded up to the coded grid; the grid
/// descriptor's declared output extents trim the right/bottom
/// overflow, so no `clap` is needed (per av1-avif §4.2.1 / MIAF,
/// transformative properties may only sit on the grid item anyway).
///
/// Alpha rides as a hidden alpha `grid` of monochrome tiles (`auxl` →
/// the colour grid); pass-through properties land on the grid item.
pub fn encode_still_grid(
    img: &StillImage,
    opts: &StillEncodeOptions,
    columns: u16,
    rows: u16,
) -> Result<Vec<u8>> {
    img.validate()?;
    if img.props.identity_derivation.is_some() {
        return Err(Error::unsupported(
            "avif still: an identity derivation over a grid primary is not supported",
        ));
    }
    if img.depth_map.is_some() {
        return Err(Error::unsupported(
            "avif still: a depth map on the grid encode path is not supported",
        ));
    }
    if columns == 0 || rows == 0 {
        return Err(Error::invalid("avif still: grid needs at least 1x1 tiles"));
    }
    let tile_w = coded_extent(img.width.div_ceil(columns as u32));
    let tile_h = coded_extent(img.height.div_ceil(rows as u32));
    if tile_w > STILL_MAX_CODED_DIM || tile_h > STILL_MAX_CODED_DIM {
        return Err(Error::unsupported(format!(
            "avif still: {columns}x{rows} tiling of {}x{} needs {tile_w}x{tile_h} tiles, \
             beyond the per-tile bound {STILL_MAX_CODED_DIM}",
            img.width, img.height
        )));
    }
    if tile_w < GRID_MIN_TILE_DIM || tile_h < GRID_MIN_TILE_DIM {
        return Err(Error::invalid(format!(
            "avif still: {columns}x{rows} tiling of {}x{} yields {tile_w}x{tile_h} tiles, \
             below the {GRID_MIN_TILE_DIM}-pixel grid-tile floor external readers enforce",
            img.width, img.height
        )));
    }
    // Every tile must contribute visible pixels (HEIF §6.6.2.3.1: the
    // tiled extent covers the canvas, and trimming happens only on the
    // last row/column).
    if (columns as u32 - 1) * tile_w >= img.width || (rows as u32 - 1) * tile_h >= img.height {
        return Err(Error::invalid(format!(
            "avif still: {columns}x{rows} tiling of {}x{} leaves fully-trimmed tiles \
             (tile extents {tile_w}x{tile_h})",
            img.width, img.height
        )));
    }

    let (w, h) = (img.width as usize, img.height as usize);
    let (sx, sy) = img.chroma.subsampling();
    let (cw, ch) = img.chroma_dims();
    let mut muxer =
        AvifGridMuxer::new(rows, columns, img.width, img.height).with_pixi(pixi_bits(img));
    if let Some(colr) = &img.colr {
        muxer = muxer.with_colr(colr.clone());
    }
    let props = &img.props;
    if let Some(exif) = &props.exif {
        muxer = muxer.with_exif(exif.clone());
    }
    if let Some(xmp) = &props.xmp {
        muxer = muxer.with_xmp(xmp.clone());
    }
    if let Some(mdcv) = props.mdcv {
        muxer = muxer.with_mdcv(mdcv);
    }
    if let Some(clli) = props.clli {
        muxer = muxer.with_clli(clli);
    }
    if let Some(amve) = props.amve {
        muxer = muxer.with_amve(amve);
    }
    if let Some(pasp) = props.pasp {
        muxer = muxer.with_pasp(pasp);
    }
    if let Some(angle) = props.irot {
        muxer = muxer.with_irot(angle);
    }
    if let Some(axis) = props.imir {
        muxer = muxer.with_imir(axis);
    }
    let mut max_profile = 0u8;
    for r in 0..rows as usize {
        for c in 0..columns as usize {
            let x0 = c * tile_w as usize;
            let y0 = r * tile_h as usize;
            let ty = extract_rect(&img.y, w, h, x0, y0, tile_w as usize, tile_h as usize);
            let (tu, tv) = if img.chroma.has_chroma() {
                let (tcx, tcy) = (x0 >> sx, y0 >> sy);
                let (tcw, tch) = ((tile_w >> sx) as usize, (tile_h >> sy) as usize);
                (
                    extract_rect(&img.u, cw as usize, ch as usize, tcx, tcy, tcw, tch),
                    extract_rect(&img.v, cw as usize, ch as usize, tcx, tcy, tcw, tch),
                )
            } else {
                (Vec::new(), Vec::new())
            };
            let coded = encode_coded_item(
                tile_w,
                tile_h,
                img.bit_depth,
                img.chroma.to_av1(),
                ty,
                tu,
                tv,
                opts.base_q_idx,
                colr_is_full_range(img),
                "grid tile",
            )?;
            max_profile = max_profile.max(coded.seq_profile);
            muxer = muxer.tile(GridTile {
                width: tile_w,
                height: tile_h,
                payload: coded.payload,
                av1c: coded.av1c,
            });
            if let Some(alpha) = &img.alpha {
                // Alpha tiles: same geometry, monochrome, full range
                // (av1-avif §4.1) — stitched by a hidden alpha `grid`.
                let ta = extract_rect(alpha, w, h, x0, y0, tile_w as usize, tile_h as usize);
                let coded_alpha = encode_coded_item(
                    tile_w,
                    tile_h,
                    img.bit_depth,
                    ChromaFormat::Monochrome,
                    ta,
                    Vec::new(),
                    Vec::new(),
                    opts.alpha_q_idx,
                    true,
                    "grid alpha tile",
                )?;
                max_profile = max_profile.max(coded_alpha.seq_profile);
                muxer = muxer.alpha_tile(GridTile {
                    width: tile_w,
                    height: tile_h,
                    payload: coded_alpha.payload,
                    av1c: coded_alpha.av1c,
                });
            }
        }
    }
    if img.alpha.is_some() {
        muxer = muxer
            .with_alpha_pixi(vec![img.bit_depth])
            .premultiplied_alpha(opts.premultiplied_alpha);
    }
    muxer = match max_profile {
        0 => muxer,
        1 => muxer.advanced_profile(),
        _ => muxer.no_profile_brand(),
    };
    muxer.build()
}

/// The canvas of an overlay-derived encode ([`encode_still_overlay`]).
#[derive(Clone, Debug)]
pub struct OverlayCanvas {
    /// `output_width` of the `iovl` descriptor (HEIF §6.6.2.2.3).
    pub width: u32,
    /// `output_height`.
    pub height: u32,
    /// `canvas_fill_value` — sRGB RGBA, 16 bits per channel, `A`
    /// linear 0 (transparent) … 65535 (opaque). Shows wherever no
    /// layer covers the canvas.
    pub fill_rgba: [u16; 4],
    /// `colr` for the overlay item; defaults to the first layer's.
    pub colr: Option<Colr>,
    /// `irot` on the overlay item (applies to the composed canvas).
    pub irot: Option<u8>,
    /// `imir` on the overlay item.
    pub imir: Option<u8>,
}

impl OverlayCanvas {
    /// An opaque-black canvas of the given extents.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            fill_rgba: [0, 0, 0, 65535],
            colr: None,
            irot: None,
            imir: None,
        }
    }
}

/// One layer of an overlay-derived encode: a still image and the
/// canvas position of its top-left corner (signed — partially or
/// wholly off-canvas placements are legal and clipped, §6.6.2.2.3).
#[derive(Clone, Copy, Debug)]
pub struct OverlayLayerImage<'a> {
    /// The layer's pixels (alpha optional, encoded as its auxiliary).
    pub image: &'a StillImage,
    /// `horizontal_offset`.
    pub x: i32,
    /// `vertical_offset`.
    pub y: i32,
}

/// Encode `layers` (bottom-most first) as an **overlay-derived** AVIF:
/// the primary is an `iovl` item (HEIF §6.6.2.2) whose hidden `av01`
/// inputs are the layers, each coded exactly like an [`encode_still`]
/// primary (edge-replicated pad to the coded 8-grid + top-left `clap`,
/// same-depth monochrome alpha auxiliary with `auxl` / `prem`). Every
/// layer must share one bit depth and chroma layout so a reader can
/// composite them without colour conversion.
pub fn encode_still_overlay(
    canvas: &OverlayCanvas,
    layers: &[OverlayLayerImage<'_>],
    opts: &StillEncodeOptions,
) -> Result<Vec<u8>> {
    if layers.is_empty() {
        return Err(Error::invalid(
            "avif still: overlay needs at least one layer",
        ));
    }
    if canvas.width == 0 || canvas.height == 0 {
        return Err(Error::invalid("avif still: overlay canvas dimensions zero"));
    }
    let first = layers[0].image;
    let mut muxer = AvifOverlayMuxer::new(canvas.width, canvas.height)
        .with_fill(canvas.fill_rgba)
        .with_pixi(pixi_bits(first));
    if let Some(colr) = canvas.colr.as_ref().or(first.colr.as_ref()) {
        muxer = muxer.with_colr(colr.clone());
    }
    if let Some(angle) = canvas.irot {
        muxer = muxer.with_irot(angle);
    }
    if let Some(axis) = canvas.imir {
        muxer = muxer.with_imir(axis);
    }
    let mut max_profile = 0u8;
    for (i, layer) in layers.iter().enumerate() {
        let img = layer.image;
        img.validate()?;
        if (img.bit_depth, img.chroma) != (first.bit_depth, first.chroma) {
            return Err(Error::unsupported(format!(
                "avif still: overlay layer {i} is {}-bit {:?} but layer 0 is {}-bit {:?} — \
                 layers must share one coded layout",
                img.bit_depth, img.chroma, first.bit_depth, first.chroma
            )));
        }
        let (pw, ph) = (coded_extent(img.width), coded_extent(img.height));
        if pw > STILL_MAX_CODED_DIM || ph > STILL_MAX_CODED_DIM {
            return Err(Error::unsupported(format!(
                "avif still: overlay layer {i} coded extents {pw}x{ph} exceed {STILL_MAX_CODED_DIM}"
            )));
        }
        let coded = encode_primary(img, pw, ph, opts.base_q_idx)?;
        max_profile = max_profile.max(coded.seq_profile);
        let clap = ((pw, ph) != (img.width, img.height))
            .then(|| top_left_clap(img.width, img.height, pw, ph));
        let alpha = match &img.alpha {
            Some(a) => {
                let coded_alpha = encode_alpha(img, a, pw, ph, opts.alpha_q_idx)?;
                max_profile = max_profile.max(coded_alpha.seq_profile);
                Some((coded_alpha.payload, coded_alpha.av1c))
            }
            None => None,
        };
        muxer = muxer.layer(OverlayLayer {
            width: pw,
            height: ph,
            payload: coded.payload,
            av1c: coded.av1c,
            pixi: Some(pixi_bits(img)),
            colr: img.colr.clone(),
            clap,
            alpha_pixi: alpha.as_ref().map(|_| vec![img.bit_depth]),
            alpha,
            premultiplied: opts.premultiplied_alpha,
            offset_x: layer.x,
            offset_y: layer.y,
        });
    }
    muxer = match max_profile {
        0 => muxer,
        1 => muxer.advanced_profile(),
        _ => muxer.no_profile_brand(),
    };
    muxer.build()
}

/// Per-layer byte sizes of a layered AV1 Image Item Data payload, in
/// increasing `spatial_id` order — the `a1lx` `layer_size` values
/// (av1-avif §2.3.2.3.4). OBUs without an extension header (temporal
/// delimiter, sequence header) belong to `spatial_id` 0.
#[doc(hidden)]
pub fn layer_byte_sizes(payload: &[u8]) -> Result<Vec<u32>> {
    let mut sizes = [0u32; 4];
    let mut max_layer = 0usize;
    let mut off = 0usize;
    while off < payload.len() {
        let (obu, consumed) = parse_obu(&payload[off..])
            .map_err(|e| Error::invalid(format!("avif still: layered payload OBU walk: {e}")))?;
        let layer = usize::from(obu.spatial_id.min(3));
        max_layer = max_layer.max(layer);
        sizes[layer] += consumed as u32;
        off += consumed;
    }
    Ok(sizes[..=max_layer].to_vec())
}

/// Encode a **layered** (progressive) AVIF: `layers` are renditions of
/// one image at increasing resolution — every layer at most the top
/// layer's extents, all sharing one bit depth and chroma layout, 2 to
/// 4 of them — coded as independently decodable AV1 spatial layers
/// inside a single temporal unit (one Sequence Header, av1-avif §2.1;
/// each layer's frame OBUs carry `spatial_id`, §2.3.1). The item
/// carries `a1lx` (per-layer byte sizes, §2.3.2.3) and `lsel`
/// (HEIF §6.5.11): `render_layer = None` writes `0xFFFF`, allowing a
/// reader to render every layer progressively and finish on the top
/// one; `Some(s)` pins layer `s`. `ispe` / `clap` describe the
/// selected layer (§2.2.2: the last frame decoded at operating point 0
/// when unpinned).
///
/// Alpha (of the top layer only) rides as the usual single-layer
/// auxiliary. Pass-through properties come from the top layer.
pub fn encode_still_layered(
    layers: &[&StillImage],
    render_layer: Option<u8>,
    opts: &StillEncodeOptions,
) -> Result<Vec<u8>> {
    if !(2..=4).contains(&layers.len()) {
        return Err(Error::invalid(format!(
            "avif still: layered encode needs 2..=4 layers, got {}",
            layers.len()
        )));
    }
    let top = layers[layers.len() - 1];
    top.validate()?;
    let mut frames: Vec<Vec<YuvFrame>> = Vec::with_capacity(layers.len());
    let mut coded_dims = Vec::with_capacity(layers.len());
    for (i, layer) in layers.iter().enumerate() {
        layer.validate()?;
        if (layer.bit_depth, layer.chroma) != (top.bit_depth, top.chroma) {
            return Err(Error::unsupported(format!(
                "avif still: layer {i} is {}-bit {:?} but the top layer is {}-bit {:?}",
                layer.bit_depth, layer.chroma, top.bit_depth, top.chroma
            )));
        }
        if layer.width > top.width || layer.height > top.height {
            return Err(Error::invalid(format!(
                "avif still: layer {i} ({}x{}) exceeds the top layer ({}x{})",
                layer.width, layer.height, top.width, top.height
            )));
        }
        let (pw, ph) = (coded_extent(layer.width), coded_extent(layer.height));
        if pw > STILL_MAX_CODED_DIM || ph > STILL_MAX_CODED_DIM {
            return Err(Error::unsupported(format!(
                "avif still: layer {i} coded extents {pw}x{ph} exceed {STILL_MAX_CODED_DIM}"
            )));
        }
        let (w, h) = (layer.width as usize, layer.height as usize);
        let y = pad_plane(&layer.y, w, h, pw as usize, ph as usize);
        let (u, v) = if layer.chroma.has_chroma() {
            let (sx, sy) = layer.chroma.subsampling();
            let (cw, ch) = layer.chroma_dims();
            let (pcw, pch) = ((pw >> sx) as usize, (ph >> sy) as usize);
            (
                pad_plane(&layer.u, cw as usize, ch as usize, pcw, pch),
                pad_plane(&layer.v, cw as usize, ch as usize, pcw, pch),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        frames.push(vec![YuvFrame {
            width: pw,
            height: ph,
            bit_depth: layer.bit_depth,
            format: layer.chroma.to_av1(),
            y,
            u,
            v,
        }]);
        coded_dims.push((pw, ph));
    }
    let selected = match render_layer {
        Some(s) if usize::from(s) < layers.len() => usize::from(s),
        Some(s) => {
            return Err(Error::invalid(format!(
                "avif still: render_layer {s} out of range for {} layers",
                layers.len()
            )))
        }
        None => layers.len() - 1,
    };
    let coded = encode_spatial_layered_gop_yuv_with_q(&frames, opts.base_q_idx)
        .map_err(|e| av1_err("layered", e))?;
    let unit =
        coded.temporal_units.into_iter().next().ok_or_else(|| {
            Error::invalid("avif still: layered encode produced no temporal unit")
        })?;
    let payload = if colr_is_full_range(top) {
        full_range_temporal_unit(&unit, &coded.seq)?
    } else {
        unit
    };
    let sizes = layer_byte_sizes(&payload)?;
    if sizes.len() != layers.len() {
        return Err(Error::invalid(format!(
            "avif still: layered payload carries {} spatial layers, expected {}",
            sizes.len(),
            layers.len()
        )));
    }
    let mut a1lx = [0u32; 3];
    for (slot, size) in a1lx.iter_mut().zip(&sizes[..sizes.len() - 1]) {
        *slot = *size;
    }
    let av1c = av1c_from_seq(&coded.seq);
    let mut max_profile = coded.seq.seq_profile;

    let (pw, ph) = coded_dims[selected];
    let sel_img = layers[selected];
    let mut mux = AvifMuxer::new(pw, ph, payload, av1c)
        .with_pixi(pixi_bits(top))
        .with_layered_index(a1lx)
        .with_layer_selector(render_layer.map_or(0xFFFF, u16::from));
    if (pw, ph) != (sel_img.width, sel_img.height) {
        mux = mux.with_clap(top_left_clap(sel_img.width, sel_img.height, pw, ph));
    }
    if let Some(colr) = &top.colr {
        mux = mux.with_colr(colr.clone());
    }
    let props = &top.props;
    if let Some(exif) = &props.exif {
        mux = mux.with_exif(exif.clone());
    }
    if let Some(xmp) = &props.xmp {
        mux = mux.with_xmp(xmp.clone());
    }
    if let Some(mdcv) = props.mdcv {
        mux = mux.with_mdcv(mdcv);
    }
    if let Some(clli) = props.clli {
        mux = mux.with_clli(clli);
    }
    if let Some(amve) = props.amve {
        mux = mux.with_amve(amve);
    }
    if let Some(pasp) = props.pasp {
        mux = mux.with_pasp(pasp);
    }
    if let Some(angle) = props.irot {
        mux = mux.with_irot(angle);
    }
    if let Some(axis) = props.imir {
        mux = mux.with_imir(axis);
    }
    if selected == layers.len() - 1 {
        if let Some(alpha) = &top.alpha {
            let coded_alpha = encode_alpha(top, alpha, pw, ph, opts.alpha_q_idx)?;
            max_profile = max_profile.max(coded_alpha.seq_profile);
            mux = mux
                .with_alpha(
                    coded_alpha.payload,
                    coded_alpha.av1c,
                    opts.premultiplied_alpha,
                )
                .with_alpha_pixi(vec![top.bit_depth]);
        }
    }
    mux = apply_profile_brand_mux(mux, max_profile);
    mux.build()
}
