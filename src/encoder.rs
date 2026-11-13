//! AVIF `Encoder` trait wiring — registry-gated.
//!
//! Each frame sent through [`Encoder::send_frame`] encodes into one
//! **complete AVIF file** (AVIF is a still-image container: the
//! packet boundary is the file boundary), produced by the
//! [`crate::still`] pipeline — AV1 KEY-frame encode via `oxideav_av1`
//! plus this crate's container muxer. [`Encoder::receive_packet`]
//! hands the file bytes back as a data packet carrying the frame's
//! `pts`.
//!
//! # Input mapping
//!
//! [`CodecParameters`] must declare `width`, `height` and
//! `pixel_format`. Supported formats:
//!
//! * Planar YUV, 8-bit: `Yuv420P` / `Yuv422P` / `Yuv444P` / `Gray8`,
//!   plus the full-range `YuvJ420P` / `YuvJ422P` / `YuvJ444P`
//!   variants (same plane layout; a full-range `colr` `nclx` is
//!   attached and the coded stream signals §5.5.2 `color_range = 1`).
//! * Planar YUV, high bit depth: `Yuv420P10Le` / `Yuv422P10Le` /
//!   `Yuv444P10Le` / `Yuv420P12Le` / `Yuv422P12Le` / `Yuv444P12Le` /
//!   `Gray10Le` / `Gray12Le` (little-endian 2-byte samples).
//! * Packed RGB(A): `Rgb24` / `Rgba` / `Bgr24` / `Bgra` — at the
//!   lossless quality coded via the H.273 identity matrix in 4:4:4
//!   (byte-exact; see [`crate::still::StillImage::rgb8`]); at a lossy
//!   quality converted to Y′CbCr (the signalled matrix, else the MIAF
//!   default BT.601, at the signalled range, else full) and coded in
//!   the `chroma` layout (4:2:0 by default, 4:4:4 at odd extents) —
//!   the coding a lossy RGB picture is expected to land on (the
//!   identity matrix spends bits on the un-decorrelated planes). The
//!   alpha channel becomes an alpha auxiliary item either way.
//! * Planar RGB(A): `Gbrp8` / `Gbrap8` / `Gbrp10Le` / `Gbrap10Le` /
//!   `Gbrp12Le` / `Gbrap12Le` (planes G, B, R [, A] — the layout the
//!   decoder labels an identity-matrix item with): coded as an
//!   identity-matrix 4:4:4 item at the input depth, alpha as the
//!   auxiliary.
//! * Alpha-carrying planar: `Yuva420P` (four planes, full-resolution
//!   alpha) and `Ya8` (interleaved luma + alpha).
//!
//! # Colour signal
//!
//! [`CodecParameters::color_signal`] (oxideav-core 0.1.37) decides the
//! `colr` `nclx` the item is written with: a specified range writes
//! `full_range_flag` (and codes the AV1 `color_range` bit to match),
//! specified primaries / transfer / matrix code points are written as
//! given. The `YuvJ*` labels imply full range and the RGB layouts the
//! identity matrix (a signalled matrix cannot override it — the planes
//! *are* G B R); with nothing signalled the item keeps this crate's
//! historical defaults (no `colr` for Y′CbCr input, `1 / 13 / 0` full
//! range for RGB). An item that carries primaries / transfer but no
//! range is written limited (H.273's inferred `VideoFullRangeFlag`).
//! The encoder's [`Encoder::output_params`] carry the signal the file
//! was written with.
//!
//! The encoder's [`Encoder::output_params`] carry the **coded**
//! label after a frame — what the file decodes back as — rather than
//! the input label: packed RGB(A) codes as `Gbrp8` / `Gbrap8`
//! losslessly and as Y′CbCr at a lossy quality; an 8-bit `Yuv*` label
//! (no implied range) is written without `colr`, which MIAF reads as
//! full range, so it decodes back as `YuvJ*` — the samples are stored
//! as given either way; set `color_signal.range = Limited` to write a
//! limited-range `colr` instead.
//!
//! # Metadata
//!
//! The ICC profile, Exif block and XMP packet a picture carries as
//! oxideav-core 0.1.40 metadata blobs are written as the file's
//! metadata: [`CodecParameters::blobs`] (stream-level) refined by the
//! frame's own blobs record (a frame blob overrides a stream blob of
//! the same kind — [`crate::Metadata::from_refined_blobs`]).
//! `BlobKind::ICC` becomes the `prof` `colr` property next to the
//! `nclx` one, `BlobKind::EXIF` (the TIFF-structured block from its
//! byte-order mark) an `Exif` item whose 4-byte offset header this
//! encoder writes, `BlobKind::XMP` a `mime` item. Other kinds have no
//! AVIF carriage and are dropped; the decoder puts the same three
//! kinds back on its frames, so `decode(encode(frame))` carries the
//! metadata round. There is no "embed" switch on this path: a caller
//! that wants a blob withheld strips it from the parameters / frame
//! (the gateway's strip rules).
//!
//! The accepted layouts are published as [`ENCODER_PIXEL_FORMATS`]
//! (the registry's `CodecCapabilities::accepted_pixel_formats`, the
//! set a gateway's conversion ladder reads); any other label is a
//! typed `Unsupported` error naming it.
//!
//! # Options
//!
//! Codec options (string map on [`CodecParameters::options`]; the
//! schema is [`AvifEncoderOptions`], declared on the registry entry):
//!
//! * `quality` — `0..=100`, `100` = lossless; absent =
//!   [`DEFAULT_QUALITY`] (60, the production default — see the
//!   README's quality table). When `q` is not given, `base_q_idx =
//!   ((100 − quality) × 255 + 50) / 100` (the
//!   [`EncodeOptions::with_quality`] mapping: 100 → 0, 90 → 26,
//!   75 → 64, 60 → 102, 50 → 128, 10 → 230, 0 → 255); the alpha
//!   auxiliary follows the same mapping when `alpha_q` is not given
//!   and `quality` is — with neither, alpha is coded losslessly
//!   (coverage artefacts are far more visible than colour ones and a
//!   flat alpha plane is cheap).
//! * `q` — AV1 `base_q_idx` for the colour planes, `0..=255`
//!   (`0` = lossless). An explicit `q` wins over `quality`.
//! * `alpha_q` — `base_q_idx` for the alpha auxiliary; wins over
//!   `quality` for that plane.
//! * `chroma` — `420` (default) / `422` / `444`: the Y′CbCr layout a
//!   packed RGB(A) / BGR(A) frame is coded in at a lossy quality (odd
//!   extents promote to 4:4:4). Planar frames keep their own layout.
//! * `premultiplied` — `true` to emit the `prem` iref declaring
//!   premultiplied alpha (default `false`).
//! * `speed` — `fast` (default) / `balanced` / `thorough`: the AV1
//!   search effort ([`EncodeSpeed`]).
//! * `threads` — worker threads for the AV1 tile search; `0` (default)
//!   follows the budget of [`Encoder::set_execution_context`] (serial
//!   until one is granted). The tile layout is derived from the
//!   picture size alone, so the bytes never depend on the budget.
//! * `grid` — `auto` (default) / `none` / a tile edge in pixels
//!   (`64..=4096`): how the picture splits into coded items
//!   ([`GridTiling`]). `auto` codes one item up to
//!   [`crate::still::AUTO_GRID_MAX_SINGLE_PIXELS`] luma samples and a
//!   `grid` of [`crate::still::AUTO_GRID_TILE`]-pixel tiles above it
//!   (the tiles coded on the budget's workers, each with a bounded
//!   working set); `none` is one item whenever the coded extents fit
//!   [`crate::still::STILL_MAX_CODED_DIM`]; a tile edge forces a grid
//!   of tiles at most that size. The decoded pixels are the same on
//!   every shape.
//!
//! * `lossless` — `true` for a mathematically lossless file: the
//!   colour planes and the alpha auxiliary are coded with the AV1
//!   lossless arm whatever `quality` / `q` / `alpha_q` say, and RGB
//!   input stays identity-matrix 4:4:4 RGB (`chroma` does not apply).
//!   The decoded frame equals the input sample for sample.
//!
//! With neither `quality` nor `q` the colour planes code at
//! [`DEFAULT_QUALITY`]; `lossless=true`, `quality=100` or `q=0` is the
//! lossless path (`q=0` alone leaves the alpha auxiliary on
//! `alpha_q` / `quality`).

