//! The workspace **image-crate API** (`IMAGE_CRATE_API.md`): the small
//! root vocabulary every `oxideav-<format>` picture crate shares —
//! `probe` / `info` / `decode*` / `encode*`, the pixel type
//! [`AvifImage`], the raw `RgbImage` / `RgbaImage` records and the
//! option records.
//!
//! AVIF is a container whose pixels come from the AV1 codec crate,
//! which is framework-only by rule. So the split is:
//!
//! * **standalone** (no `registry` feature, no `oxideav-core`):
//!   [`probe`], [`info`], the container model ([`crate::parse`] /
//!   [`crate::parse_header`] / [`crate::inspect`]), composition over
//!   caller-supplied planes ([`crate::grid`], [`crate::alpha`],
//!   [`crate::transform`]), and every type on this page — including
//!   [`AvifImage::to_rgb8`] / [`AvifImage::to_rgba8`], which convert
//!   planes a caller decoded with its own AV1 decoder;
//! * **`registry`** (default on): [`crate::decode`], [`crate::decode_with`],
//!   [`crate::decode_rgb8`], [`crate::decode_rgba8`], [`crate::decode_all`],
//!   [`crate::decode_from`], [`crate::encode`], [`crate::encode_rgb8`],
//!   [`crate::encode_rgba8`], [`crate::encode_to`] — the AV1 decode and
//!   encode behind them is `oxideav-av1`.
//!
//! # Pixel layouts
//!
//! [`AvifImage::format`] is the storage layout ([`PixelFormat`] =
//! [`AvifPixelFormat`], variant names mirroring
//! `oxideav_core::PixelFormat`): planar Y′CbCr at 4:2:0 / 4:2:2 /
//! 4:4:4, monochrome, each with or without a full-resolution alpha
//! plane, at 8 bits (one byte per sample) or 10 / 12 bits (one
//! little-endian 16-bit word per sample, value in the low bits), plus
//! the packed monochrome + alpha layouts `Ya8` / `Ya16Le`. An
//! **identity-matrix** item (`color.matrix == 0`, the way AVIF stores
//! RGB) decodes to the 4:4:4 layout with the planes holding **G, B, R**
//! (H.273 §8.3: Y = G, Cb = B, Cr = R); [`AvifImage::from_rgb8`] /
//! [`AvifImage::from_rgba8`] build exactly that, so RGB goes in and out
//! without arithmetic.
//!
//! # RGB conversion
//!
//! [`AvifImage::to_rgb8`] / [`AvifImage::to_rgba8`] are exact per
//! sample: the H.273 matrix of [`AvifImage::color`] (identity copies,
//! BT.601 / BT.709 / BT.2020 non-constant-luminance and the other
//! `Kr` / `Kb` matrices invert the §8.3 equations, limited or full
//! range as signalled, round-to-nearest, clipped), sub-sampled chroma
//! replicated to its co-sited luma samples (no interpolation), and
//! 10 / 12-bit samples brought to 8 bits as `round(v × 255 / (2^d − 1))`.
//! Alpha rides through the same depth scaling; an image without alpha
//! yields opaque `255`.

use std::time::Duration;

use crate::error::{AvifError as Error, Result};
use crate::image::{AvifFrame, AvifPixelFormat, AvifPlane};
use crate::meta::Colr;
use oxideav_heif::props as hprops;

/// The pixel-layout enum under the contract's name (variant names
/// mirror `oxideav_core::PixelFormat`).
pub type PixelFormat = AvifPixelFormat;

/// One plane of an [`AvifImage`] (`stride` bytes per row, `data`
/// row-major).
pub type Plane = AvifPlane;

// ---------------------------------------------------------------------
// Colour + metadata
// ---------------------------------------------------------------------

/// The H.273 colour description of a picture: nominal range plus the
/// `ColourPrimaries` / `TransferCharacteristics` /
/// `MatrixCoefficients` code points (one byte each on the wire).
///
/// Filled from the item's `colr` `nclx` on decode; when a file carries
/// none (or only an ICC `colr`) the MIAF §7.3.6.4 default applies —
/// BT.709 primaries (1), sRGB transfer (13), BT.601 matrix (6), full
/// range — which is also [`Default`] here.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    /// `VideoFullRangeFlag`: `true` = full (PC) range, `false` =
    /// limited (studio) range.
    pub full_range: bool,
    /// H.273 §8.1 `ColourPrimaries` (1 = BT.709, 9 = BT.2020, 12 =
    /// Display P3, 2 = unspecified).
    pub primaries: u8,
    /// H.273 §8.2 `TransferCharacteristics` (13 = sRGB, 16 = PQ, 18 =
    /// HLG, 2 = unspecified).
    pub transfer: u8,
    /// H.273 §8.3 `MatrixCoefficients` (0 = identity / RGB, 1 = BT.709,
    /// 6 = BT.601, 9 = BT.2020 NCL, 2 = unspecified).
    pub matrix: u8,
}

impl ColorInfo {
    /// Every field as a positional argument, in declaration order.
    pub const fn new(full_range: bool, primaries: u8, transfer: u8, matrix: u8) -> Self {
        Self {
            full_range,
            primaries,
            transfer,
            matrix,
        }
    }

    /// The MIAF §7.3.6.4 default for an item without CICP: BT.709
    /// primaries, sRGB transfer, BT.601 matrix, full range.
    pub const fn miaf_default() -> Self {
        Self::new(true, 1, 13, 6)
    }

    /// The description the RGB constructors use: BT.709 primaries,
    /// sRGB transfer, the identity matrix (planes are G, B, R), full
    /// range.
    pub const fn identity_full_range() -> Self {
        Self::new(true, 1, 13, 0)
    }

    /// The effective colour of an item from its `colr` (MIAF
    /// §7.3.6.4): the `nclx` code points as signalled, else
    /// [`Self::miaf_default`]. Code points past 255 read as unspecified
    /// (2).
    pub fn from_colr(colr: Option<&Colr>) -> Self {
        match colr {
            Some(Colr::Nclx {
                colour_primaries,
                transfer_characteristics,
                matrix_coefficients,
                full_range,
            }) => {
                let code = |v: u16| u8::try_from(v).unwrap_or(2);
                Self::new(
                    *full_range,
                    code(*colour_primaries),
                    code(*transfer_characteristics),
                    code(*matrix_coefficients),
                )
            }
            _ => Self::miaf_default(),
        }
    }

