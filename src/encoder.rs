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
//! * `quality` — `0..=100`, `100` = lossless (the default). When `q`
//!   is not given, `base_q_idx = ((100 − quality) × 255 + 50) / 100`
//!   (the [`EncodeOptions::with_quality`] mapping: 100 → 0, 90 → 26,
//!   75 → 64, 50 → 128, 10 → 230, 0 → 255); the alpha auxiliary
//!   follows the same mapping when `alpha_q` is not given.
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
//!
//! With neither `quality` nor `q` the encode is lossless.

use oxideav_core::frame::VideoFrame;
use oxideav_core::{
    parse_options, CodecId, CodecOptionsStruct, CodecParameters, ColorRange, ColorSignal, Encoder,
    Error, Frame, OptionField, OptionKind, OptionValue, Packet, PixelFormat, Result, TimeBase,
};

use crate::api::{ColorInfo, EncodeOptions, EncodeSpeed};
use crate::meta::Colr;
use crate::still::{encode_still, identity_full_range_colr, StillChroma, StillImage};

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
        }
    }
}

impl AvifEncoderOptions {
    /// The defaults (lossless, 4:2:0 for lossy RGB, straight alpha).
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
            if let Ok(parsed) = parse_options::<Self>(&one) {
                if let Err(e) = parsed.apply_into(&mut out, k) {
                    debug_assert!(false, "lenient option apply: {e}");
                }
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
            other => return Err(Error::invalid(format!("unknown option '{other}'"))),
        }
        Ok(())
    }

    /// The still-encoder tuning these options resolve to: `q` else the
    /// `quality` mapping else lossless for the colour planes, the same
    /// for alpha with `alpha_q`; `chroma` / `premultiplied` / `speed`
    /// as given; `threads` as given, else `1` (the encoder fills the
    /// execution context's budget in per frame).
    pub fn resolve(&self) -> EncodeOptions {
        let from_quality = self.quality.map(quality_to_base_q_idx).unwrap_or(0);
        EncodeOptions::new(
            self.q.unwrap_or(from_quality),
            self.alpha_q.unwrap_or(from_quality),
            self.premultiplied,
        )
        .with_chroma(self.chroma)
        .with_speed(self.speed)
        .with_threads(self.threads.unwrap_or(1))
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
            default: OptionValue::U32(100),
            help: "Quality 0..=100 (100 = lossless): when q is not given, base_q_idx = ((100 - quality) * 255 + 50) / 100 for the colour planes and, when alpha_q is not given, for the alpha auxiliary",
        },
        OptionField {
            name: "q",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "AV1 base_q_idx 0..=255 for the colour planes (0 = lossless); wins over quality",
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
            help: "Worker threads for the AV1 tile search (bytes never depend on it); 0 = the execution context's budget (serial unless granted)",
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
                        )))
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
                        )))
                    }
                }
            }
            "threads" => {
                let n = value.as_u32()?;
                self.threads = (n > 0).then_some(n as usize);
            }
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
        (self.opts.base_q_idx > 0).then(|| (self.lossy_rgb_color(), self.opts.chroma))
    }

    /// Convert one framework video frame into a [`StillImage`] per the
    /// declared `pixel_format`.
    fn frame_to_still(&self, vf: &VideoFrame) -> Result<StillImage> {
        let width = self
            .params
            .width
            .ok_or_else(|| Error::invalid("avif encode: CodecParameters.width required"))?;
        let height = self
            .params
            .height
            .ok_or_else(|| Error::invalid("avif encode: CodecParameters.height required"))?;
        let fmt = self
            .params
            .pixel_format
            .ok_or_else(|| Error::invalid("avif encode: CodecParameters.pixel_format required"))?;

        let img = match fmt {
            PixelFormat::Yuv420P | PixelFormat::YuvJ420P => planar8(
                vf,
                width,
                height,
                StillChroma::Yuv420,
                fmt == PixelFormat::YuvJ420P,
            )?,
            PixelFormat::Yuv422P | PixelFormat::YuvJ422P => planar8(
                vf,
                width,
                height,
                StillChroma::Yuv422,
                fmt == PixelFormat::YuvJ422P,
            )?,
            PixelFormat::Yuv444P | PixelFormat::YuvJ444P => planar8(
                vf,
                width,
                height,
                StillChroma::Yuv444,
                fmt == PixelFormat::YuvJ444P,
            )?,
            PixelFormat::Gray8 => planar8(vf, width, height, StillChroma::Monochrome, false)?,
            PixelFormat::Yuv420P10Le => planar16(vf, width, height, 10, StillChroma::Yuv420)?,
            PixelFormat::Yuv422P10Le => planar16(vf, width, height, 10, StillChroma::Yuv422)?,
            PixelFormat::Yuv444P10Le => planar16(vf, width, height, 10, StillChroma::Yuv444)?,
            PixelFormat::Yuv420P12Le => planar16(vf, width, height, 12, StillChroma::Yuv420)?,
            PixelFormat::Yuv422P12Le => planar16(vf, width, height, 12, StillChroma::Yuv422)?,
            PixelFormat::Yuv444P12Le => planar16(vf, width, height, 12, StillChroma::Yuv444)?,
            PixelFormat::Gray10Le => planar16(vf, width, height, 10, StillChroma::Monochrome)?,
            PixelFormat::Gray12Le => planar16(vf, width, height, 12, StillChroma::Monochrome)?,
            PixelFormat::Rgb24 => packed_rgb(vf, width, height, 3, [0, 1, 2], self.lossy_rgb())?,
            PixelFormat::Bgr24 => packed_rgb(vf, width, height, 3, [2, 1, 0], self.lossy_rgb())?,
            PixelFormat::Rgba => packed_rgb(vf, width, height, 4, [0, 1, 2], self.lossy_rgb())?,
            PixelFormat::Bgra => packed_rgb(vf, width, height, 4, [2, 1, 0], self.lossy_rgb())?,
            PixelFormat::Yuva420P => yuva420(vf, width, height)?,
            PixelFormat::Ya8 => ya8(vf, width, height)?,
            PixelFormat::Gbrp8 => planar_gbr(vf, width, height, 8, false)?,
            PixelFormat::Gbrap8 => planar_gbr(vf, width, height, 8, true)?,
            PixelFormat::Gbrp10Le => planar_gbr(vf, width, height, 10, false)?,
            PixelFormat::Gbrap10Le => planar_gbr(vf, width, height, 10, true)?,
            PixelFormat::Gbrp12Le => planar_gbr(vf, width, height, 12, false)?,
            PixelFormat::Gbrap12Le => planar_gbr(vf, width, height, 12, true)?,
            other => {
                return Err(Error::unsupported(format!(
                    "avif encode: pixel format {other:?} is not supported \
                     (planar YUV 8/10/12-bit, Gray, RGB(A)/BGR(A), Gbrp(a) 8/10/12-bit, \
                     Yuva420P, Ya8)"
                )))
            }
        };
        Ok(apply_color_signal(img, &self.params))
    }
}