use oxideav_core::frame::VideoFrame;
use oxideav_core::{
    CodecId, CodecOptionsStruct, CodecParameters, ColorRange, ColorSignal, Encoder, Error, Frame,
    OptionField, OptionKind, OptionValue, Packet, PixelFormat, Result, TimeBase, parse_options,
};

use crate::api::{ColorInfo, EncodeOptions, EncodeSpeed, GridTiling, Metadata, exif_item_body};
use crate::meta::Colr;
use crate::source::{LossyRgb, PictureView, PlaneRef, TileSource};
use crate::still::{
    StillChroma, StillImage, StillProperties, encode_source_auto, identity_full_range_colr,
};

/// Every input layout the encoder accepts (the module docs' list) —
/// the registry's `accepted_pixel_formats`. A frame in any other
/// layout is refused with a typed `Unsupported` error naming it.
pub const ENCODER_PIXEL_FORMATS: &[PixelFormat] = &[
    // Planar Y′CbCr, 8-bit (+ the full-range J labels) and grey.
    PixelFormat::Yuv420P,
    PixelFormat::Yuv422P,
    PixelFormat::Yuv444P,
    PixelFormat::YuvJ420P,
    PixelFormat::YuvJ422P,
    PixelFormat::YuvJ444P,
    PixelFormat::Gray8,
    // Planar Y′CbCr / grey, 10- and 12-bit little-endian words.
    PixelFormat::Yuv420P10Le,
    PixelFormat::Yuv422P10Le,
    PixelFormat::Yuv444P10Le,
    PixelFormat::Yuv420P12Le,
    PixelFormat::Yuv422P12Le,
    PixelFormat::Yuv444P12Le,
    PixelFormat::Gray10Le,
    PixelFormat::Gray12Le,
    // Packed RGB(A) / BGR(A).
    PixelFormat::Rgb24,
    PixelFormat::Rgba,
    PixelFormat::Bgr24,
    PixelFormat::Bgra,
    // Planar RGB(A) — the identity-matrix 4:4:4 coding at 8 / 10 / 12 bits.
    PixelFormat::Gbrp8,
    PixelFormat::Gbrap8,
    PixelFormat::Gbrp10Le,
    PixelFormat::Gbrap10Le,
    PixelFormat::Gbrp12Le,
    PixelFormat::Gbrap12Le,
    // Alpha-carrying planar / interleaved grey + alpha.
    PixelFormat::Yuva420P,
    PixelFormat::Ya8,
];

/// The `quality` → `base_q_idx` mapping of the module docs
/// (`((100 − quality) × 255 + 50) / 100`; qualities above 100 clamp).
pub fn quality_to_base_q_idx(quality: u8) -> u8 {
    EncodeOptions::default().with_quality(quality).base_q_idx
}

/// The colour quality the registry encoder codes at when the caller
/// sets neither `quality` nor `q`: `base_q_idx` 102. On the 12 MP
/// device sample (8-bit 4:2:0) it writes 2.04 MB at 35.8 dB luma —
/// the size the picture's own camera HEIC has — where 50 is 1.48 MB /
/// 33.5 dB, 70 is 2.62 MB / 38.2 dB, 90 is 4.81 MB / 44.8 dB and the
/// lossless coding 12.0 MB; it is also the `quality` the HEIF
/// container's own AV1 items default to, so one dial means one thing
/// across the family.
pub const DEFAULT_QUALITY: u8 = 60;

/// The registry encoder's option schema (see the module docs): the
/// typed form of the `quality` / `q` / `alpha_q` / `chroma` /
/// `premultiplied` codec options, parsed strictly at construction
/// through [`oxideav_core::parse_options`] (an unknown key or a
/// malformed value is `InvalidData`).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvifEncoderOptions {
    /// `quality` 0..=100 (100 = lossless); `None` when not given.
    pub quality: Option<u8>,
    /// Explicit colour `base_q_idx` (`q`); wins over `quality`.
    pub q: Option<u8>,
    /// Explicit alpha `base_q_idx` (`alpha_q`); wins over `quality`.
    pub alpha_q: Option<u8>,
    /// Chroma layout for a lossy packed-RGB encode (`chroma`).
    pub chroma: StillChroma,
    /// Emit the `prem` iref (`premultiplied`).
    pub premultiplied: bool,
    /// AV1 search effort (`speed`).
    pub speed: EncodeSpeed,
    /// Explicit worker threads (`threads`); `None` = the execution
    /// context's budget.
    pub threads: Option<usize>,
    /// How the picture splits into coded items (`grid`).
    pub grid: GridTiling,
    /// Mathematically lossless coding (`lossless`): colour and alpha
    /// coded with the AV1 lossless arm, packed / planar RGB kept as
    /// identity-matrix 4:4:4; wins over `quality`, `q`, `alpha_q` and
    /// `chroma`.
    pub lossless: bool,
}

impl Default for AvifEncoderOptions {
    fn default() -> Self {
        Self {
            quality: None,
            q: None,
            alpha_q: None,
            chroma: StillChroma::Yuv420,
            premultiplied: false,
            speed: EncodeSpeed::Fast,
            threads: None,
            grid: GridTiling::Auto,
            lossless: false,
        }
    }
}