    /// The `colr` `nclx` property carrying this description.
    pub fn to_colr(&self) -> Colr {
        Colr::Nclx {
            colour_primaries: u16::from(self.primaries),
            transfer_characteristics: u16::from(self.transfer),
            matrix_coefficients: u16::from(self.matrix),
            full_range: self.full_range,
        }
    }

    /// `true` for `MatrixCoefficients == 0`: the planes are G, B, R.
    pub fn is_identity_matrix(&self) -> bool {
        self.matrix == 0
    }

    /// Setter: replace `full_range`.
    pub fn with_full_range(mut self, full_range: bool) -> Self {
        self.full_range = full_range;
        self
    }
    /// Setter: replace `primaries`.
    pub fn with_primaries(mut self, primaries: u8) -> Self {
        self.primaries = primaries;
        self
    }
    /// Setter: replace `transfer`.
    pub fn with_transfer(mut self, transfer: u8) -> Self {
        self.transfer = transfer;
        self
    }
    /// Setter: replace `matrix`.
    pub fn with_matrix(mut self, matrix: u8) -> Self {
        self.matrix = matrix;
        self
    }

    fn to_heif_colr(self) -> hprops::Colr {
        hprops::Colr::from(&self.to_colr())
    }
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self::miaf_default()
    }
}

/// Embedded metadata of a picture, each as the raw bytes a consumer
/// hands to its own parser.
///
/// * `icc` — the ICC profile of an ICC `colr` (`rICC` / `prof`).
/// * `exif` — the Exif payload from the TIFF header on (the HEIF
///   A.2.1 `exif_tiff_header_offset` word and any bytes before the
///   TIFF header are stripped on decode and re-written on encode).
/// * `xmp` — the XMP packet (`application/rdf+xml` `mime` item).
/// * `gamma` — AVIF carries no gamma value; always `None` on decode,
///   ignored on encode.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metadata {
    pub icc: Option<Vec<u8>>,
    pub exif: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    pub gamma: Option<f32>,
}

impl Metadata {
    /// Every field as a positional argument, in declaration order.
    pub fn new(
        icc: Option<Vec<u8>>,
        exif: Option<Vec<u8>>,
        xmp: Option<Vec<u8>>,
        gamma: Option<f32>,
    ) -> Self {
        Self {
            icc,
            exif,
            xmp,
            gamma,
        }
    }
    /// Setter: replace `icc`.
    pub fn with_icc(mut self, icc: Option<Vec<u8>>) -> Self {
        self.icc = icc;
        self
    }
    /// Setter: replace `exif`.
    pub fn with_exif(mut self, exif: Option<Vec<u8>>) -> Self {
        self.exif = exif;
        self
    }
    /// Setter: replace `xmp`.
    pub fn with_xmp(mut self, xmp: Option<Vec<u8>>) -> Self {
        self.xmp = xmp;
        self
    }
    /// Setter: replace `gamma`.
    pub fn with_gamma(mut self, gamma: Option<f32>) -> Self {
        self.gamma = gamma;
        self
    }

    /// `true` when nothing is embedded.
    pub fn is_empty(&self) -> bool {
        self.icc.is_none() && self.exif.is_none() && self.xmp.is_none() && self.gamma.is_none()
    }
}

/// The Exif payload from the TIFF header on, out of a HEIF `Exif`
/// item body (A.2.1: a 4-byte big-endian `exif_tiff_header_offset`
/// then the Exif data; the TIFF header sits `offset` bytes into the
/// data). `None` when the body is too short for its own offset.
#[cfg_attr(not(feature = "registry"), allow(dead_code))]
pub(crate) fn exif_tiff_bytes(body: &[u8]) -> Option<Vec<u8>> {
    if body.len() < 4 {
        return None;
    }
    let offset = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    let start = 4usize.checked_add(offset)?;
    if start > body.len() {
        return None;
    }
    Some(body[start..].to_vec())
}

/// The HEIF `Exif` item body for a TIFF-header-first Exif payload: a
/// zero `exif_tiff_header_offset` word then the bytes.
#[cfg_attr(not(feature = "registry"), allow(dead_code))]
pub(crate) fn exif_item_body(tiff: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + tiff.len());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(tiff);
    out
}

// ---------------------------------------------------------------------
// Raw RGB records
// ---------------------------------------------------------------------

/// A tightly packed 8-bit RGB picture: `3 × width` bytes per row,
/// row-major, no padding.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbImage {
    pub width: u32,
    pub height: u32,
    /// `width × height × 3` bytes, R G B per pixel.
    pub data: Vec<u8>,
}

impl RgbImage {
    /// Every field as a positional argument, in declaration order.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }
    /// The packed bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
    /// The packed bytes, owned.
    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }
}

/// A tightly packed 8-bit RGBA picture: `4 × width` bytes per row,
/// row-major, straight (non-premultiplied) alpha.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    /// `width × height × 4` bytes, R G B A per pixel.
    pub data: Vec<u8>,
}

impl RgbaImage {
    /// Every field as a positional argument, in declaration order.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }
    /// The packed bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
    /// The packed bytes, owned.
    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }
}

// ---------------------------------------------------------------------
// The pixel type
// ---------------------------------------------------------------------

/// A decoded (or to-be-encoded) AVIF picture in its native planar
/// layout — the contract's `XxxImage`.
///
/// `planes` holds `format.plane_count()` planes in Y, Cb, Cr, A order
/// (one packed plane for `Ya8` / `Ya16Le`), each at its own extent
/// (chroma planes at the sub-sampled size, ceiling division). Rows are
/// `stride` bytes apart; a tightly packed plane has
/// `stride == plane_width × bytes_per_sample`.
///
/// Build one with [`AvifImage::new`] (any layout), [`AvifImage::from_rgb8`]
/// / [`AvifImage::from_rgba8`] (identity-matrix 4:4:4, exact), or
/// through the `registry`-gated [`crate::decode`] family.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct AvifImage {
    pub width: u32,
    pub height: u32,
    /// Storage layout.
    pub format: PixelFormat,
    /// `format.plane_count()` planes, Y Cb Cr A (or the packed YA plane).
    pub planes: Vec<Plane>,
    /// Range + H.273 primaries / transfer / matrix.
    pub color: ColorInfo,
    /// ICC / Exif / XMP, when the file carried them.
    pub metadata: Metadata,
    /// Coded sample depth — 8, 10 or 12. Equals `format.bit_depth()`
    /// for every layout but `Ya16Le`, whose 16-bit words hold 10- or
    /// 12-bit values.
    pub bit_depth: u8,
}