/// Fold the stream's [`CodecParameters::color_signal`] into the item's
/// `colr` (see the module docs): the layout's own implications (`YuvJ*`
/// full range, RGB identity) stay, every specified field of the signal
/// is written, and with nothing specified the image is left as built.
fn apply_color_signal(mut img: StillImage, params: &CodecParameters) -> StillImage {
    let signal = params.color_signal;
    let range = params.resolved_color_range();
    if signal.is_unspecified() && range.is_unspecified() {
        return img;
    }
    let (mut p, mut t, mut m, mut full) = match img.colr {
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
    img.colr = Some(Colr::Nclx {
        colour_primaries: p,
        transfer_characteristics: t,
        matrix_coefficients: m,
        full_range: full,
    });
    img
}

/// The colour signal an encoded item carries, from its `colr` —
/// what [`Encoder::output_params`] announce.
fn signal_of(img: &StillImage) -> ColorSignal {
    crate::signal::color_signal_for(img.colr.as_ref())
}

/// Extract one tightly-packed plane (`w × h` samples, 1 byte each)
/// from a frame plane whose stride may exceed the row width.
fn pack8(plane: &oxideav_core::frame::VideoPlane, w: usize, h: usize) -> Result<Vec<u16>> {
    if plane.data.len() < plane.stride * (h - 1) + w || plane.stride < w {
        return Err(Error::invalid(format!(
            "avif encode: plane too short (stride {}, need {w}x{h})",
            plane.stride
        )));
    }
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        let s = row * plane.stride;
        out.extend(plane.data[s..s + w].iter().map(|&b| b as u16));
    }
    Ok(out)
}