impl AvifEncoderOptions {
    /// The defaults ([`DEFAULT_QUALITY`] colour, lossless alpha, 4:2:0
    /// for lossy RGB, straight alpha, `fast`, the budget's threads,
    /// `grid=auto`).
    pub fn new() -> Self {
        Self::default()
    }
    /// Setter: replace `quality`.
    pub fn with_quality(mut self, quality: Option<u8>) -> Self {
        self.quality = quality;
        self
    }
    /// Setter: replace `q`.
    pub fn with_q(mut self, q: Option<u8>) -> Self {
        self.q = q;
        self
    }
    /// Setter: replace `alpha_q`.
    pub fn with_alpha_q(mut self, alpha_q: Option<u8>) -> Self {
        self.alpha_q = alpha_q;
        self
    }
    /// Setter: replace `chroma`.
    pub fn with_chroma(mut self, chroma: StillChroma) -> Self {
        self.chroma = chroma;
        self
    }
    /// Setter: replace `premultiplied`.
    pub fn with_premultiplied(mut self, premultiplied: bool) -> Self {
        self.premultiplied = premultiplied;
        self
    }
    /// Setter: replace `speed`.
    pub fn with_speed(mut self, speed: EncodeSpeed) -> Self {
        self.speed = speed;
        self
    }
    /// Setter: replace `threads` (`None` = the execution context's budget).
    pub fn with_threads(mut self, threads: Option<usize>) -> Self {
        self.threads = threads;
        self
    }
    /// Setter: replace `grid`.
    pub fn with_grid(mut self, grid: GridTiling) -> Self {
        self.grid = grid;
        self
    }

    /// Setter: replace `lossless`.
    pub fn with_lossless(mut self, lossless: bool) -> Self {
        self.lossless = lossless;
        self
    }

    /// Strict parse of a parameter set's option bag.
    pub fn from_params(params: &CodecParameters) -> Result<Self> {
        parse_options::<Self>(&params.options)
    }

    /// Lenient parse: unknown keys and malformed values are ignored
    /// (the historical [`AvifEncoder::with_params`] behaviour).
    fn from_params_lenient(params: &CodecParameters) -> Self {
        let mut out = Self::default();
        for (k, v) in params.options.iter() {
            let one = oxideav_core::CodecOptions::new().set(k, v);
            if let Ok(parsed) = parse_options::<Self>(&one)
                && let Err(e) = parsed.apply_into(&mut out, k)
            {
                debug_assert!(false, "lenient option apply: {e}");
            }
        }
        out
    }

    /// Copy the field named `key` from `self` into `into`.
    fn apply_into(&self, into: &mut Self, key: &str) -> Result<()> {
        match key {
            "quality" => into.quality = self.quality,
            "q" => into.q = self.q,
            "alpha_q" => into.alpha_q = self.alpha_q,
            "chroma" => into.chroma = self.chroma,
            "premultiplied" => into.premultiplied = self.premultiplied,
            "speed" => into.speed = self.speed,
            "threads" => into.threads = self.threads,
            "grid" => into.grid = self.grid,
            "lossless" => into.lossless = self.lossless,
            other => return Err(Error::invalid(format!("unknown option '{other}'"))),
        }
        Ok(())
    }

    /// The still-encoder tuning these options resolve to: `q` else the
    /// `quality` mapping else [`DEFAULT_QUALITY`] for the colour
    /// planes; `alpha_q` else the `quality` mapping when `quality` was
    /// given, else lossless for the alpha auxiliary; `chroma` /
    /// `premultiplied` / `speed` / `grid` as given; `threads` as
    /// given, else `1` (the encoder fills the execution context's
    /// budget in per frame); `lossless` as given (it then overrides
    /// both quantisers — [`EncodeOptions::lossless`]).
    pub fn resolve(&self) -> EncodeOptions {
        let colour = self
            .quality
            .map(quality_to_base_q_idx)
            .unwrap_or_else(|| quality_to_base_q_idx(DEFAULT_QUALITY));
        let alpha = self.quality.map(quality_to_base_q_idx).unwrap_or(0);
        EncodeOptions::new(
            self.q.unwrap_or(colour),
            self.alpha_q.unwrap_or(alpha),
            self.premultiplied,
        )
        .with_chroma(self.chroma)
        .with_speed(self.speed)
        .with_threads(self.threads.unwrap_or(1))
        .with_grid(self.grid)
        .with_lossless(self.lossless)
    }
}

/// Parse the `grid` option: `auto` / `none` / a tile edge in pixels.
fn parse_grid_option(v: &OptionValue) -> Result<GridTiling> {
    let s = v.as_str()?;
    match s {
        "auto" => Ok(GridTiling::Auto),
        "none" | "single" => Ok(GridTiling::Single),
        other => {
            let edge: u32 = other.parse().map_err(|_| {
                Error::invalid(format!(
                    "avif encode: grid '{other}' is not auto / none / a tile edge in pixels"
                ))
            })?;
            if !(crate::still::GRID_MIN_TILE_DIM..=crate::still::STILL_MAX_CODED_DIM)
                .contains(&edge)
            {
                return Err(Error::invalid(format!(
                    "avif encode: grid tile edge {edge} is outside {}..={}",
                    crate::still::GRID_MIN_TILE_DIM,
                    crate::still::STILL_MAX_CODED_DIM
                )));
            }
            Ok(GridTiling::MaxTile(edge))
        }
    }
}

fn parse_u8_option(key: &str, v: &OptionValue, max: u32) -> Result<u8> {
    let n = v.as_u32()?;
    if n > max {
        return Err(Error::invalid(format!(
            "avif encode: option '{key}' = {n} is outside 0..={max}"
        )));
    }
    Ok(n as u8)
}

impl CodecOptionsStruct for AvifEncoderOptions {
    const SCHEMA: &'static [OptionField] = &[
        OptionField {
            name: "quality",
            kind: OptionKind::U32,
            default: OptionValue::U32(60),
            help: "Quality 0..=100 (100 = lossless; absent = 60): when q is not given, base_q_idx = ((100 - quality) * 255 + 50) / 100 for the colour planes and, when alpha_q is not given, for the alpha auxiliary (lossless when quality is absent too)",
        },
        OptionField {
            name: "q",
            kind: OptionKind::U32,
            default: OptionValue::U32(102),
            help: "AV1 base_q_idx 0..=255 for the colour planes (0 = lossless; absent = quality's, 102 by default); wins over quality",
        },
        OptionField {
            name: "alpha_q",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "AV1 base_q_idx 0..=255 for the alpha auxiliary (0 = lossless); wins over quality",
        },
        OptionField {
            name: "chroma",
            kind: OptionKind::Enum(&["420", "422", "444"]),
            default: OptionValue::String(String::new()),
            help: "Y'CbCr layout a packed RGB(A) / BGR(A) frame is coded in at a lossy quality (default 420; odd extents promote to 444); lossless RGB keeps the identity-matrix 4:4:4 coding",
        },
        OptionField {
            name: "premultiplied",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "Emit the prem reference declaring the colour planes premultiplied by alpha (signalling only)",
        },
        OptionField {
            name: "speed",
            kind: OptionKind::Enum(&["fast", "balanced", "thorough"]),
            default: OptionValue::String(String::new()),
            help: "AV1 search effort (default fast)",
        },
        OptionField {
            name: "threads",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "Worker threads for the AV1 tile search and the grid's tiles (bytes never depend on it); 0 = the execution context's budget (serial unless granted)",
        },
        OptionField {
            name: "grid",
            kind: OptionKind::String,
            default: OptionValue::String(String::new()),
            help: "How the picture splits into coded items (default auto): auto (one item up to 1 MP, a grid of 512-pixel tiles above it), none (one item up to 4096 per axis), or a tile edge 64..=4096 in pixels; the pixels decode the same on every shape",
        },
        OptionField {
            name: "lossless",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "Mathematically lossless: colour and alpha coded with the AV1 lossless arm, RGB input kept as identity-matrix 4:4:4 RGB (matrix_coefficients = 0, full range) so the decoded picture equals the source; wins over quality / q / alpha_q / chroma",
        },
    ];

    fn apply(&mut self, key: &str, value: &OptionValue) -> Result<()> {
        match key {
            "quality" => self.quality = Some(parse_u8_option(key, value, 100)?),
            "q" => self.q = Some(parse_u8_option(key, value, 255)?),
            "alpha_q" => self.alpha_q = Some(parse_u8_option(key, value, 255)?),
            "chroma" => {
                self.chroma = match value.as_str()? {
                    "420" => StillChroma::Yuv420,
                    "422" => StillChroma::Yuv422,
                    "444" => StillChroma::Yuv444,
                    other => {
                        return Err(Error::invalid(format!(
                            "avif encode: chroma '{other}' is not 420 / 422 / 444"
                        )));
                    }
                }
            }
            "premultiplied" => self.premultiplied = value.as_bool()?,
            "speed" => {
                self.speed = match value.as_str()? {
                    "fast" => EncodeSpeed::Fast,
                    "balanced" => EncodeSpeed::Balanced,
                    "thorough" => EncodeSpeed::Thorough,
                    other => {
                        return Err(Error::invalid(format!(
                            "avif encode: speed '{other}' is not fast / balanced / thorough"
                        )));
                    }
                }
            }
            "threads" => {
                let n = value.as_u32()?;
                self.threads = (n > 0).then_some(n as usize);
            }
            "grid" => self.grid = parse_grid_option(value)?,
            "lossless" => self.lossless = value.as_bool()?,
            other => return Err(Error::invalid(format!("unknown option '{other}'"))),
        }
        Ok(())
    }
}