impl AvifImage {
    /// A picture from its planes; colour defaults to the MIAF default
    /// ([`ColorInfo::miaf_default`]), metadata to none, `bit_depth` to
    /// the layout's (set [`Self::with_bit_depth`] for `Ya16Le`). The
    /// planes are not validated here — a mismatch surfaces as an error
    /// from the functions that read them.
    pub fn new(width: u32, height: u32, format: PixelFormat, planes: Vec<Plane>) -> Self {
        Self {
            width,
            height,
            format,
            planes,
            color: ColorInfo::default(),
            metadata: Metadata::default(),
            bit_depth: format.bit_depth(),
        }
    }

    /// Packed 8-bit RGB (`3 × width × height` bytes) as an
    /// identity-matrix 4:4:4 picture: planes G, B, R, full range,
    /// BT.709 primaries / sRGB transfer. Exact in both directions.
    ///
    /// # Panics
    ///
    /// When `data.len() != 3 × width × height`.
    pub fn from_rgb8(width: u32, height: u32, data: Vec<u8>) -> Self {
        let n = width as usize * height as usize;
        assert_eq!(
            data.len(),
            n * 3,
            "AvifImage::from_rgb8: {width}x{height} RGB needs {} bytes, got {}",
            n * 3,
            data.len()
        );
        let mut g = Vec::with_capacity(n);
        let mut b = Vec::with_capacity(n);
        let mut r = Vec::with_capacity(n);
        for px in data.chunks_exact(3) {
            r.push(px[0]);
            g.push(px[1]);
            b.push(px[2]);
        }
        let w = width as usize;
        let planes = vec![Plane::new(w, g), Plane::new(w, b), Plane::new(w, r)];
        Self::new(width, height, PixelFormat::Yuv444P, planes)
            .with_color(ColorInfo::identity_full_range())
    }

    /// Packed 8-bit RGBA (`4 × width × height` bytes) as an
    /// identity-matrix 4:4:4 picture with a full-resolution alpha
    /// plane (planes G, B, R, A). Exact in both directions.
    ///
    /// # Panics
    ///
    /// When `data.len() != 4 × width × height`.
    pub fn from_rgba8(width: u32, height: u32, data: Vec<u8>) -> Self {
        let n = width as usize * height as usize;
        assert_eq!(
            data.len(),
            n * 4,
            "AvifImage::from_rgba8: {width}x{height} RGBA needs {} bytes, got {}",
            n * 4,
            data.len()
        );
        let mut g = Vec::with_capacity(n);
        let mut b = Vec::with_capacity(n);
        let mut r = Vec::with_capacity(n);
        let mut a = Vec::with_capacity(n);
        for px in data.chunks_exact(4) {
            r.push(px[0]);
            g.push(px[1]);
            b.push(px[2]);
            a.push(px[3]);
        }
        let w = width as usize;
        let planes = vec![
            Plane::new(w, g),
            Plane::new(w, b),
            Plane::new(w, r),
            Plane::new(w, a),
        ];
        Self::new(width, height, PixelFormat::Yuva444P, planes)
            .with_color(ColorInfo::identity_full_range())
    }