/// Extract one tightly-packed plane of little-endian 2-byte samples.
fn pack16le(plane: &oxideav_core::frame::VideoPlane, w: usize, h: usize) -> Result<Vec<u16>> {
    let row_bytes = w * 2;
    if plane.data.len() < plane.stride * (h - 1) + row_bytes || plane.stride < row_bytes {
        return Err(Error::invalid(format!(
            "avif encode: 16-bit plane too short (stride {}, need {w}x{h}x2)",
            plane.stride
        )));
    }
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        let s = row * plane.stride;
        out.extend(
            plane.data[s..s + row_bytes]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]])),
        );
    }
    Ok(out)
}

fn chroma_shift(chroma: StillChroma) -> (u32, u32) {
    match chroma {
        StillChroma::Yuv420 => (1, 1),
        StillChroma::Yuv422 => (1, 0),
        StillChroma::Yuv444 | StillChroma::Monochrome => (0, 0),
    }
}

fn planar8(
    vf: &VideoFrame,
    width: u32,
    height: u32,
    chroma: StillChroma,
    full_range: bool,
) -> Result<StillImage> {
    let planes_needed = if chroma == StillChroma::Monochrome {
        1
    } else {
        3
    };
    if vf.image_plane_count() != planes_needed {
        return Err(Error::invalid(format!(
            "avif encode: {chroma:?} expects {planes_needed} planes, got {}",
            vf.image_plane_count()
        )));
    }
    let (sx, sy) = chroma_shift(chroma);
    let y = pack8(&vf.planes[0], width as usize, height as usize)?;
    let (u, v) = if planes_needed == 3 {
        let (cw, ch) = ((width >> sx) as usize, (height >> sy) as usize);
        (pack8(&vf.planes[1], cw, ch)?, pack8(&vf.planes[2], cw, ch)?)
    } else {
        (Vec::new(), Vec::new())
    };
    let mut img = StillImage::yuv(width, height, 8, chroma, y, u, v)?;
    if full_range {
        // The J-variants declare full-range samples with unspecified
        // CICP; carry that through as an nclx colr (H.273 code 2 =
        // unspecified for each of the three fields).
        img = img.with_colr(crate::meta::Colr::Nclx {
            colour_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            full_range: true,
        });
    }
    Ok(img)
}