/// Frame-to-AVIF encoder: every video frame becomes one complete AVIF
/// file packet. See the module docs for the input-format mapping and
/// the option surface.
pub struct AvifEncoder {
    params: CodecParameters,
    /// The input frames' layout, from the construction parameters.
    /// Kept apart from `params.pixel_format`, which moves to the coded
    /// layout's label once a frame is coded (what the file decodes
    /// back as) — the next input frame still has the input layout.
    input_format: Option<PixelFormat>,
    opts: EncodeOptions,
    /// `threads` given explicitly (wins over the execution context).
    threads_option: Option<usize>,
    /// The budget of the last [`Encoder::set_execution_context`].
    exec_threads: usize,
    pending: Vec<Packet>,
    flushed: bool,
}

impl AvifEncoder {
    /// Build an encoder announcing `codec_id` in its output parameters.
    pub fn new(codec_id: CodecId) -> Self {
        Self::with_params(CodecParameters::video(codec_id))
    }

    /// Build an encoder from an explicit parameter set, reading the
    /// options leniently: unknown keys and malformed values are
    /// ignored (the historical behaviour of this constructor).
    /// [`Self::try_with_params`] — what [`make_encoder`] and the
    /// registry use — refuses them instead. Option parsing happens
    /// here (construction time), never on the frame path.
    pub fn with_params(params: CodecParameters) -> Self {
        let opts = AvifEncoderOptions::from_params_lenient(&params);
        Self::from_options(params, opts)
    }

    /// Build an encoder from a parameter set whose options are parsed
    /// strictly against [`AvifEncoderOptions`]'s schema: an unknown
    /// key, a malformed value or one outside its range is
    /// `InvalidData`.
    pub fn try_with_params(params: CodecParameters) -> Result<Self> {
        let opts = AvifEncoderOptions::from_params(&params)?;
        Ok(Self::from_options(params, opts))
    }

    /// Build an encoder from a parameter set and already-parsed
    /// options (the option bag on `params` is not read again).
    pub fn from_options(params: CodecParameters, opts: AvifEncoderOptions) -> Self {
        Self {
            input_format: params.pixel_format,
            params,
            opts: opts.resolve(),
            threads_option: opts.threads,
            exec_threads: 1,
            pending: Vec::new(),
            flushed: false,
        }
    }

    /// The still-encoder tuning this encoder codes with (`q`,
    /// `alpha_q`, the `quality` mapping, `chroma`, `premultiplied`,
    /// `speed`, and the worker threads the next frame runs on: the
    /// explicit `threads` option, else the execution context's budget).
    pub fn encode_options(&self) -> EncodeOptions {
        self.opts
            .with_threads(self.threads_option.unwrap_or(self.exec_threads))
    }

    /// The colour a lossy packed-RGB frame is converted to Y′CbCr
    /// with: the stream's signalled primaries / transfer / matrix /
    /// range where specified (a matrix the converter has no kernel for
    /// falls back), the MIAF default (BT.709 / sRGB / BT.601, full
    /// range) otherwise.
    fn lossy_rgb_color(&self) -> ColorInfo {
        let signal = self.params.color_signal;
        let mut color = ColorInfo::miaf_default();
        if !signal.primaries.is_unspecified() {
            color.primaries = signal.primaries.0;
        }
        if !signal.transfer.is_unspecified() {
            color.transfer = signal.transfer.0;
        }
        let m = signal.matrix.0;
        if !signal.matrix.is_unspecified()
            && m != 0
            && oxideav_heif::rgb::matrix_kr_kb(u16::from(m)).is_some()
        {
            color.matrix = m;
        }
        match self.params.resolved_color_range() {
            ColorRange::Limited => color.range = crate::api::ColorRange::Limited,
            ColorRange::Full => color.range = crate::api::ColorRange::Full,
            _ => {}
        }
        color
    }

    /// The Y′CbCr conversion a packed RGB(A) frame goes through — only
    /// at a lossy colour quality (the lossless identity-matrix coding
    /// is byte-exact and stays).
    fn lossy_rgb(&self) -> Option<(ColorInfo, StillChroma)> {
        (!self.opts.is_colour_lossless()).then(|| (self.lossy_rgb_color(), self.opts.chroma))
    }