    /// Setter: replace `color`.
    pub fn with_color(mut self, color: ColorInfo) -> Self {
        self.color = color;
        self
    }
    /// Setter: replace `metadata`.
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = metadata;
        self
    }
    /// Setter: replace `bit_depth` (8, 10 or 12; meaningful for
    /// `Ya16Le`, whose storage width is not its coded depth).
    pub fn with_bit_depth(mut self, bit_depth: u8) -> Self {
        self.bit_depth = bit_depth;
        self
    }

    /// Luma width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }
    /// Luma height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Storage layout.
    pub fn format(&self) -> PixelFormat {
        self.format
    }
    /// Coded sample depth (8 / 10 / 12).
    pub fn bit_depth(&self) -> u8 {
        self.bit_depth
    }
    /// `true` when the layout carries an alpha plane / channel.
    pub fn has_alpha(&self) -> bool {
        self.format.has_alpha()
    }

    /// The single plane's bytes for the one-plane layouts (`Gray8` /
    /// `Gray10Le` / `Gray12Le`, packed `Ya8` / `Ya16Le`); `None` for
    /// the planar Y′CbCr layouts — use [`Self::into_raw`].
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if self.planes.len() == 1 {
            self.planes.first().map(|p| p.data.as_slice())
        } else {
            None
        }
    }

    /// Every plane's bytes concatenated in plane order, strides as
    /// reported in `planes`.
    pub fn into_raw(self) -> Vec<u8> {
        let total: usize = self.planes.iter().map(|p| p.data.len()).sum();
        let mut out = Vec::with_capacity(total);
        for p in self.planes {
            out.extend_from_slice(&p.data);
        }
        out
    }

    /// The picture as tightly packed 8-bit RGB (see the module docs for
    /// the exact conversion). Alpha, if any, is dropped.
    ///
    /// A `MatrixCoefficients` the converter has no kernel for — the
    /// reserved code points, unspecified (2), the constant-luminance
    /// BT.2020 (10), chromaticity-derived (12 / 13) and ICtCp (14)
    /// matrices, YCgCo (8) — is converted as the MIAF default BT.601 (6)
    /// at the signalled range; [`Self::try_to_rgb8`] reports it as
    /// [`Error::Unsupported`] instead for callers that must know.
    ///
    /// # Panics
    ///
    /// Only when the planes assigned to this image do not cover
    /// `width × height` at `format` (an image produced by this crate
    /// always does); [`Self::try_to_rgb8`] reports that as an error.
    pub fn to_rgb8(&self) -> Vec<u8> {
        self.rgb_bytes_or_bt601(3)
            .expect("AvifImage::to_rgb8: planes inconsistent with width/height/format")
    }

    /// The picture as tightly packed 8-bit RGBA, alpha opaque (`255`)
    /// when the layout has none. Matrix fallback as [`Self::to_rgb8`].
    ///
    /// # Panics
    ///
    /// As [`Self::to_rgb8`].
    pub fn to_rgba8(&self) -> Vec<u8> {
        self.rgb_bytes_or_bt601(4)
            .expect("AvifImage::to_rgba8: planes inconsistent with width/height/format")
    }

    /// Fallible, exact [`Self::to_rgb8`]: [`Error::Unsupported`] for a
    /// matrix without a kernel (no BT.601 fallback),
    /// [`Error::InvalidData`] for planes inconsistent with the layout.
    pub fn try_to_rgb8(&self) -> Result<Vec<u8>> {
        self.rgb_bytes(3, self.color)
    }

    /// Fallible, exact [`Self::to_rgba8`] (see [`Self::try_to_rgb8`]).
    pub fn try_to_rgba8(&self) -> Result<Vec<u8>> {
        self.rgb_bytes(4, self.color)
    }

    /// The exact conversion, else the BT.601 reading of a matrix the
    /// converter does not know.
    fn rgb_bytes_or_bt601(&self, channels: usize) -> Result<Vec<u8>> {
        match self.rgb_bytes(channels, self.color) {
            Err(Error::Unsupported(_)) => self.rgb_bytes(channels, self.color.with_matrix(6)),
            other => other,
        }
    }

    /// The container crate's planar frame for this picture (the shape
    /// its composition and colour layers work on).
    pub(crate) fn to_heif_frame(&self) -> Result<oxideav_heif::HeifFrame> {
        let frame = AvifFrame::new(None, self.planes.clone());
        crate::frame_bridge::to_heif(&frame, self.format, self.bit_depth, self.width, self.height)
    }

    /// `channels` = 3 (RGB) or 4 (RGBA), 8 bits per channel, converted
    /// with `color`.
    fn rgb_bytes(&self, channels: usize, color: ColorInfo) -> Result<Vec<u8>> {
        if !matches!(self.bit_depth, 8 | 10 | 12) {
            return Err(Error::invalid(format!(
                "avif: bit_depth {} is not 8, 10 or 12",
                self.bit_depth
            )));
        }
        let frame = self.to_heif_frame()?;
        let colr = color.to_heif_colr();
        let rgb = oxideav_heif::rgb::to_rgb(&frame, Some(&colr))?;
        let n = self.width as usize * self.height as usize;
        if rgb.channels < 3 || rgb.data.len() < n * rgb.channels {
            return Err(Error::invalid(
                "avif: RGB conversion returned fewer samples than the picture holds",
            ));
        }
        let depth = rgb.bit_depth;
        let max = (1u32 << depth) - 1;
        let half = max / 2;
        let to8 = |v: u16| -> u8 {
            if depth == 8 {
                v as u8
            } else {
                ((u32::from(v).min(max) * 255 + half) / max) as u8
            }
        };
        let mut out = Vec::with_capacity(n * channels);
        for px in rgb.data.chunks_exact(rgb.channels).take(n) {
            out.push(to8(px[0]));
            out.push(to8(px[1]));
            out.push(to8(px[2]));
            if channels == 4 {
                out.push(if rgb.channels >= 4 { to8(px[3]) } else { 255 });
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------
// Frames, header info, options
// ---------------------------------------------------------------------

/// One picture of a multi-picture file ([`crate::decode_all`]): a
/// sample of an `avis` image sequence (with its display `delay`) or
/// one entity of an image-burst group (no delay).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub image: AvifImage,
    /// How long the frame is shown before the next one (sequence
    /// samples); `None` for bursts and stills.
    pub delay: Option<Duration>,
}

impl Frame {
    /// Every field as a positional argument, in declaration order.
    pub fn new(image: AvifImage, delay: Option<Duration>) -> Self {
        Self { image, delay }
    }
}

/// What [`probe`] + a header parse say about a file, without decoding
/// pixels.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    /// Output extents of the primary item (`ispe` / derived canvas; a
    /// sequence's `tkhd` display size, else its track's sample entry).
    pub width: u32,
    pub height: u32,
    /// The layout a decode yields: depth and chroma from `av1C`, alpha
    /// from the attached auxiliary.
    pub format: PixelFormat,
    /// Pictures a [`crate::decode_all`] yields: samples of a sequence,
    /// entities of the primary's burst group, else 1.
    pub frames: usize,
    pub has_alpha: bool,
    /// The primary's effective colour (MIAF default when unsignalled).
    pub color: ColorInfo,
    pub has_icc: bool,
    pub has_exif: bool,
    pub has_xmp: bool,
    /// Coded sample depth from `av1C` (8 / 10 / 12).
    pub bit_depth: u8,
    /// `true` for an `avis` image sequence with a sample table.
    pub is_sequence: bool,
}

impl ImageInfo {
    /// Every field as a positional argument, in declaration order.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        format: PixelFormat,
        frames: usize,
        has_alpha: bool,
        color: ColorInfo,
        has_icc: bool,
        has_exif: bool,
        has_xmp: bool,
        bit_depth: u8,
        is_sequence: bool,
    ) -> Self {
        Self {
            width,
            height,
            format,
            frames,
            has_alpha,
            color,
            has_icc,
            has_exif,
            has_xmp,
            bit_depth,
            is_sequence,
        }
    }
}