fn planar16(
    vf: &VideoFrame,
    width: u32,
    height: u32,
    bit_depth: u8,
    chroma: StillChroma,
) -> Result<StillImage> {
    let planes_needed = if chroma == StillChroma::Monochrome {
        1
    } else {
        3
    };
    if vf.image_plane_count() != planes_needed {
        return Err(Error::invalid(format!(
            "avif encode: {chroma:?} {bit_depth}-bit expects {planes_needed} planes, got {}",
            vf.image_plane_count()
        )));
    }
    let (sx, sy) = chroma_shift(chroma);
    let y = pack16le(&vf.planes[0], width as usize, height as usize)?;
    let (u, v) = if planes_needed == 3 {
        let (cw, ch) = ((width >> sx) as usize, (height >> sy) as usize);
        (
            pack16le(&vf.planes[1], cw, ch)?,
            pack16le(&vf.planes[2], cw, ch)?,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(StillImage::yuv(width, height, bit_depth, chroma, y, u, v)?)
}

/// Packed RGB(A)/BGR(A): `rgb_at` gives the byte offsets of R, G, B
/// within each `bpp`-byte pixel; a 4th byte (when `bpp == 4`) is the
/// alpha channel. `lossy` = `Some((colour, chroma))` converts the
/// picture to Y′CbCr in `colour` and codes it in `chroma` (odd extents
/// 4:4:4); `None` keeps the byte-exact identity-matrix 4:4:4 coding.
fn packed_rgb(
    vf: &VideoFrame,
    width: u32,
    height: u32,
    bpp: usize,
    rgb_at: [usize; 3],
    lossy: Option<(ColorInfo, StillChroma)>,
) -> Result<StillImage> {
    if vf.image_plane_count() != 1 {
        return Err(Error::invalid(format!(
            "avif encode: packed RGB expects 1 plane, got {}",
            vf.image_plane_count()
        )));
    }
    let plane = &vf.planes[0];
    let (w, h) = (width as usize, height as usize);
    let row_bytes = w * bpp;
    if plane.data.len() < plane.stride * (h - 1) + row_bytes || plane.stride < row_bytes {
        return Err(Error::invalid(format!(
            "avif encode: packed plane too short (stride {}, need {w}x{h}x{bpp})",
            plane.stride
        )));
    }
    let mut rgb = Vec::with_capacity(w * h * 3);
    let mut alpha = if bpp == 4 {
        Vec::with_capacity(w * h)
    } else {
        Vec::new()
    };
    for row in 0..h {
        let s = row * plane.stride;
        for px in plane.data[s..s + row_bytes].chunks_exact(bpp) {
            rgb.push(px[rgb_at[0]]);
            rgb.push(px[rgb_at[1]]);
            rgb.push(px[rgb_at[2]]);
            if bpp == 4 {
                alpha.push(px[3] as u16);
            }
        }
    }
    let img = match lossy {
        Some((color, chroma)) => {
            crate::api_codec::still_of_rgb_in(width, height, &rgb, 3, color, chroma)?
        }
        None => StillImage::rgb8(width, height, &rgb)?,
    };
    if bpp == 4 {
        Ok(img.with_alpha(alpha)?)
    } else {
        Ok(img)
    }
}

/// The framework label of the layout an item is coded in (what the
/// crate's decoder reports for the file): planar RGB for the identity
/// matrix, `YuvJ*` for full-range 8-bit Y′CbCr, the storage label
/// otherwise.
fn coded_label(img: &StillImage) -> Option<PixelFormat> {
    let (sx, sy) = img.chroma.subsampling();
    let layout = crate::image::AvifPixelFormat::from_layout(
        img.bit_depth,
        sx as u8,
        sy as u8,
        img.chroma == StillChroma::Monochrome,
        img.alpha.is_some(),
    )?;
    Some(crate::signal::labelled_pixel_format(
        layout,
        &signal_of(img),
    ))
}

/// Planar RGB(A) (`Gbrp*` / `Gbrap*`: planes G, B, R [, A]) → an
/// identity-matrix 4:4:4 item (Y = G, Cb = B, Cr = R, H.273 §8.3) at
/// `bit_depth`, the alpha plane as the auxiliary.
fn planar_gbr(
    vf: &VideoFrame,
    width: u32,
    height: u32,
    bit_depth: u8,
    alpha: bool,
) -> Result<StillImage> {
    let planes_needed = if alpha { 4 } else { 3 };
    if vf.image_plane_count() != planes_needed {
        return Err(Error::invalid(format!(
            "avif encode: planar RGB{} {bit_depth}-bit expects {planes_needed} planes, got {}",
            if alpha { "A" } else { "" },
            vf.image_plane_count()
        )));
    }
    let (w, h) = (width as usize, height as usize);
    let pack = |i: usize| -> Result<Vec<u16>> {
        if bit_depth == 8 {
            pack8(&vf.planes[i], w, h)
        } else {
            pack16le(&vf.planes[i], w, h)
        }
    };
    let (g, b, r) = (pack(0)?, pack(1)?, pack(2)?);
    let img = StillImage::yuv(width, height, bit_depth, StillChroma::Yuv444, g, b, r)?
        .with_colr(identity_full_range_colr());
    if alpha {
        Ok(img.with_alpha(pack(3)?)?)
    } else {
        Ok(img)
    }
}

fn yuva420(vf: &VideoFrame, width: u32, height: u32) -> Result<StillImage> {
    if vf.image_plane_count() != 4 {
        return Err(Error::invalid(format!(
            "avif encode: Yuva420P expects 4 planes, got {}",
            vf.image_plane_count()
        )));
    }
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (width.div_ceil(2) as usize, height.div_ceil(2) as usize);
    let img = StillImage::yuv(
        width,
        height,
        8,
        StillChroma::Yuv420,
        pack8(&vf.planes[0], w, h)?,
        pack8(&vf.planes[1], cw, ch)?,
        pack8(&vf.planes[2], cw, ch)?,
    )?;
    Ok(img.with_alpha(pack8(&vf.planes[3], w, h)?)?)
}

fn ya8(vf: &VideoFrame, width: u32, height: u32) -> Result<StillImage> {
    if vf.image_plane_count() != 1 {
        return Err(Error::invalid(format!(
            "avif encode: Ya8 expects 1 interleaved plane, got {}",
            vf.image_plane_count()
        )));
    }
    let plane = &vf.planes[0];
    let (w, h) = (width as usize, height as usize);
    let row_bytes = w * 2;
    if plane.data.len() < plane.stride * (h - 1) + row_bytes || plane.stride < row_bytes {
        return Err(Error::invalid(format!(
            "avif encode: Ya8 plane too short (stride {}, need {w}x{h}x2)",
            plane.stride
        )));
    }
    let mut y = Vec::with_capacity(w * h);
    let mut a = Vec::with_capacity(w * h);
    for row in 0..h {
        let s = row * plane.stride;
        for px in plane.data[s..s + row_bytes].chunks_exact(2) {
            y.push(px[0] as u16);
            a.push(px[1] as u16);
        }
    }
    let img = StillImage::yuv(
        width,
        height,
        8,
        StillChroma::Monochrome,
        y,
        Vec::new(),
        Vec::new(),
    )?;
    Ok(img.with_alpha(a)?)
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
            other => {
                return Err(Error::invalid(format!(
                    "avif encode: expected a video frame, got {other:?}"
                )))
            }
        };
        let img = self.frame_to_still(vf)?;
        self.params.color_signal = signal_of(&img);
        // The output parameters carry the coded layout's label (what
        // the file decodes back as), not the input label: `Rgb24` is
        // coded as `Gbrp8` losslessly and as Y′CbCr at a lossy quality.
        if let Some(label) = coded_label(&img) {
            self.params.pixel_format = Some(label);
        }
        let bytes = encode_still(&img, &self.encode_options())?;
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
    use oxideav_core::frame::{VideoFrame, VideoPlane};
    use oxideav_core::Decoder;

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

    /// Trait-level end-to-end: send a Yuv420P frame, receive one
    /// complete AVIF file packet, decode it back pixel-exact through
    /// the crate's own decoder.
    #[test]
    fn encoder_trait_round_trips_yuv420_frame() {
        let params = video_params(PixelFormat::Yuv420P, 16, 16);
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

        // The packet is a complete AVIF file that decodes back to the
        // input planes exactly (lossless default).
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