    /// One framework video frame as the encode reads it, per the
    /// declared `pixel_format`: a view of the frame's own planes (no
    /// copy — the coder cuts each coded item from them), or the packed
    /// RGB rows behind the Y′CbCr conversion at a lossy quality; plus
    /// the item's `colr`.
    fn frame_source<'a>(&self, vf: &'a VideoFrame) -> Result<FrameSource<'a>> {
        let width = self
            .params
            .width
            .ok_or_else(|| Error::invalid("avif encode: CodecParameters.width required"))?;
        let height = self
            .params
            .height
            .ok_or_else(|| Error::invalid("avif encode: CodecParameters.height required"))?;
        let fmt = self
            .input_format
            .ok_or_else(|| Error::invalid("avif encode: CodecParameters.pixel_format required"))?;
        if width == 0 || height == 0 {
            return Err(Error::invalid(
                "avif encode: dimensions must be at least 1x1",
            ));
        }
        use PixelFormat as P;
        use StillChroma as C;
        let planar = |depth: u8, chroma: StillChroma, alpha: bool| {
            planar_view(vf, width, height, depth, chroma, alpha).map(FrameSource::view)
        };
        // The J-variants declare full-range samples with unspecified
        // CICP: an nclx colr with H.273 code 2 in each field.
        let full_range = Colr::Nclx {
            colour_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            full_range: true,
        };
        let rgb = |bpp: usize, rgb_at: [usize; 3]| {
            packed_rgb(vf, width, height, bpp, rgb_at, self.lossy_rgb())
        };
        let mut src = match fmt {
            P::Yuv420P => planar(8, C::Yuv420, false)?,
            P::Yuv422P => planar(8, C::Yuv422, false)?,
            P::Yuv444P => planar(8, C::Yuv444, false)?,
            P::YuvJ420P => planar(8, C::Yuv420, false)?.with_colr(full_range),
            P::YuvJ422P => planar(8, C::Yuv422, false)?.with_colr(full_range),
            P::YuvJ444P => planar(8, C::Yuv444, false)?.with_colr(full_range),
            P::Gray8 => planar(8, C::Monochrome, false)?,
            P::Yuv420P10Le => planar(10, C::Yuv420, false)?,
            P::Yuv422P10Le => planar(10, C::Yuv422, false)?,
            P::Yuv444P10Le => planar(10, C::Yuv444, false)?,
            P::Yuv420P12Le => planar(12, C::Yuv420, false)?,
            P::Yuv422P12Le => planar(12, C::Yuv422, false)?,
            P::Yuv444P12Le => planar(12, C::Yuv444, false)?,
            P::Gray10Le => planar(10, C::Monochrome, false)?,
            P::Gray12Le => planar(12, C::Monochrome, false)?,
            P::Rgb24 => rgb(3, [0, 1, 2])?,
            P::Bgr24 => rgb(3, [2, 1, 0])?,
            P::Rgba => rgb(4, [0, 1, 2])?,
            P::Bgra => rgb(4, [2, 1, 0])?,
            P::Yuva420P => planar(8, C::Yuv420, true)?,
            P::Ya8 => ya8(vf, width, height)?,
            // Planar RGB(A) (planes G, B, R [, A]) is the identity-
            // matrix 4:4:4 item as it stands: Y = G, Cb = B, Cr = R
            // (H.273 §8.3).
            P::Gbrp8 => planar(8, C::Yuv444, false)?.with_colr(identity_full_range_colr()),
            P::Gbrap8 => planar(8, C::Yuv444, true)?.with_colr(identity_full_range_colr()),
            P::Gbrp10Le => planar(10, C::Yuv444, false)?.with_colr(identity_full_range_colr()),
            P::Gbrap10Le => planar(10, C::Yuv444, true)?.with_colr(identity_full_range_colr()),
            P::Gbrp12Le => planar(12, C::Yuv444, false)?.with_colr(identity_full_range_colr()),
            P::Gbrap12Le => planar(12, C::Yuv444, true)?.with_colr(identity_full_range_colr()),
            other => {
                return Err(Error::unsupported(format!(
                    "avif encode: pixel format {other:?} is not supported \
                     (planar YUV 8/10/12-bit, Gray, RGB(A)/BGR(A), Gbrp(a) 8/10/12-bit, \
                     Yuva420P, Ya8)"
                )));
            }
        };
        src.colr = apply_color_signal(src.colr.take(), &self.params);
        Ok(src)
    }

    /// The container metadata of a frame: its ICC / Exif / XMP blobs
    /// refined over the stream's (see the module docs), in the item
    /// framing the muxer expects (the Exif body gets its offset
    /// header).
    fn metadata_props(&self, vf: &VideoFrame) -> StillProperties {
        let meta = Metadata::from_refined_blobs(self.params.blobs(), &vf.blobs());
        StillProperties::default()
            .with_exif(meta.exif.as_deref().map(exif_item_body))
            .with_xmp(meta.xmp)
            .with_icc(meta.icc)
    }
}

/// Fold the stream's [`CodecParameters::color_signal`] into the item's
/// `colr` (see the module docs): the layout's own implications (`YuvJ*`
/// full range, RGB identity) stay, every specified field of the signal
/// is written, and with nothing specified the image is left as built.
fn apply_color_signal(colr: Option<Colr>, params: &CodecParameters) -> Option<Colr> {
    let signal = params.color_signal;
    let range = params.resolved_color_range();
    if signal.is_unspecified() && range.is_unspecified() {
        return colr;
    }
    let (mut p, mut t, mut m, mut full) = match colr {
        Some(Colr::Nclx {
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range,
        }) => (
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range,
        ),
        _ => (2, 2, 2, false),
    };
    let rgb = m == 0;
    if !signal.primaries.is_unspecified() {
        p = u16::from(signal.primaries.0);
    }
    if !signal.transfer.is_unspecified() {
        t = u16::from(signal.transfer.0);
    }
    if !rgb && !signal.matrix.is_unspecified() {
        m = u16::from(signal.matrix.0);
    }
    match range {
        ColorRange::Full => full = true,
        ColorRange::Limited => full = false,
        _ => {}
    }
    Some(Colr::Nclx {
        colour_primaries: p,
        transfer_characteristics: t,
        matrix_coefficients: m,
        full_range: full,
    })
}

/// One input frame as the encode reads it: where its samples come
/// from and the `colr` its item carries.
struct FrameSource<'a> {
    source: Box<dyn TileSource + 'a>,
    colr: Option<Colr>,
}

impl<'a> FrameSource<'a> {
    fn view(view: PictureView<'a>) -> Self {
        Self {
            source: Box::new(view),
            colr: None,
        }
    }

    fn with_colr(mut self, colr: Colr) -> Self {
        self.colr = Some(colr);
        self
    }

    /// The colour signal the encoded item carries, from its `colr` —
    /// what [`Encoder::output_params`] announce.
    fn signal(&self) -> ColorSignal {
        crate::signal::color_signal_for(self.colr.as_ref())
    }
}

fn chroma_shift(chroma: StillChroma) -> (u32, u32) {
    match chroma {
        StillChroma::Yuv420 => (1, 1),
        StillChroma::Yuv422 => (1, 0),
        StillChroma::Yuv444 | StillChroma::Monochrome => (0, 0),
    }
}

/// A planar frame — Y [, Cb, Cr] [, A], 8-bit bytes or little-endian
/// words — viewed in place: chroma planes at the rounded-up
/// sub-sampled size, every plane with its own stride.
fn planar_view<'a>(
    vf: &'a VideoFrame,
    width: u32,
    height: u32,
    bit_depth: u8,
    chroma: StillChroma,
    alpha: bool,
) -> Result<PictureView<'a>> {
    let colour = if chroma == StillChroma::Monochrome {
        1
    } else {
        3
    };
    let planes_needed = colour + usize::from(alpha);
    if vf.image_plane_count() != planes_needed {
        return Err(Error::invalid(format!(
            "avif encode: {chroma:?} {bit_depth}-bit{} expects {planes_needed} planes, got {}",
            if alpha { " with alpha" } else { "" },
            vf.image_plane_count()
        )));
    }
    let (w, h) = (width as usize, height as usize);
    let plane = |i: usize, pw: usize, ph: usize, what: &str| -> Result<PlaneRef<'a>> {
        let p = &vf.planes[i];
        Ok(if bit_depth == 8 {
            PlaneRef::bytes(&p.data, p.stride, 1, 0, pw, ph, what)?
        } else {
            PlaneRef::words_le(&p.data, p.stride, 1, 0, pw, ph, what)?
        })
    };
    let (sx, sy) = chroma_shift(chroma);
    let (cw, ch) = ((w + sx as usize) >> sx, (h + sy as usize) >> sy);
    let cb_cr = if colour == 3 {
        Some((plane(1, cw, ch, "Cb")?, plane(2, cw, ch, "Cr")?))
    } else {
        None
    };
    let a = if alpha {
        Some(plane(colour, w, h, "alpha")?)
    } else {
        None
    };
    Ok(PictureView::new(
        width,
        height,
        bit_depth,
        chroma,
        plane(0, w, h, "Y")?,
        cb_cr,
        a,
    )?)
}