/// Bounds and switches for [`crate::decode_with`]. The limits are
/// checked against the header before any pixel allocation and reported
/// as [`Error::LimitExceeded`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecodeOptions {
    /// Refuse a picture wider than this (default 16384, the largest
    /// AV1 coded extent this crate composes).
    pub max_width: u32,
    /// Refuse a picture taller than this (default 16384).
    pub max_height: u32,
    /// Refuse a picture — or any derived canvas on the way to it —
    /// with more pixels than this (default `1 << 28`).
    pub max_pixels: u64,
    /// Refuse an input longer than this many bytes (default: no bound).
    pub max_bytes: usize,
    /// Strict conformance: the `ftyp` must declare an AVIF brand
    /// (`avif` / `avis` / `avio`; a HEIF-structural-only brand set is
    /// refused) and a file claiming `mif1` must carry every HEIF
    /// §10.2.1.1 mandatory box. Default `false`: the lenient reading
    /// this crate has always applied.
    pub strict: bool,
    /// Apply the gain map of a `tmap` alternative (HEIF Amd 1
    /// §6.6.2.4, ISO 21496-1): the reconstructed HDR rendition in the
    /// `tmap` item's own colour instead of the base image. Default
    /// `false` — the base, what readers without tone-map support show.
    pub tone_mapped: bool,
    /// HDR reference white (cd/m²) for a PQ-coded reconstruction;
    /// default 203.
    pub reference_white_nits: f64,
    /// Render this spatial layer of a layered primary item instead of
    /// the file's `lsel` choice (av1-avif §2.3.2.2); `None` honours the
    /// file.
    pub layer: Option<u16>,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            max_width: 16_384,
            max_height: 16_384,
            max_pixels: 1 << 28,
            max_bytes: usize::MAX,
            strict: false,
            tone_mapped: false,
            reference_white_nits: oxideav_heif::gainmap::DEFAULT_HDR_REFERENCE_WHITE_NITS,
            layer: None,
        }
    }
}

impl DecodeOptions {
    /// The defaults (see the field docs).
    pub fn new() -> Self {
        Self::default()
    }
    /// Setter: replace `max_width`.
    pub fn with_max_width(mut self, max_width: u32) -> Self {
        self.max_width = max_width;
        self
    }
    /// Setter: replace `max_height`.
    pub fn with_max_height(mut self, max_height: u32) -> Self {
        self.max_height = max_height;
        self
    }
    /// Setter: replace `max_pixels`.
    pub fn with_max_pixels(mut self, max_pixels: u64) -> Self {
        self.max_pixels = max_pixels;
        self
    }
    /// Setter: replace `max_bytes`.
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }
    /// Setter: replace `strict`.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
    /// Setter: replace `tone_mapped`.
    pub fn with_tone_mapped(mut self, tone_mapped: bool) -> Self {
        self.tone_mapped = tone_mapped;
        self
    }
    /// Setter: replace `reference_white_nits`.
    pub fn with_reference_white_nits(mut self, reference_white_nits: f64) -> Self {
        self.reference_white_nits = reference_white_nits;
        self
    }
    /// Setter: replace `layer`.
    pub fn with_layer(mut self, layer: Option<u16>) -> Self {
        self.layer = layer;
        self
    }

    /// Check an input length against `max_bytes`
    /// ([`Error::LimitExceeded`] when over).
    pub fn check_bytes(&self, len: usize) -> Result<()> {
        if len > self.max_bytes {
            return Err(Error::limit(format!(
                "avif: input is {len} bytes, more than the {} allowed",
                self.max_bytes
            )));
        }
        Ok(())
    }

    /// Check a picture's extents against `max_width` / `max_height` /
    /// `max_pixels` ([`Error::LimitExceeded`] when over).
    pub fn check_dims(&self, width: u32, height: u32) -> Result<()> {
        if width > self.max_width || height > self.max_height {
            return Err(Error::limit(format!(
                "avif: picture is {width}x{height}, more than the {}x{} allowed",
                self.max_width, self.max_height
            )));
        }
        let pixels = u64::from(width) * u64::from(height);
        if pixels > self.max_pixels {
            return Err(Error::limit(format!(
                "avif: picture holds {pixels} pixels, more than the {} allowed",
                self.max_pixels
            )));
        }
        Ok(())
    }
}

/// Chroma layout of an encode ([`crate::still::StillImage`] and the
/// RGB one-call paths). Mirrors the AV1 §6.4.2 subsampling pairings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StillChroma {
    /// Chroma at half extent on both axes (`subsampling_x =
    /// subsampling_y = 1`). Requires even luma extents.
    #[default]
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

#[cfg_attr(not(feature = "registry"), allow(dead_code))]
impl StillChroma {
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

/// Tuning for the encoders ([`crate::encode`], [`crate::encode_rgb8`],
/// [`crate::encode_rgba8`], [`crate::still::encode_still`] …). The
/// default is a lossless colour encode (`base_q_idx = 0`), lossless
/// alpha, straight (non-premultiplied) alpha signalling, 4:2:0 for the
/// RGB one-call paths, every metadata block embedded.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeOptions {
    /// AV1 `base_q_idx` for the colour planes. `0` = lossless
    /// (default); higher = lossier / smaller (255 = coarsest). See
    /// [`Self::with_quality`] for the 0–100 scale.
    pub base_q_idx: u8,
    /// AV1 `base_q_idx` for the alpha auxiliary. Defaults to `0`
    /// (lossless) — alpha artefacts are far more visible than colour
    /// ones, and flat alpha planes are cheap.
    pub alpha_q_idx: u8,
    /// Emit the `prem` iref declaring the colour planes premultiplied
    /// by alpha (HEIF §6.10.1.1). Signalling only — the samples are
    /// stored as given.
    pub premultiplied_alpha: bool,
    /// Chroma layout of [`crate::encode_rgb8`] / [`crate::encode_rgba8`]
    /// (default 4:2:0; odd extents fall back to 4:4:4, see those
    /// functions). [`crate::encode`] keeps the image's own layout.
    pub chroma: StillChroma,
    /// Embed `metadata.exif` as an `Exif` item (default `true`).
    pub embed_exif: bool,
    /// Embed `metadata.xmp` as a `mime` item (default `true`).
    pub embed_xmp: bool,
    /// Embed `metadata.icc` as an ICC `colr` next to the `nclx` one
    /// (default `true`).
    pub embed_icc: bool,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            base_q_idx: 0,
            alpha_q_idx: 0,
            premultiplied_alpha: false,
            chroma: StillChroma::Yuv420,
            embed_exif: true,
            embed_xmp: true,
            embed_icc: true,
        }
    }
}