/// Packed RGB(A)/BGR(A): `rgb_at` gives the byte offsets of R, G, B
/// within each `bpp`-byte pixel; a 4th byte (when `bpp == 4`) is the
/// alpha channel. `lossy` = `Some((colour, chroma))` converts the
/// picture to Y′CbCr in `colour` and codes it in `chroma` (at every
/// extent) — rectangle by rectangle as the coder asks, straight from
/// the packed rows; `None` keeps the byte-exact identity-matrix 4:4:4
/// coding, whose planes *are* the channels (Y = G, Cb = B, Cr = R),
/// read in place.
fn packed_rgb<'a>(
    vf: &'a VideoFrame,
    width: u32,
    height: u32,
    bpp: usize,
    rgb_at: [usize; 3],
    lossy: Option<(ColorInfo, StillChroma)>,
) -> Result<FrameSource<'a>> {
    if vf.image_plane_count() != 1 {
        return Err(Error::invalid(format!(
            "avif encode: packed RGB expects 1 plane, got {}",
            vf.image_plane_count()
        )));
    }
    let plane = &vf.planes[0];
    let (w, h) = (width as usize, height as usize);
    match lossy {
        Some((color, chroma)) => {
            let src = LossyRgb::new(
                &plane.data,
                plane.stride,
                width,
                height,
                bpp,
                rgb_at,
                color,
                chroma,
            )?;
            Ok(FrameSource {
                colr: Some(src.color().to_colr()),
                source: Box::new(src),
            })
        }
        None => {
            let channel = |offset: usize, what: &str| {
                PlaneRef::bytes(&plane.data, plane.stride, bpp, offset, w, h, what)
            };
            let alpha = if bpp == 4 {
                Some(channel(3, "alpha")?)
            } else {
                None
            };
            let view = PictureView::new(
                width,
                height,
                8,
                StillChroma::Yuv444,
                channel(rgb_at[1], "G")?,
                Some((channel(rgb_at[2], "B")?, channel(rgb_at[0], "R")?)),
                alpha,
            )?;
            Ok(FrameSource::view(view).with_colr(identity_full_range_colr()))
        }
    }
}

/// The H.273 §8.3 quantisation of an 8-bit picture under `color`:
/// `(luma offset, luma scale, chroma mid, chroma scale)` — equations
/// 20–22 for limited range (16 / 219 / 128 / 224), 38–40 for full
/// range (0 / 255 / 128 / 255).
fn ycbcr_scales(color: &ColorInfo) -> (f64, f64, f64, f64) {
    if color.range == crate::api::ColorRange::Limited {
        (16.0, 219.0, 128.0, 224.0)
    } else {
        (0.0, 255.0, 128.0, 255.0)
    }
}

/// Packed 8-bit RGB(A) rows → the Y′CbCr coding planes of `chroma`
/// in one pass: every pixel is converted to 4:4:4 exactly (H.273
/// §8.3, `(Kr, Kb)` of the signalled matrix — the MIAF default BT.601
/// for an unspecified one — rounded to nearest at the signalled
/// range), the chroma of a sub-sampled layout box-filtered from the
/// rounded 4:4:4 values (`(a + b + c + d + 2) >> 2` per 2×2 for
/// 4:2:0, `(a + b + 1) >> 1` per pair for 4:2:2; at an odd extent the
/// last chroma column / row averages the samples the picture has —
/// the chroma plane is `(extent + 1) >> 1` wide / tall). The same
/// numbers as the contract path's conversion
/// ([`crate::api_codec`]'s `still_of_rgb_in`) without its packed /
/// 4:4:4 intermediates: the working set is the output planes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn packed_rgb_to_ycbcr(
    data: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    bpp: usize,
    rgb_at: [usize; 3],
    color: ColorInfo,
    chroma: StillChroma,
) -> StillImage {
    let chroma = match chroma {
        StillChroma::Monochrome => StillChroma::Yuv444,
        other => other,
    };
    let (kr, kb) = oxideav_heif::rgb::matrix_kr_kb(u16::from(color.matrix))
        .unwrap_or_else(|| oxideav_heif::rgb::matrix_kr_kb(6).expect("BT.601"));
    let kg = 1.0 - kr - kb;
    let (y_off, y_scale, mid, c_scale) = ycbcr_scales(&color);
    let (sx, sy) = chroma_shift(chroma);
    let (cw, ch) = ((w + sx as usize) >> sx, (h + sy as usize) >> sy);
    let mut y_plane = Vec::with_capacity(w * h);
    let mut u_plane = vec![0u16; cw * ch];
    let mut v_plane = vec![0u16; cw * ch];
    // Luma samples under one chroma sample: `1 << (sx + sy)` inside
    // the picture, fewer in the last column / row of an odd extent.
    let cols_in = |cx: usize| ((cx + 1) << sx).min(w) - (cx << sx);
    let rows_in = |cy: usize| ((cy + 1) << sy).min(h) - (cy << sy);
    let round = |v: f64| v.round().clamp(0.0, 255.0) as u16;
    // Rows go in pairs for 4:2:0 so the 2×2 sum closes per row pair
    // without a full-resolution chroma plane: `acc` holds the running
    // Cb / Cr sums of the chroma row under construction.
    let mut acc_u = vec![0u32; cw];
    let mut acc_v = vec![0u32; cw];
    for row in 0..h {
        let s = row * stride;
        let cy = row >> sy;
        for (x, px) in data[s..s + w * bpp].chunks_exact(bpp).enumerate() {
            let r = f64::from(px[rgb_at[0]]) / 255.0;
            let g = f64::from(px[rgb_at[1]]) / 255.0;
            let b = f64::from(px[rgb_at[2]]) / 255.0;
            let ey = kr * r + kg * g + kb * b;
            let pb = (b - ey) / (2.0 * (1.0 - kb));
            let pr = (r - ey) / (2.0 * (1.0 - kr));
            y_plane.push(round(y_off + y_scale * ey));
            let cb = round(mid + c_scale * pb);
            let cr = round(mid + c_scale * pr);
            acc_u[x >> sx] += u32::from(cb);
            acc_v[x >> sx] += u32::from(cr);
        }
        // The last luma row of this chroma row: close the sums.
        if (row >> sy) != ((row + 1) >> sy) || row + 1 == h {
            let base = cy * cw;
            let rows = rows_in(cy);
            let cells = u_plane[base..base + cw]
                .iter_mut()
                .zip(&mut v_plane[base..base + cw]);
            for (cx, ((u, v), (su, sv))) in cells
                .zip(acc_u.iter_mut().zip(acc_v.iter_mut()))
                .enumerate()
            {
                let count = (cols_in(cx) * rows) as u32;
                *u = ((*su + count / 2) / count) as u16;
                *v = ((*sv + count / 2) / count) as u16;
                *su = 0;
                *sv = 0;
            }
        }
    }
    StillImage::yuv(w as u32, h as u32, 8, chroma, y_plane, u_plane, v_plane)
        .expect("planes sized to the layout")
        .with_colr(color.to_colr())
}

/// The framework label of the layout an item is coded in (what the
/// crate's decoder reports for the file): planar RGB for the identity
/// matrix, `YuvJ*` for full-range 8-bit Y′CbCr, the storage label
/// otherwise.
fn coded_label(src: &FrameSource<'_>) -> Option<PixelFormat> {
    let chroma = src.source.chroma();
    let (sx, sy) = chroma.subsampling();
    let layout = crate::image::AvifPixelFormat::from_layout(
        src.source.bit_depth(),
        sx as u8,
        sy as u8,
        chroma == StillChroma::Monochrome,
        src.source.has_alpha(),
    )?;
    Some(crate::signal::labelled_pixel_format(layout, &src.signal()))
}

/// `Ya8`: one plane of interleaved gray / alpha bytes, read in place
/// as a monochrome picture plus its alpha plane.
fn ya8<'a>(vf: &'a VideoFrame, width: u32, height: u32) -> Result<FrameSource<'a>> {
    if vf.image_plane_count() != 1 {
        return Err(Error::invalid(format!(
            "avif encode: Ya8 expects 1 interleaved plane, got {}",
            vf.image_plane_count()
        )));
    }
    let plane = &vf.planes[0];
    let (w, h) = (width as usize, height as usize);
    // The alpha byte ends each pixel: checking it first refuses a
    // plane that holds the last gray sample but not its alpha.
    let alpha = PlaneRef::bytes(&plane.data, plane.stride, 2, 1, w, h, "Ya8")?;
    let gray = PlaneRef::bytes(&plane.data, plane.stride, 2, 0, w, h, "Ya8")?;
    Ok(FrameSource::view(PictureView::new(
        width,
        height,
        8,
        StillChroma::Monochrome,
        gray,
        None,
        Some(alpha),
    )?))
}

impl Encoder for AvifEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            // The input geometry and layout are the construction
            // parameters' (`width` / `height` / `pixel_format`); a
            // mid-stream input-shape change is not re-configured
            // (`oxideav-core` 0.1.43 `Encoder::send_frame` contract).
            Frame::StreamParams(_) => {
                return Err(Error::unsupported(
                    "avif encode: cannot re-configure for a mid-stream input-shape change \
                     (Frame::StreamParams); build a new encoder for the new shape",
                ));
            }
            other => {
                return Err(Error::invalid(format!(
                    "avif encode: expected a video frame, got {other:?}"
                )));
            }
        };
        let src = self.frame_source(vf)?;
        let props = self.metadata_props(vf);
        self.params.color_signal = src.signal();
        // The output parameters carry the coded layout's label (what
        // the file decodes back as), not the input label: `Rgb24` is
        // coded as `Gbrp8` losslessly and as Y′CbCr at a lossy quality.
        if let Some(label) = coded_label(&src) {
            self.params.pixel_format = Some(label);
        }
        // The coder cuts each coded item (a grid's tiles one at a
        // time) straight from the frame's planes.
        let bytes = encode_source_auto(
            &*src.source,
            src.colr.as_ref(),
            &props,
            &self.encode_options(),
        )?;
        let mut pkt = Packet::new(0, TimeBase::new(1, 90_000), bytes);
        if let Some(pts) = vf.pts {
            pkt = pkt.with_pts(pts);
        }
        pkt = pkt.with_keyframe(true);
        self.pending.push(pkt);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if self.pending.is_empty() {
            return if self.flushed {
                Err(Error::Eof)
            } else {
                Err(Error::NeedMore)
            };
        }
        Ok(self.pending.remove(0))
    }

    fn flush(&mut self) -> Result<()> {
        self.flushed = true;
        Ok(())
    }

    /// The worker budget for the AV1 tile search of every coded item;
    /// an explicit `threads` option wins over it. The bytes never
    /// depend on the budget (the tile layout follows the picture size).
    fn set_execution_context(&mut self, ctx: &oxideav_core::ExecutionContext) {
        self.exec_threads = ctx.threads.max(1);
    }
}