impl EncodeOptions {
    /// The historical three-field constructor (`base_q_idx`,
    /// `alpha_q_idx`, `premultiplied_alpha`); the other fields take
    /// their defaults.
    pub fn new(base_q_idx: u8, alpha_q_idx: u8, premultiplied_alpha: bool) -> Self {
        Self {
            base_q_idx,
            alpha_q_idx,
            premultiplied_alpha,
            ..Self::default()
        }
    }
    /// Setter: replace `base_q_idx`.
    pub fn with_base_q_idx(mut self, base_q_idx: u8) -> Self {
        self.base_q_idx = base_q_idx;
        self
    }
    /// Setter: replace `alpha_q_idx`.
    pub fn with_alpha_q_idx(mut self, alpha_q_idx: u8) -> Self {
        self.alpha_q_idx = alpha_q_idx;
        self
    }
    /// Setter: replace `premultiplied_alpha`.
    pub fn with_premultiplied_alpha(mut self, premultiplied_alpha: bool) -> Self {
        self.premultiplied_alpha = premultiplied_alpha;
        self
    }
    /// Setter: replace `chroma`.
    pub fn with_chroma(mut self, chroma: StillChroma) -> Self {
        self.chroma = chroma;
        self
    }
    /// Setter: replace `embed_exif`.
    pub fn with_embed_exif(mut self, embed_exif: bool) -> Self {
        self.embed_exif = embed_exif;
        self
    }
    /// Setter: replace `embed_xmp`.
    pub fn with_embed_xmp(mut self, embed_xmp: bool) -> Self {
        self.embed_xmp = embed_xmp;
        self
    }
    /// Setter: replace `embed_icc`.
    pub fn with_embed_icc(mut self, embed_icc: bool) -> Self {
        self.embed_icc = embed_icc;
        self
    }

    /// Colour quality on a 0–100 scale (100 = lossless): sets
    /// `base_q_idx = ((100 − quality) × 255 + 50) / 100`, so 100 → 0,
    /// 90 → 26, 75 → 64, 50 → 128, 0 → 255. Values above 100 clamp to
    /// 100. Alpha stays at its own `alpha_q_idx`.
    pub fn with_quality(mut self, quality: u8) -> Self {
        let q = u32::from(quality.min(100));
        self.base_q_idx = (((100 - q) * 255 + 50) / 100) as u8;
        self
    }
}

// ---------------------------------------------------------------------
// probe / info (standalone)
// ---------------------------------------------------------------------

const BRAND_AVIF: [u8; 4] = *b"avif";
const BRAND_AVIS: [u8; 4] = *b"avis";
const BRAND_AVIO: [u8; 4] = *b"avio";

/// `true` when `bytes` start with an ISOBMFF `ftyp` box whose major or
/// compatible brands include an AVIF brand (`avif` / `avis` / `avio`,
/// av1-avif §6). No allocation, never panics, `false` on short input
/// or on a HEIF file that is not AVIF.
pub fn probe(bytes: &[u8]) -> bool {
    if bytes.len() < 16 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let size32 = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let (size, brands_start) = match size32 {
        0 => (bytes.len(), 16),
        1 => {
            if bytes.len() < 24 {
                return false;
            }
            let large = u64::from_be_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ]);
            (usize::try_from(large).unwrap_or(usize::MAX), 24)
        }
        n => (n, 16),
    };
    let major_at = brands_start - 8;
    if size < brands_start {
        return false;
    }
    let end = size.min(bytes.len());
    let is_avif = |b: &[u8]| b == BRAND_AVIF || b == BRAND_AVIS || b == BRAND_AVIO;
    if is_avif(&bytes[major_at..major_at + 4]) {
        return true;
    }
    bytes[brands_start..end].chunks_exact(4).any(is_avif)
}

/// Header-only description of an AVIF file: extents, the layout a
/// decode yields, the picture count, alpha, colour and metadata
/// presence. Parses `ftyp` + `meta` (and the sample table of an
/// `avis` sequence), never a coded payload.
pub fn info(bytes: &[u8]) -> Result<ImageInfo> {
    let (ftyp_payload, _) = crate::box_parser::find_box(bytes, b"ftyp")?
        .ok_or_else(|| Error::invalid("avif: missing ftyp"))?;
    let (major, _minor, compat) = crate::parser::parse_ftyp(ftyp_payload)?;
    let brands = crate::parser::classify_brands(&major, &compat)?;
    if (brands.is_sequence || brands.has_msf1) && has_moov(bytes) {
        return sequence_info(bytes);
    }
    still_info(bytes)
}

pub(crate) fn has_moov(bytes: &[u8]) -> bool {
    crate::box_parser::find_box(bytes, b"moov")
        .ok()
        .flatten()
        .is_some()
}

/// The `nclx` and ICC `colr` properties of an item, wherever they sit
/// in its association list (an item may carry one of each — MIAF
/// §7.3.6.4).
pub(crate) fn item_colrs(
    meta: &crate::meta::Meta,
    item_id: u32,
) -> (Option<Colr>, Option<Vec<u8>>) {
    let mut nclx = None;
    let mut icc = None;
    for prop in meta.properties_for(item_id) {
        if let crate::meta::Property::Colr(c) = prop {
            match c {
                Colr::Nclx { .. } if nclx.is_none() => nclx = Some(c.clone()),
                Colr::Icc(bytes) if icc.is_none() => icc = Some(bytes.clone()),
                _ => {}
            }
        }
    }
    (nclx, icc)
}

/// The layout a decode of the primary yields, from the inspected
/// `av1C` flags plus alpha presence.
pub(crate) fn layout_of_info(
    bit_depth: u8,
    monochrome: bool,
    chroma: Option<(bool, bool)>,
    has_alpha: bool,
) -> Result<PixelFormat> {
    let (sx, sy) = match chroma {
        Some((x, y)) => (u8::from(x), u8::from(y)),
        None => (1, 1),
    };
    PixelFormat::from_layout(bit_depth, sx, sy, monochrome, has_alpha).ok_or_else(|| {
        Error::unsupported(format!(
            "avif: no pixel layout for {bit_depth}-bit subsampling ({sx}, {sy}) \
             monochrome={monochrome} alpha={has_alpha}"
        ))
    })
}

/// The entities of a `brst` image-burst group (HEIF §6.8.9) that
/// contains `item_id`, in group order; empty when there is none.
pub(crate) fn burst_members(meta: &crate::meta::Meta, item_id: u32) -> Vec<u32> {
    meta.groups()
        .unwrap_or_default()
        .into_iter()
        .find(|g| g.is_burst() && g.entity_ids.contains(&item_id))
        .map(|g| g.entity_ids)
        .unwrap_or_default()
}

fn still_info(bytes: &[u8]) -> Result<ImageInfo> {
    let hdr = crate::parser::parse_header(bytes)?;
    let primary_id = hdr
        .meta
        .primary_item_id
        .ok_or_else(|| Error::invalid("avif: missing pitm"))?;
    let info = crate::inspect::inspect(bytes)?;
    let bit_depth = info.bit_depth.unwrap_or(8);
    let format = layout_of_info(
        bit_depth,
        info.monochrome,
        info.chroma_subsampling,
        info.has_alpha,
    )?;
    let (nclx, icc) = item_colrs(&hdr.meta, primary_id);
    // A derived primary without its own `colr` inherits its first
    // input's (what `inspect` resolved).
    let colour = nclx.or_else(|| match &info.colour {
        Some(c @ Colr::Nclx { .. }) => Some(c.clone()),
        _ => None,
    });
    let has_icc = icc.is_some() || matches!(info.colour, Some(Colr::Icc(_)));
    let burst = burst_members(&hdr.meta, primary_id);
    let frames = if burst.is_empty() { 1 } else { burst.len() };
    Ok(ImageInfo::new(
        info.width,
        info.height,
        format,
        frames,
        info.has_alpha,
        ColorInfo::from_colr(colour.as_ref()),
        has_icc,
        info.exif_item_id.is_some(),
        info.xmp_item_id.is_some(),
        bit_depth,
        false,
    ))
}