/// Direct factory endpoint (matches the crate's dual-API convention):
/// build a boxed AVIF [`Encoder`] from a parameter set. The options
/// are parsed strictly ([`AvifEncoder::try_with_params`]): an unknown
/// key or a malformed value is `InvalidData` here rather than silently
/// ignored.
pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(AvifEncoder::try_with_params(params.clone())?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::Decoder;
    use oxideav_core::frame::{VideoFrame, VideoPlane};

    /// The one-pass packed-RGB conversion is sample-identical to the
    /// general one (`still_of_rgb_general`: 4:4:4 then box filter) for
    /// every matrix the converter has a kernel for, both ranges, the
    /// three layouts, odd extents, BGR order and a padded stride.
    #[test]
    fn packed_rgb_one_pass_matches_the_contract_conversion() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        };
        for (w, h) in [(16usize, 12usize), (17, 9), (8, 2), (1, 1), (30, 31)] {
            for matrix in [1u8, 2, 4, 5, 6, 7, 9] {
                for range in [
                    crate::api::ColorRange::Full,
                    crate::api::ColorRange::Limited,
                ] {
                    for chroma in [
                        StillChroma::Yuv420,
                        StillChroma::Yuv422,
                        StillChroma::Yuv444,
                    ] {
                        for (bpp, order) in [(3usize, [0usize, 1, 2]), (4, [2, 1, 0])] {
                            let color = ColorInfo::new(range, 1, 13, matrix);
                            let stride = w * bpp + 5;
                            let mut data = vec![0u8; stride * h];
                            for row in 0..h {
                                for b in &mut data[row * stride..row * stride + w * bpp] {
                                    *b = next();
                                }
                            }
                            let plane = oxideav_core::frame::VideoPlane { stride, data };
                            let fast = packed_rgb_to_ycbcr(
                                &plane.data,
                                plane.stride,
                                w,
                                h,
                                bpp,
                                order,
                                color,
                                chroma,
                            );
                            let mut rgb = Vec::with_capacity(w * h * 3);
                            for row in 0..h {
                                for px in plane.data[row * stride..row * stride + w * bpp]
                                    .chunks_exact(bpp)
                                {
                                    rgb.extend_from_slice(&[
                                        px[order[0]],
                                        px[order[1]],
                                        px[order[2]],
                                    ]);
                                }
                            }
                            let slow = crate::api_codec::still_of_rgb_general(
                                w as u32, h as u32, &rgb, 3, color, chroma,
                            )
                            .expect("contract conversion");
                            let label = format!("{w}x{h} m{matrix} {range:?} {chroma:?} bpp{bpp}");
                            assert_eq!(fast.chroma, slow.chroma, "{label}");
                            assert_eq!(fast.y, slow.y, "{label}: Y");
                            assert_eq!(fast.u, slow.u, "{label}: Cb");
                            assert_eq!(fast.v, slow.v, "{label}: Cr");
                            assert_eq!(
                                format!("{:?}", fast.colr),
                                format!("{:?}", slow.colr),
                                "{label}: colr"
                            );
                        }
                    }
                }
            }
        }
    }

    fn video_params(fmt: PixelFormat, w: u32, h: u32) -> CodecParameters {
        let mut p = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
        p.width = Some(w);
        p.height = Some(h);
        p.pixel_format = Some(fmt);
        p
    }

    fn planar_frame(dims: &[(usize, usize)], seed: u8) -> Frame {
        Frame::Video(VideoFrame {
            pts: Some(7),
            planes: dims
                .iter()
                .enumerate()
                .map(|(i, &(w, h))| VideoPlane {
                    stride: w,
                    data: (0..w * h)
                        .map(|j| (j as u8).wrapping_mul(31) ^ seed.wrapping_add(i as u8))
                        .collect(),
                })
                .collect(),
        })
    }

    /// A `Frame::StreamParams` event is refused with `Unsupported`
    /// (no mid-stream re-configuration), and the encoder keeps working
    /// on frames of its construction shape afterwards — including a
    /// second packed-RGB frame after the output label moved to the
    /// coded layout.
    #[test]
    fn stream_params_event_is_unsupported() {
        let params = video_params(PixelFormat::Yuv420P, 16, 16);
        let mut enc = make_encoder(&params).expect("encoder");
        let ev = Frame::StreamParams(Box::new(video_params(PixelFormat::Yuv444P, 32, 32)));
        assert!(matches!(enc.send_frame(&ev), Err(Error::Unsupported(_))));
        enc.send_frame(&planar_frame(&[(16, 16), (8, 8), (8, 8)], 3))
            .expect("frame after the refused event");
        enc.receive_packet().expect("packet");
        let rgb = video_params(PixelFormat::Rgb24, 16, 16);
        let mut enc = make_encoder(&rgb).expect("encoder");
        for seed in 0..2 {
            enc.send_frame(&planar_frame(&[(48, 16)], seed))
                .expect("packed RGB frame");
            enc.receive_packet().expect("packet");
        }
    }

    /// Trait-level end-to-end: send a Yuv420P frame at `quality=100`,
    /// receive one complete AVIF file packet, decode it back
    /// pixel-exact through the crate's own decoder; the same frame
    /// with no option codes at [`DEFAULT_QUALITY`] (lossy, smaller).
    #[test]
    fn encoder_trait_round_trips_yuv420_frame() {
        let mut params = video_params(PixelFormat::Yuv420P, 16, 16);
        let defaults = make_encoder(&params).expect("default encoder");
        let mut defaults = defaults;
        defaults
            .send_frame(&planar_frame(&[(16, 16), (8, 8), (8, 8)], 3))
            .expect("send_frame");
        let lossy = defaults.receive_packet().expect("default packet");
        params.options = oxideav_core::CodecOptions::new().set("quality", "100");
        let mut enc = make_encoder(&params).expect("make encoder");
        let frame = planar_frame(&[(16, 16), (8, 8), (8, 8)], 3);
        enc.send_frame(&frame).expect("send_frame");
        let pkt = enc.receive_packet().expect("receive_packet");
        assert_eq!(pkt.pts, Some(7), "pts carried through");
        assert!(pkt.flags.keyframe, "still images are sync samples");

        // NeedMore until the next frame, Eof after flush.
        assert!(matches!(enc.receive_packet(), Err(Error::NeedMore)));
        enc.flush().expect("flush");
        assert!(matches!(enc.receive_packet(), Err(Error::Eof)));

        assert!(
            lossy.data.len() < pkt.data.len(),
            "the default quality codes lossily: {} vs lossless {}",
            lossy.data.len(),
            pkt.data.len()
        );
        assert_eq!(
            AvifEncoderOptions::default().resolve().base_q_idx,
            quality_to_base_q_idx(DEFAULT_QUALITY)
        );
        assert_eq!(AvifEncoderOptions::default().resolve().alpha_q_idx, 0);

        // The packet is a complete AVIF file that decodes back to the
        // input planes exactly (quality 100 = lossless).
        let mut dec = crate::decoder::AvifDecoder::new(CodecId::new(crate::CODEC_ID_STR));
        dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), pkt.data.clone()))
            .expect("decode muxed file");
        let vf = match dec.receive_frame().expect("frame") {
            Frame::Video(v) => v,
            other => panic!("expected VideoFrame, got {other:?}"),
        };
        let src = match &frame {
            Frame::Video(v) => v,
            _ => unreachable!(),
        };
        assert_eq!(vf.image_plane_count(), 3);
        for i in 0..3 {
            assert_eq!(vf.planes[i].data, src.planes[i].data, "plane {i}");
        }
    }

    /// Rgba input: the trait path routes through the identity-matrix
    /// 4:4:4 encode with an alpha auxiliary; a lossy `q` option
    /// produces a smaller file than lossless.
    #[test]
    fn encoder_trait_rgba_and_q_option() {
        let (w, h) = (16u32, 16u32);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| ((i * 13) & 0xff) as u8).collect();
        let mk_frame = || {
            Frame::Video(VideoFrame {
                pts: Some(0),
                planes: vec![VideoPlane {
                    stride: (w * 4) as usize,
                    data: rgba.clone(),
                }],
            })
        };
        let params = video_params(PixelFormat::Rgba, w, h);
        let mut enc = make_encoder(&params).expect("encoder");
        enc.send_frame(&mk_frame()).expect("send");
        let lossless = enc.receive_packet().expect("packet");
        let info = crate::inspect::inspect(&lossless.data).expect("inspect");
        assert!(info.has_alpha, "alpha auxiliary present");

        let mut lossy_params = video_params(PixelFormat::Rgba, w, h);
        lossy_params.options = oxideav_core::CodecOptions::new().set("q", "120");
        let mut enc = make_encoder(&lossy_params).expect("encoder");
        enc.send_frame(&mk_frame()).expect("send");
        let lossy = enc.receive_packet().expect("packet");
        assert!(
            lossy.data.len() < lossless.data.len(),
            "q=120 ({}) must be smaller than lossless ({})",
            lossy.data.len(),
            lossless.data.len()
        );
    }

    /// Missing parameters and unsupported formats surface precise
    /// errors instead of panics.
    #[test]
    fn encoder_trait_rejects_bad_setup() {
        // No width/height/pixel_format.
        let params = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
        let mut enc = make_encoder(&params).expect("encoder");
        let frame = planar_frame(&[(8, 8)], 0);
        match enc.send_frame(&frame) {
            Err(Error::InvalidData(msg)) => assert!(msg.contains("width"), "{msg}"),
            other => panic!("expected InvalidData, got {other:?}"),
        }
        // Unsupported pixel format.
        let params = video_params(PixelFormat::Pal8, 8, 8);
        let mut enc = make_encoder(&params).expect("encoder");
        match enc.send_frame(&frame) {
            Err(Error::Unsupported(msg)) => assert!(msg.contains("Pal8"), "{msg}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