fn sequence_info(bytes: &[u8]) -> Result<ImageInfo> {
    let meta = crate::avis::parse_avis(bytes)?;
    let av1c = meta.av1_codec_config.as_deref().ok_or_else(|| {
        Error::invalid("avis: track stsd → av01 → av1C is missing (av1-avif §2.2.1)")
    })?;
    let cfg = oxideav_heif::Av1Config::parse(av1c)?;
    let bit_depth = cfg.bit_depth();
    let format = layout_of_info(
        bit_depth,
        cfg.monochrome,
        Some((cfg.chroma_subsampling_x, cfg.chroma_subsampling_y)),
        false,
    )?;
    let (width, height) = meta.display_dims.unwrap_or((0, 0));
    let (has_exif, has_xmp, has_icc) = match crate::parser::parse_header(bytes) {
        Ok(hdr) => {
            // The cover still of the sequence (av1-avif §6.3) may carry
            // the metadata items.
            let primary = hdr.meta.primary_item_id;
            match primary.and_then(|_| crate::inspect::inspect(bytes).ok()) {
                Some(i) => (
                    i.exif_item_id.is_some(),
                    i.xmp_item_id.is_some(),
                    matches!(i.colour, Some(Colr::Icc(_)))
                        || primary
                            .map(|p| item_colrs(&hdr.meta, p).1.is_some())
                            .unwrap_or(false),
                ),
                None => (false, false, false),
            }
        }
        Err(_) => (false, false, false),
    };
    Ok(ImageInfo::new(
        width,
        height,
        format,
        meta.samples.len(),
        false,
        ColorInfo::from_colr(meta.colr.as_ref()),
        has_icc,
        has_exif,
        has_xmp,
        bit_depth,
        true,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        let mut v = Vec::new();
        let size = 16 + 4 * compat.len();
        v.extend_from_slice(&(size as u32).to_be_bytes());
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(major);
        v.extend_from_slice(&[0, 0, 0, 0]);
        for c in compat {
            v.extend_from_slice(*c);
        }
        v
    }

    #[test]
    fn probe_accepts_avif_brands_only() {
        assert!(probe(&ftyp(b"avif", &[b"mif1", b"miaf"])));
        assert!(probe(&ftyp(b"mif1", &[b"avif"])));
        assert!(probe(&ftyp(b"avis", &[b"msf1"])));
        assert!(probe(&ftyp(b"avio", &[])));
        assert!(!probe(&ftyp(b"heic", &[b"mif1", b"miaf"])));
        assert!(!probe(&ftyp(b"mif1", &[b"heic"])));
        assert!(!probe(b""));
        assert!(!probe(b"\0\0\0\x10ftypavif"));
        assert!(!probe(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0"));
        // A truncated compatible-brand list is scanned as far as it goes.
        let mut t = ftyp(b"mif1", &[b"miaf", b"avif"]);
        t.truncate(22);
        assert!(!probe(&t));
        // Largesize header.
        let mut large = Vec::new();
        large.extend_from_slice(&1u32.to_be_bytes());
        large.extend_from_slice(b"ftyp");
        large.extend_from_slice(&28u64.to_be_bytes());
        large.extend_from_slice(b"avif");
        large.extend_from_slice(&[0, 0, 0, 0]);
        large.extend_from_slice(b"mif1");
        assert!(probe(&large));
    }

    #[test]
    fn rgb_round_trips_through_identity_planes() {
        let rgb: Vec<u8> = (0..3 * 4 * 2).map(|i| (i * 7 % 256) as u8).collect();
        let img = AvifImage::from_rgb8(4, 2, rgb.clone());
        assert_eq!(img.format(), PixelFormat::Yuv444P);
        assert!(img.color.is_identity_matrix());
        assert_eq!(img.to_rgb8(), rgb);
        let rgba = img.to_rgba8();
        assert_eq!(rgba.len(), 4 * 2 * 4);
        assert!(rgba.chunks_exact(4).all(|p| p[3] == 255));
        let with_alpha: Vec<u8> = (0..4 * 4 * 2).map(|i| (i * 13 % 256) as u8).collect();
        let img = AvifImage::from_rgba8(4, 2, with_alpha.clone());
        assert_eq!(img.format(), PixelFormat::Yuva444P);
        assert_eq!(img.to_rgba8(), with_alpha);
        assert_eq!(
            img.to_rgb8(),
            with_alpha
                .chunks_exact(4)
                .flat_map(|p| p[..3].to_vec())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn limited_range_luma_maps_to_rgb_extremes() {
        // BT.709 limited: Y = 16 → 0, Y = 235 → 255 with neutral chroma.
        let planes = vec![
            Plane::new(2, vec![16, 235, 128, 128]),
            Plane::new(1, vec![128]),
            Plane::new(1, vec![128]),
        ];
        let img = AvifImage::new(2, 2, PixelFormat::Yuv420P, planes)
            .with_color(ColorInfo::new(false, 1, 1, 1));
        let rgb = img.to_rgb8();
        assert_eq!(&rgb[0..3], &[0, 0, 0]);
        assert_eq!(&rgb[3..6], &[255, 255, 255]);
        // Y = 128 limited → (128 − 16) × 255 / 219 = 130.4 → 130.
        assert_eq!(&rgb[6..9], &[130, 130, 130]);
        // The same samples full range: 16 and 235 stay.
        let full = img.clone().with_color(ColorInfo::new(true, 1, 1, 1));
        let rgb = full.to_rgb8();
        assert_eq!(&rgb[0..3], &[16, 16, 16]);
        assert_eq!(&rgb[3..6], &[235, 235, 235]);
    }

    #[test]
    fn deep_samples_scale_to_eight_bits_by_rounding() {
        // 10-bit gray: 0 → 0, 1023 → 255, 512 → round(512*255/1023) = 128.
        let words: Vec<u8> = [0u16, 1023, 512, 2]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let img = AvifImage::new(4, 1, PixelFormat::Gray10Le, vec![Plane::new(8, words)]);
        let rgb = img.to_rgb8();
        assert_eq!(rgb[0], 0);
        assert_eq!(rgb[3], 255);
        assert_eq!(rgb[6], 128);
        assert_eq!(rgb[9], 0); // 2 * 255 / 1023 = 0.498 → 0
        assert_eq!(img.as_bytes().map(|b| b.len()), Some(8));
    }

    #[test]
    fn unknown_matrix_falls_back_to_bt601_in_the_infallible_view() {
        let planes = vec![
            Plane::new(2, vec![16, 235, 128, 128]),
            Plane::new(1, vec![128]),
            Plane::new(1, vec![128]),
        ];
        let img = AvifImage::new(2, 2, PixelFormat::Yuv420P, planes)
            .with_color(ColorInfo::new(false, 1, 1, 131));
        assert!(matches!(img.try_to_rgb8(), Err(Error::Unsupported(_))));
        let bt601 = img.clone().with_color(ColorInfo::new(false, 1, 1, 6));
        assert_eq!(img.to_rgb8(), bt601.to_rgb8());
        assert_eq!(img.to_rgba8(), bt601.to_rgba8());
    }

    #[test]
    fn packed_ya_carries_alpha() {
        let img = AvifImage::new(
            2,
            1,
            PixelFormat::Ya8,
            vec![Plane::new(4, vec![200, 10, 50, 255])],
        );
        let rgba = img.to_rgba8();
        assert_eq!(rgba, vec![200, 200, 200, 10, 50, 50, 50, 255]);
        assert!(img.has_alpha());
    }

    #[test]
    fn inconsistent_planes_are_an_error_not_a_panic() {
        let img = AvifImage::new(4, 4, PixelFormat::Yuv420P, vec![Plane::new(4, vec![0; 4])]);
        assert!(img.try_to_rgb8().is_err());
        assert!(img.as_bytes().is_some());
        assert_eq!(img.into_raw().len(), 4);
    }

    #[test]
    fn quality_scale_maps_to_q_idx() {
        assert_eq!(EncodeOptions::default().with_quality(100).base_q_idx, 0);
        assert_eq!(EncodeOptions::default().with_quality(90).base_q_idx, 26);
        assert_eq!(EncodeOptions::default().with_quality(50).base_q_idx, 128);
        assert_eq!(EncodeOptions::default().with_quality(0).base_q_idx, 255);
        assert_eq!(EncodeOptions::default().with_quality(200).base_q_idx, 0);
        let o = EncodeOptions::new(3, 4, true);
        assert_eq!(
            (o.base_q_idx, o.alpha_q_idx, o.premultiplied_alpha),
            (3, 4, true)
        );
        assert_eq!(o.chroma, StillChroma::Yuv420);
        assert!(o.embed_exif && o.embed_xmp && o.embed_icc);
    }

    #[test]
    fn exif_offset_word_is_stripped_and_rewritten() {
        let body = [0, 0, 0, 2, 0xAA, 0xBB, b'M', b'M', 0, 42];
        assert_eq!(exif_tiff_bytes(&body), Some(vec![b'M', b'M', 0, 42]));
        assert_eq!(exif_tiff_bytes(&[0, 0, 0, 9, 1]), None);
        assert_eq!(exif_tiff_bytes(&[0, 0]), None);
        assert_eq!(exif_item_body(b"II"), vec![0, 0, 0, 0, b'I', b'I']);
    }

    #[test]
    fn decode_limits_are_reported_as_limit_exceeded() {
        let o = DecodeOptions::default()
            .with_max_width(8)
            .with_max_pixels(10);
        assert!(matches!(o.check_dims(9, 1), Err(Error::LimitExceeded(_))));
        assert!(matches!(o.check_dims(4, 4), Err(Error::LimitExceeded(_))));
        assert!(o.check_dims(5, 2).is_ok());
        assert!(matches!(
            DecodeOptions::default().with_max_bytes(3).check_bytes(4),
            Err(Error::LimitExceeded(_))
        ));
    }

    #[test]
    fn info_describes_fixture_headers() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/yuv420_10bit_full.avif"
        ))
        .unwrap();
        assert!(probe(&bytes));
        let i = info(&bytes).unwrap();
        assert_eq!(i.format, PixelFormat::Yuv420P10Le);
        assert_eq!(i.bit_depth, 10);
        assert_eq!(i.frames, 1);
        assert!(!i.is_sequence);
        assert!(i.color.full_range);
        assert!(i.width > 0 && i.height > 0);

        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/alpha_video.avif"
        ))
        .unwrap();
        let i = info(&bytes).unwrap();
        assert!(i.is_sequence);
        assert!(i.frames > 1);

        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/monochrome.avif"
        ))
        .unwrap();
        let i = info(&bytes).unwrap();
        assert_eq!(i.format, PixelFormat::Gray8);
        assert!(!i.has_alpha);
        assert!(info(b"not an avif file at all, just bytes").is_err());
    }
}
