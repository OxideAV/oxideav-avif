//! The `registry`-gated half of the image-crate API: `decode*` /
//! `encode*` over [`crate::api::AvifImage`], the AV1 work done by
//! `oxideav-av1`. The framework `Decoder` / `Encoder`
//! ([`crate::AvifDecoder`] / [`crate::AvifEncoder`]) are thin adapters
//! over the functions here.

use std::io::{Read, Write};
use std::time::Duration;

use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{CodecId, CodecParameters, ColorSignal, Packet, TimeBase};

use crate::api::{
    burst_members, exif_item_body, exif_tiff_bytes, has_moov, item_colrs, AvifImage, ColorInfo,
    ColorRange, DecodeOptions, EncodeOptions, Frame, Metadata, RgbImage, RgbaImage, StillChroma,
};
use crate::av1_config::{parse_av1c, Av1CodecConfig};
use crate::avis::{parse_avis, sample_bytes};
use crate::decoder::{
    core_to_avif_frame, decode_item_output, infer_av1_pixmap, tone_map_of, validate_av1_config,
    DecodeCtx, MAX_AV1_ITEM_BYTES,
};
use crate::error::{AvifError as Error, Result};
use crate::frame_bridge::from_heif;
use crate::image::{AvifPixelFormat, AvifPlane};
use crate::inspect::{build_info, build_info_derived, build_info_grid, AvifInfo};
use crate::meta::{Colr, ITEM_TYPE_IDEN, ITEM_TYPE_IOVL, ITEM_TYPE_TMAP};
use crate::parser::{
    audit_mif1, classify_brands, parse, parse_header, AvifHeader, BrandClass, ITEM_TYPE_AV01,
    ITEM_TYPE_GRID,
};
use crate::signal::{color_signal_for, layout_of_record};
use crate::still::{encode_still_auto, StillImage, StillProperties};
use oxideav_heif::{Chroma, HeifFrame};

/// Framework errors (the AV1 decoder's, the composition layer's) as
/// this crate's.
pub(crate) fn from_core_err(e: oxideav_core::Error) -> Error {
    match e {
        oxideav_core::Error::InvalidData(s) => Error::InvalidData(s),
        oxideav_core::Error::Unsupported(s) => Error::Unsupported(s),
        oxideav_core::Error::ResourceExhausted(s) => Error::LimitExceeded(s),
        oxideav_core::Error::Io(e) => Error::Io(e),
        other => Error::InvalidData(other.to_string()),
    }
}

// ---------------------------------------------------------------------
// Framework frame conversions
// ---------------------------------------------------------------------

/// The picture as a framework frame: the planes as given, the colour
/// as the frame's `ColorSignal` side channel, and — for `Ya16Le`,
/// whose words are wider than the coded depth — the depth on the
/// significant-bits side channel.
impl From<AvifImage> for VideoFrame {
    fn from(img: AvifImage) -> Self {
        let signal = color_signal_for(Some(&img.color.to_colr()));
        let mut vf = VideoFrame {
            pts: None,
            planes: img
                .planes
                .into_iter()
                .map(|p| VideoPlane {
                    stride: p.stride,
                    data: p.data,
                })
                .collect(),
        };
        vf.set_color_signal(signal);
        if img.format == AvifPixelFormat::Ya16Le && img.bit_depth < 16 {
            vf.set_significant_bits(vec![img.bit_depth]);
        }
        vf
    }
}

impl From<ColorSignal> for ColorInfo {
    fn from(s: ColorSignal) -> Self {
        let range = match s.range {
            oxideav_core::ColorRange::Full => ColorRange::Full,
            oxideav_core::ColorRange::Limited => ColorRange::Limited,
            _ => ColorRange::Unspecified,
        };
        ColorInfo::new(range, s.primaries.0, s.transfer.0, s.matrix.0)
    }
}

/// The storage layout behind a framework pixel-format label, plus what
/// the label itself says about the colour: the `YuvJ*` family is full
/// range, the `Gbrp*` / `Gbrap*` family the identity matrix (planes
/// G B R [A], exactly this crate's 4:4:4 layout).
fn storage_layout_of(
    fmt: oxideav_core::PixelFormat,
) -> Result<(AvifPixelFormat, Option<ColorRange>, bool)> {
    use oxideav_core::PixelFormat as P;
    Ok(match fmt {
        P::YuvJ420P => (AvifPixelFormat::Yuv420P, Some(ColorRange::Full), false),
        P::YuvJ422P => (AvifPixelFormat::Yuv422P, Some(ColorRange::Full), false),
        P::YuvJ444P => (AvifPixelFormat::Yuv444P, Some(ColorRange::Full), false),
        P::Gbrp8 => (AvifPixelFormat::Yuv444P, None, true),
        P::Gbrap8 => (AvifPixelFormat::Yuva444P, None, true),
        P::Gbrp10Le => (AvifPixelFormat::Yuv444P10Le, None, true),
        P::Gbrap10Le => (AvifPixelFormat::Yuva444P10Le, None, true),
        P::Gbrp12Le => (AvifPixelFormat::Yuv444P12Le, None, true),
        P::Gbrap12Le => (AvifPixelFormat::Yuva444P12Le, None, true),
        other => (AvifPixelFormat::try_from(other)?, None, false),
    })
}

impl AvifImage {
    /// A picture from a framework frame plus the parameters that carry
    /// what the frame does not: `width`, `height` and `pixel_format`
    /// (the storage layouts by name, plus the `YuvJ*` full-range and
    /// `Gbrp*` / `Gbrap*` identity-matrix labels this crate's decoder
    /// emits). Only the image planes are copied (a side-channel record
    /// is not a plane); `color` is the frame's `ColorSignal`, else the
    /// parameters' stream-level one, else (fully unspecified) the MIAF
    /// default, with the label's own range / matrix applied; the frame's
    /// significant-bits channel sets the depth of a `Ya16Le` picture.
    pub fn from_video_frame(frame: &VideoFrame, params: &CodecParameters) -> Result<Self> {
        let width = params
            .width
            .ok_or_else(|| Error::invalid("avif: CodecParameters.width required"))?;
        let height = params
            .height
            .ok_or_else(|| Error::invalid("avif: CodecParameters.height required"))?;
        let label = params
            .pixel_format
            .ok_or_else(|| Error::invalid("avif: CodecParameters.pixel_format required"))?;
        let (format, label_range, identity) = storage_layout_of(label)?;
        // The frame's own record refines the stream-level signal; an
        // entirely unspecified signal means the MIAF default.
        let signal = frame.color_signal().unwrap_or(params.color_signal);
        let mut color = if signal.is_unspecified() {
            ColorInfo::default()
        } else {
            ColorInfo::from(signal)
        };
        if let Some(range) = label_range {
            color.range = range;
        }
        if identity {
            color.matrix = 0;
        }
        if color.range == ColorRange::Unspecified {
            color.range = ColorRange::Full;
        }
        let bit_depth = match frame.significant_bits() {
            Some(bits) if format == AvifPixelFormat::Ya16Le && !bits.is_empty() => bits[0],
            _ => format.bit_depth(),
        };
        let planes = core_to_avif_frame(frame.clone()).planes;
        Ok(Self::new(width, height, format, planes)?
            .with_color(color)
            .with_bit_depth(bit_depth))
    }
}

impl TryFrom<(&VideoFrame, &CodecParameters)> for AvifImage {
    type Error = Error;

    fn try_from((frame, params): (&VideoFrame, &CodecParameters)) -> Result<Self> {
        AvifImage::from_video_frame(frame, params)
    }
}

// ---------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------

/// A decoded primary picture with the container-side description the
/// framework decoder also reports.
pub(crate) struct Decoded {
    pub image: AvifImage,
    pub info: AvifInfo,
}

/// `strict` conformance refusals (see [`DecodeOptions::strict`]).
fn strict_checks(brands: &BrandClass, mif1: &crate::derived::Mif1Compliance) -> Result<()> {
    if !(brands.is_image || brands.is_sequence || brands.is_intra_only) {
        return Err(Error::invalid(
            "avif (strict): ftyp declares no AVIF brand (avif / avis / avio, av1-avif §6)",
        ));
    }
    if mif1.claims_mif1
        && !(mif1.has_hdlr && mif1.has_pitm && mif1.has_iinf && mif1.has_iloc && mif1.has_iprp)
    {
        return Err(Error::invalid(
            "avif (strict): file claims mif1 but lacks a HEIF §10.2.1.1 mandatory box",
        ));
    }
    Ok(())
}

/// Decode the primary item's output image with its colour and
/// metadata — the one implementation behind [`decode_with`] and
/// [`crate::AvifDecoder::decode_file`].
pub(crate) fn decode_primary(file: &[u8], opts: &DecodeOptions) -> Result<Decoded> {
    opts.check_bytes(file.len())?;
    let hdr = parse_header(file)?;
    let primary_id = hdr
        .meta
        .primary_item_id
        .ok_or_else(|| Error::invalid("avif: missing pitm"))?;
    let primary_info = hdr
        .meta
        .item_by_id(primary_id)
        .ok_or_else(|| Error::invalid("avif: pitm references unknown item"))?
        .clone();
    let brands = classify_brands(&hdr.major_brand, &hdr.compatible_brands)?;
    let mif1 = audit_mif1(file)?;
    if opts.strict {
        strict_checks(&brands, &mif1)?;
    }
    let info = if primary_info.item_type == ITEM_TYPE_GRID {
        build_info_grid(&hdr, primary_id, brands, mif1)?
    } else if primary_info.item_type == ITEM_TYPE_AV01 {
        let img = parse(file)?;
        let has_alpha = crate::alpha::find_alpha_item_id(&hdr.meta, primary_id).is_some();
        build_info(&img, has_alpha, brands, mif1, file)?
    } else if primary_info.item_type == ITEM_TYPE_IOVL
        || primary_info.item_type == ITEM_TYPE_IDEN
        || primary_info.item_type == ITEM_TYPE_TMAP
    {
        build_info_derived(&hdr, primary_id, brands, mif1)?
    } else {
        return Err(Error::unsupported(format!(
            "avif: primary item type '{}' not supported",
            String::from_utf8_lossy(&primary_info.item_type)
        )));
    };
    // Limits before any pixel allocation.
    opts.check_dims(info.width, info.height)?;
    let image = picture_of(
        &hdr,
        primary_id,
        opts,
        info.colour.as_ref(),
        (info.exif_item_id, info.xmp_item_id),
    )?;
    Ok(Decoded { image, info })
}

/// The output image of `item_id` (HEIF §6.3) as an [`AvifImage`]:
/// derived-item composition, alpha, transforms, then colour and
/// metadata. `fallback_colour` is the container-resolved `colr` of a
/// derived item without its own; `meta_items` the Exif / XMP item ids
/// attached to it.
fn picture_of(
    hdr: &AvifHeader<'_>,
    item_id: u32,
    opts: &DecodeOptions,
    fallback_colour: Option<&Colr>,
    meta_items: (Option<u32>, Option<u32>),
) -> Result<AvifImage> {
    let is_tmap = hdr
        .meta
        .item_by_id(item_id)
        .map(|i| i.item_type == ITEM_TYPE_TMAP)
        .unwrap_or(false);
    let mut ctx = DecodeCtx::new(opts);
    // Gain-map application: the `tmap` alternative of the item (the
    // item itself when it is one) replaces it.
    let target = if opts.tone_mapped {
        tone_map_of(hdr, item_id).unwrap_or(item_id)
    } else {
        item_id
    };
    let composed: HeifFrame =
        decode_item_output(hdr, target, 0, &mut ctx, true).map_err(from_core_err)?;
    opts.check_dims(composed.width, composed.height)?;
    let bit_depth = composed.format.bit_depth;
    let (frame, format) = from_heif(&composed)?;
    // The output image's colour: the item's own `nclx` `colr` (a
    // reconstructed `tmap` carries the `tmap` item's — the alternate
    // colorimetry, HEIF Amd 1 §6.6.2.4.1), else what the container
    // resolved for a derived item (its first input's), else the MIAF
    // §7.3.6.4 default.
    let (nclx, icc) = item_colrs(&hdr.meta, target);
    let colour = if target != item_id || is_tmap {
        nclx.or_else(|| nclx_of(fallback_colour))
    } else {
        nclx.or_else(|| nclx_of(fallback_colour))
    };
    let icc = icc.or_else(|| match fallback_colour {
        Some(Colr::Icc(bytes)) => Some(bytes.clone()),
        _ => None,
    });
    let exif = meta_items
        .0
        .and_then(|id| hdr.item_data(id).ok())
        .and_then(|body| exif_tiff_bytes(&body));
    let xmp = meta_items
        .1
        .and_then(|id| hdr.item_data(id).ok())
        .map(|b| b.into_owned());
    Ok(AvifImage {
        width: composed.width,
        height: composed.height,
        format,
        planes: frame.planes,
        color: ColorInfo::from_colr(colour.as_ref()),
        metadata: Metadata::new(icc, exif, xmp, None),
        bit_depth,
    })
}

fn nclx_of(colr: Option<&Colr>) -> Option<Colr> {
    match colr {
        Some(c @ Colr::Nclx { .. }) => Some(c.clone()),
        _ => None,
    }
}

/// `true` when the file is an `avis` image sequence with a sample
/// table (the brand says so and a `moov` is present).
pub(crate) fn is_sequence_file(bytes: &[u8]) -> bool {
    let Ok(Some((ftyp_payload, _))) = crate::box_parser::find_box(bytes, b"ftyp") else {
        return false;
    };
    let Ok((major, _minor, compat)) = crate::parser::parse_ftyp(ftyp_payload) else {
        return false;
    };
    let Ok(brands) = classify_brands(&major, &compat) else {
        return false;
    };
    (brands.is_sequence || brands.has_msf1) && has_moov(bytes)
}

/// What a sequence decode established before its first frame.
pub(crate) struct SequenceDecode {
    /// The `av01` sample entry's `colr` as a signal (MIAF default
    /// when absent).
    pub signal: ColorSignal,
    /// The storage layout the samples decode to, from the `av1C`.
    pub layout: Option<AvifPixelFormat>,
    /// The parsed `av1C` record.
    pub config: Av1CodecConfig,
}

/// Decode every sample of an `avis` image sequence (av1-avif §6.3 +
/// ISO/IEC 14496-12 §8) through one shared AV1 decoder so inter-frame
/// state is preserved, handing each decoded frame to `sink` with the
/// duration of the sample that produced it. `limit` stops after that
/// many frames. The one implementation behind [`decode_all`] and
/// [`crate::AvifDecoder::decode_avis_file`].
pub(crate) fn decode_sequence_with<F>(
    file: &[u8],
    opts: &DecodeOptions,
    limit: Option<usize>,
    mut sink: F,
) -> Result<SequenceDecode>
where
    F: FnMut(oxideav_core::Frame, &SequenceDecode, Duration) -> Result<()>,
{
    opts.check_bytes(file.len())?;
    let meta = parse_avis(file)?;
    if meta.samples.is_empty() {
        return Err(Error::invalid("avis: track has zero samples"));
    }
    let av1c = meta.av1_codec_config.clone().ok_or_else(|| {
        Error::invalid(
            "avis: track stsd → av01 → av1C is missing — cannot seed AV1 decoder \
             (av1-avif §2.2.1)",
        )
    })?;
    // Eagerly validate the codec config — same shape as the
    // still-image path uses for the av1C item property. 10/12-bit
    // tracks pass straight through: AVIS sample frames are handed
    // out exactly as the AV1 decoder emits them (little-endian
    // 16-bit words for a `high_bitdepth` track), no composition
    // step involved.
    let cfg = parse_av1c(&av1c).map_err(from_core_err)?;
    validate_av1_config(&cfg).map_err(from_core_err)?;
    if let Some((w, h)) = meta.display_dims {
        opts.check_dims(w, h)?;
    }
    let timescale = if meta.timescale == 0 {
        1
    } else {
        meta.timescale
    };
    let media_timescale = meta.media_timescale.filter(|&t| t > 0).unwrap_or(timescale);
    let seq = SequenceDecode {
        signal: color_signal_for(meta.colr.as_ref()),
        layout: layout_of_record(&cfg),
        config: cfg,
    };
    let mut params = CodecParameters::video(CodecId::new("av1"));
    if let Some((w, h)) = meta.display_dims {
        params.width = Some(w);
        params.height = Some(h);
    }
    params.extradata = av1c;

    let mut av1 = oxideav_av1::registry::make_decoder(&params).map_err(from_core_err)?;
    let mut delivered = 0usize;
    let done = |delivered: usize| limit.is_some_and(|n| delivered >= n);
    let mut cumulative_pts: u64 = 0;
    let mut last_duration = Duration::ZERO;
    for (i, s) in meta.samples.iter().enumerate() {
        if done(delivered) {
            break;
        }
        let bytes = sample_bytes(file, s)?;
        if bytes.len() > MAX_AV1_ITEM_BYTES {
            return Err(Error::invalid(format!(
                "avis: sample {i} payload {} bytes exceeds soft cap {} bytes",
                bytes.len(),
                MAX_AV1_ITEM_BYTES
            )));
        }
        // Build a packet on stream 0 with the movie timescale so
        // the framework consumer recovers presentation order
        // without an extra remapping step.
        let pkt = Packet::new(0, TimeBase::new(1, timescale as i64), bytes.to_vec())
            .with_pts(cumulative_pts as i64);
        cumulative_pts = cumulative_pts.saturating_add(s.duration as u64);
        last_duration = Duration::from_secs_f64(f64::from(s.duration) / f64::from(media_timescale));
        av1.send_packet(&pkt).map_err(|e| {
            Error::invalid(format!(
                "avis: av1 decoder rejected sample {i} (offset={}, size={}, sync={}): {e}",
                s.offset, s.size, s.is_sync
            ))
        })?;
        // Drain frames after every packet — most AV1 packets emit a
        // single decoded frame, but show-existing-frame OBUs can
        // produce zero, and a single packet can occasionally yield
        // more than one display frame.
        loop {
            match av1.receive_frame() {
                Ok(frame) => {
                    sink(frame, &seq, last_duration)?;
                    delivered += 1;
                    if done(delivered) {
                        break;
                    }
                }
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => return Err(from_core_err(e)),
            }
        }
    }
    if !done(delivered) {
        // Flush any frames the decoder buffered past the last packet
        // (re-ordering in standard AV1 is rare for AVIS, but the trait
        // contract requires the call).
        let _ = av1.flush();
        while let Ok(frame) = av1.receive_frame() {
            sink(frame, &seq, last_duration)?;
            delivered += 1;
            if done(delivered) {
                break;
            }
        }
    }
    Ok(seq)
}

/// A sequence sample's decoded frame as a picture: geometry inferred
/// from the plane layout at the record's depth, the sample entry's
/// colour.
fn sequence_picture(
    frame: oxideav_core::Frame,
    seq: &SequenceDecode,
    opts: &DecodeOptions,
) -> Result<AvifImage> {
    let vf = match frame {
        oxideav_core::Frame::Video(v) => v,
        other => {
            return Err(Error::unsupported(format!(
                "avis: AV1 decoder returned a non-video frame: {other:?}"
            )))
        }
    };
    let (fmt_core, w, h) = infer_av1_pixmap(&vf, &seq.config).map_err(from_core_err)?;
    opts.check_dims(w, h)?;
    let format = AvifPixelFormat::try_from(fmt_core)?;
    let planes = core_to_avif_frame(vf).planes;
    Ok(AvifImage::new(w, h, format, planes)?.with_color(ColorInfo::from(seq.signal)))
}

/// Decode the primary picture with [`DecodeOptions::default`] — a
/// still's primary item (coded or derived, alpha composited,
/// transforms applied) or the first sample of an `avis` sequence.
pub fn decode(bytes: &[u8]) -> Result<AvifImage> {
    decode_with(bytes, &DecodeOptions::default())
}

/// [`decode`] with explicit limits / strictness / gain-map and layer
/// choices.
pub fn decode_with(bytes: &[u8], opts: &DecodeOptions) -> Result<AvifImage> {
    opts.check_bytes(bytes.len())?;
    if is_sequence_file(bytes) {
        let mut first = None;
        decode_sequence_with(bytes, opts, Some(1), |frame, seq, _| {
            first = Some(sequence_picture(frame, seq, opts)?);
            Ok(())
        })?;
        return first.ok_or_else(|| Error::invalid("avis: the sequence decoded to no frame"));
    }
    decode_primary(bytes, opts).map(|d| d.image)
}

/// The primary picture as tightly packed 8-bit RGB (alpha dropped).
pub fn decode_rgb8(bytes: &[u8]) -> Result<RgbImage> {
    let img = decode(bytes)?;
    let data = img.try_to_rgb8()?;
    Ok(RgbImage::new(img.width, img.height, data))
}

/// The primary picture as tightly packed 8-bit RGBA (alpha opaque
/// when the file has none).
pub fn decode_rgba8(bytes: &[u8]) -> Result<RgbaImage> {
    let img = decode(bytes)?;
    let data = img.try_to_rgba8()?;
    Ok(RgbaImage::new(img.width, img.height, data))
}

/// Every picture of the file: the samples of an `avis` image sequence
/// (each with its display `delay`), the entities of the primary's
/// `brst` image-burst group (HEIF §6.8.9), else the primary alone.
pub fn decode_all(bytes: &[u8]) -> Result<Vec<Frame>> {
    decode_all_with(bytes, &DecodeOptions::default())
}

/// [`decode_all`] with explicit options.
pub fn decode_all_with(bytes: &[u8], opts: &DecodeOptions) -> Result<Vec<Frame>> {
    opts.check_bytes(bytes.len())?;
    if is_sequence_file(bytes) {
        let mut frames = Vec::new();
        decode_sequence_with(bytes, opts, None, |frame, seq, delay| {
            frames.push(Frame::new(sequence_picture(frame, seq, opts)?, Some(delay)));
            Ok(())
        })?;
        return Ok(frames);
    }
    let decoded = decode_primary(bytes, opts)?;
    let hdr = parse_header(bytes)?;
    let primary_id = hdr
        .meta
        .primary_item_id
        .ok_or_else(|| Error::invalid("avif: missing pitm"))?;
    let burst = burst_members(&hdr.meta, primary_id);
    if burst.len() < 2 {
        return Ok(vec![Frame::new(decoded.image, None)]);
    }
    let mut frames = Vec::with_capacity(burst.len());
    for id in burst {
        let image = if id == primary_id {
            decoded.image.clone()
        } else {
            picture_of(&hdr, id, opts, None, (None, None))?
        };
        frames.push(Frame::new(image, None));
    }
    Ok(frames)
}

/// [`decode`] from a reader (read to end first — AVIF item offsets are
/// absolute, so the whole file is needed).
pub fn decode_from<R: Read>(mut r: R) -> Result<AvifImage> {
    let mut bytes = Vec::new();
    r.read_to_end(&mut bytes)?;
    decode(&bytes)
}

// ---------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------

/// Samples of one plane as `u16` values: `rows` rows of `cols` samples
/// at `bps` bytes each (little-endian words for 2), honouring the
/// plane's stride.
fn plane_samples(
    plane: &AvifPlane,
    cols: usize,
    rows: usize,
    bps: usize,
    label: &str,
) -> Result<Vec<u16>> {
    let row_bytes = cols * bps;
    if rows > 0
        && (plane.stride < row_bytes || plane.data.len() < plane.stride * (rows - 1) + row_bytes)
    {
        return Err(Error::invalid(format!(
            "avif encode: {label} plane too short ({} bytes, stride {}, need {rows} rows of {row_bytes})",
            plane.data.len(),
            plane.stride
        )));
    }
    let mut out = Vec::with_capacity(cols * rows);
    for r in 0..rows {
        let row = &plane.data[r * plane.stride..r * plane.stride + row_bytes];
        if bps == 1 {
            out.extend(row.iter().map(|&b| u16::from(b)));
        } else {
            out.extend(
                row.chunks_exact(2)
                    .map(|w| u16::from_le_bytes([w[0], w[1]])),
            );
        }
    }
    Ok(out)
}

/// The still-encoder input for a picture: planes widened to `u16`,
/// the `nclx` of its colour, metadata per the options.
fn still_of(image: &AvifImage, opts: &EncodeOptions) -> Result<StillImage> {
    if image.width == 0 || image.height == 0 {
        return Err(Error::invalid(
            "avif encode: dimensions must be at least 1x1",
        ));
    }
    let format = image.format;
    if image.planes.len() != format.plane_count() {
        return Err(Error::invalid(format!(
            "avif encode: {format:?} needs {} planes, image carries {}",
            format.plane_count(),
            image.planes.len()
        )));
    }
    let bit_depth = image.bit_depth;
    if !matches!(bit_depth, 8 | 10 | 12) {
        return Err(Error::unsupported(format!(
            "avif encode: {bit_depth}-bit samples — AV1 codes 8, 10 or 12"
        )));
    }
    if format != AvifPixelFormat::Ya16Le && format.bit_depth() != bit_depth {
        return Err(Error::invalid(format!(
            "avif encode: bit_depth {bit_depth} does not match the {format:?} layout"
        )));
    }
    let (w, h) = (image.width as usize, image.height as usize);
    let bps = format.bytes_per_sample();
    let mono = format.plane_count() == 1 || format.is_packed_ya();
    let (sx, sy) = format.chroma_subsampling();
    let chroma = if mono {
        StillChroma::Monochrome
    } else {
        match (sx, sy) {
            (1, 1) => StillChroma::Yuv420,
            (1, 0) => StillChroma::Yuv422,
            _ => StillChroma::Yuv444,
        }
    };
    if (sx == 1 && image.width % 2 != 0) || (sy == 1 && image.height % 2 != 0) {
        return Err(Error::unsupported(format!(
            "avif encode: {format:?} at {}x{} — this encoder codes sub-sampled chroma only at \
             even extents; use 4:4:4 (or `encode_rgb8`, which falls back to it)",
            image.width, image.height
        )));
    }
    let (y, u, v, alpha) = if format.is_packed_ya() {
        let packed = plane_samples(&image.planes[0], w * 2, h, bps, "packed YA")?;
        let mut y = Vec::with_capacity(w * h);
        let mut a = Vec::with_capacity(w * h);
        for px in packed.chunks_exact(2) {
            y.push(px[0]);
            a.push(px[1]);
        }
        (y, Vec::new(), Vec::new(), Some(a))
    } else {
        let y = plane_samples(&image.planes[0], w, h, bps, "Y")?;
        let (u, v) = if mono {
            (Vec::new(), Vec::new())
        } else {
            let cw = w.div_ceil(1 << sx);
            let ch = h.div_ceil(1 << sy);
            (
                plane_samples(&image.planes[1], cw, ch, bps, "Cb")?,
                plane_samples(&image.planes[2], cw, ch, bps, "Cr")?,
            )
        };
        let alpha = if format.has_alpha() {
            let idx = image.planes.len() - 1;
            Some(plane_samples(&image.planes[idx], w, h, bps, "alpha")?)
        } else {
            None
        };
        (y, u, v, alpha)
    };
    let mut still = StillImage::yuv(image.width, image.height, bit_depth, chroma, y, u, v)?;
    if let Some(a) = alpha {
        still = still.with_alpha(a)?;
    }
    still = still.with_colr(image.color.to_colr());
    let mut props = StillProperties::default();
    if opts.embed_exif {
        props.exif = image.metadata.exif.as_deref().map(exif_item_body);
    }
    if opts.embed_xmp {
        props.xmp = image.metadata.xmp.clone();
    }
    if opts.embed_icc {
        props.icc = image.metadata.icc.clone();
    }
    Ok(still.with_props(props))
}

/// Encode `frames` as one file — the mirror of [`decode_all`]. Frames
/// without a `delay` are coded as image items: one is exactly
/// [`encode`], several become the primary plus a `brst` image-burst
/// group ([`crate::encode_still_burst`]; each member with its own
/// colour, ICC, alpha auxiliary and `clap`, Exif / XMP on the primary).
/// Frames with a `delay` become the samples of an `avis` image
/// sequence ([`crate::encode_sequence`] at timescale 1000 with each
/// frame's delay in milliseconds, `opts.base_q_idx` as the quality);
/// every frame must then carry a delay, since an `avis` file holds
/// samples only, and the sequence encoder carries no alpha
/// (`Error::Unsupported`). All frames share frame 0's geometry, depth
/// and chroma layout.
///
/// `decode_all(encode_all(frames)) == frames` holds for planes and
/// colour at the lossless default (`base_q_idx = 0`); burst members
/// read back without the primary's Exif / XMP, sequence samples with
/// their delays.
pub fn encode_all(frames: &[Frame], opts: &EncodeOptions) -> Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(Error::invalid("avif encode_all: no frames"));
    }
    let timed = frames.iter().filter(|f| f.delay.is_some()).count();
    if timed == 0 {
        if let [single] = frames {
            return encode(&single.image, opts);
        }
        let stills: Vec<StillImage> = frames
            .iter()
            .map(|f| still_of(&f.image, opts))
            .collect::<Result<_>>()?;
        return crate::still::encode_still_burst(&stills, opts);
    }
    if timed != frames.len() {
        return Err(Error::invalid(
            "avif encode_all: an avis image sequence holds timed samples only — give every \
             frame a delay, or none for an image burst",
        ));
    }
    let stills: Vec<StillImage> = frames
        .iter()
        .map(|f| still_of(&f.image, opts))
        .collect::<Result<_>>()?;
    let durations: Vec<u32> = frames
        .iter()
        .map(|f| {
            f.delay
                .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX))
                .unwrap_or(0)
        })
        .collect();
    let seq_opts = crate::sequence::SequenceEncodeOptions::default()
        .with_timescale(1000)
        .with_base_q_idx(opts.base_q_idx);
    crate::sequence::encode_sequence_timed(&stills, &seq_opts, Some(&durations))
}

/// Encode a picture as it is: its layout and depth (8 / 10 / 12-bit
/// 4:2:0 / 4:2:2 / 4:4:4 / monochrome, alpha as the AV1-coded alpha
/// auxiliary item, an identity-matrix picture as the RGB it holds),
/// its colour as the `colr` `nclx`, its metadata as `Exif` / XMP
/// items and an ICC `colr` (per [`EncodeOptions`]). A canvas beyond
/// 4096 coded pixels per axis is tiled as a `grid`. Lossless at the
/// default `base_q_idx = 0`. [`Error::Unsupported`] for a layout this
/// encoder does not carry (sub-sampled chroma at odd extents), never a
/// silent conversion.
pub fn encode(image: &AvifImage, opts: &EncodeOptions) -> Result<Vec<u8>> {
    let still = still_of(image, opts)?;
    encode_still_auto(&still, opts)
}

/// RGB(A) bytes as the still-encoder input at the MIAF default colour
/// (BT.709 primaries, sRGB transfer, BT.601 matrix, full range):
/// converted to 4:4:4 Y′CbCr exactly (H.273 §8.3, round-to-nearest),
/// then the chroma planes box-filtered to the requested layout
/// (`(a + b + c + d + 2) >> 2` per 2×2 for 4:2:0, `(a + b + 1) >> 1`
/// per pair for 4:2:2). Odd extents fall back to 4:4:4, which AV1
/// sub-sampling cannot represent at this encoder's even-extent rule.
fn still_of_rgb(
    width: u32,
    height: u32,
    data: &[u8],
    channels: usize,
    opts: &EncodeOptions,
) -> Result<StillImage> {
    let n = width as usize * height as usize;
    if width == 0 || height == 0 {
        return Err(Error::invalid(
            "avif encode: dimensions must be at least 1x1",
        ));
    }
    if data.len() != n * channels {
        return Err(Error::invalid(format!(
            "avif encode: {width}x{height} with {channels} channels needs {} bytes, got {}",
            n * channels,
            data.len()
        )));
    }
    let color = ColorInfo::miaf_default();
    let colr = oxideav_heif::props::Colr::from(&color.to_colr());
    let rgb = oxideav_heif::rgb::RgbImage16::new(
        width,
        height,
        channels,
        8,
        data.iter().map(|&b| u16::from(b)).collect(),
    );
    let mut chroma = opts.chroma;
    let (sx, sy) = chroma.subsampling();
    if (sx == 1 && width % 2 != 0) || (sy == 1 && height % 2 != 0) {
        chroma = StillChroma::Yuv444;
    }
    let target = if chroma == StillChroma::Monochrome {
        Chroma::Mono
    } else {
        Chroma::Yuv444
    };
    let frame = oxideav_heif::rgb::from_rgb(&rgb, Some(&colr), target)?;
    let (w, h) = (width as usize, height as usize);
    let heif_plane = |idx: usize| -> Result<Vec<u16>> {
        let p = frame
            .planes
            .get(idx)
            .ok_or_else(|| Error::invalid("avif encode: RGB conversion returned too few planes"))?;
        plane_samples(
            &AvifPlane::new(p.stride, p.data.clone()),
            w,
            h,
            1,
            "converted",
        )
    };
    let y = heif_plane(0)?;
    let (u, v) = match chroma {
        StillChroma::Monochrome => (Vec::new(), Vec::new()),
        StillChroma::Yuv444 => (heif_plane(1)?, heif_plane(2)?),
        StillChroma::Yuv420 => (
            box_filter(&heif_plane(1)?, w, h, 1, 1),
            box_filter(&heif_plane(2)?, w, h, 1, 1),
        ),
        StillChroma::Yuv422 => (
            box_filter(&heif_plane(1)?, w, h, 1, 0),
            box_filter(&heif_plane(2)?, w, h, 1, 0),
        ),
    };
    let mut still = StillImage::yuv(width, height, 8, chroma, y, u, v)?;
    if channels == 4 {
        let a: Vec<u16> = data.chunks_exact(4).map(|px| u16::from(px[3])).collect();
        still = still.with_alpha(a)?;
    }
    Ok(still.with_colr(color.to_colr()))
}

/// Average a full-resolution plane down by `2^sx × 2^sy` with
/// round-half-up; extents must be even on each sub-sampled axis.
fn box_filter(plane: &[u16], w: usize, h: usize, sx: u32, sy: u32) -> Vec<u16> {
    let (cw, ch) = (w >> sx, h >> sy);
    let count = 1u32 << (sx + sy);
    let mut out = Vec::with_capacity(cw * ch);
    for cy in 0..ch {
        for cx in 0..cw {
            let mut sum = 0u32;
            for dy in 0..(1usize << sy) {
                for dx in 0..(1usize << sx) {
                    sum += u32::from(plane[(cy * (1 << sy) + dy) * w + cx * (1 << sx) + dx]);
                }
            }
            out.push(((sum + count / 2) / count) as u16);
        }
    }
    out
}

/// Encode tightly packed 8-bit RGB as an AVIF at the MIAF default
/// colour signalling (BT.709 / sRGB / BT.601, full range) in
/// `opts.chroma` — 4:2:0 by default, 4:4:4 at odd extents (see
/// [`EncodeOptions::chroma`]); `opts.base_q_idx` / [`EncodeOptions::with_quality`]
/// set the quality. For byte-exact RGB use
/// `encode(&AvifImage::from_rgb8(..).unwrap(), ..)`, which codes the identity
/// matrix in 4:4:4.
pub fn encode_rgb8(width: u32, height: u32, rgb: &[u8], opts: &EncodeOptions) -> Result<Vec<u8>> {
    let still = still_of_rgb(width, height, rgb, 3, opts)?;
    encode_still_auto(&still, opts)
}

/// [`encode_rgb8`] for tightly packed 8-bit RGBA: the alpha channel
/// becomes the AV1-coded alpha auxiliary item (av1-avif §4.1, coded
/// at `opts.alpha_q_idx`, `prem` signalled per
/// `opts.premultiplied_alpha`).
pub fn encode_rgba8(width: u32, height: u32, rgba: &[u8], opts: &EncodeOptions) -> Result<Vec<u8>> {
    let still = still_of_rgb(width, height, rgba, 4, opts)?;
    encode_still_auto(&still, opts)
}

/// [`encode`] into a writer.
pub fn encode_to<W: Write>(image: &AvifImage, opts: &EncodeOptions, mut w: W) -> Result<()> {
    let bytes = encode(image, opts)?;
    w.write_all(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{PixelFormat, Plane};
    use crate::info;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn encode_all_mirrors_decode_all() {
        use std::time::Duration;
        let pic = |seed: u8, alpha: bool| -> AvifImage {
            let n = 16 * 8;
            if alpha {
                let rgba: Vec<u8> = (0..n * 4)
                    .map(|i| ((i * 31 + seed as usize * 17) % 251) as u8)
                    .collect();
                AvifImage::from_rgba8(16, 8, rgba).unwrap()
            } else {
                let rgb: Vec<u8> = (0..n * 3)
                    .map(|i| ((i * 29 + seed as usize * 13) % 251) as u8)
                    .collect();
                AvifImage::from_rgb8(16, 8, rgb).unwrap()
            }
        };
        let opts = EncodeOptions::default();

        // One delay-less frame is exactly `encode`.
        let one = pic(1, false);
        assert_eq!(
            encode_all(&[Frame::new(one.clone(), None)], &opts).unwrap(),
            encode(&one, &opts).unwrap()
        );

        // Several delay-less frames: a `brst` burst, lossless, with
        // each member's alpha; the primary is frame 0.
        let burst = vec![
            Frame::new(pic(1, true), None),
            Frame::new(pic(2, true), None),
            Frame::new(pic(3, true), None),
        ];
        let bytes = encode_all(&burst, &opts).unwrap();
        assert_eq!(info(&bytes).unwrap().frames, 3);
        assert_eq!(decode(&bytes).unwrap(), burst[0].image);
        assert_eq!(decode_all(&bytes).unwrap(), burst);

        // Timed frames: an `avis` sequence with the delays in ms.
        let seq = vec![
            Frame::new(pic(4, false), Some(Duration::from_millis(40))),
            Frame::new(pic(5, false), Some(Duration::from_millis(1500))),
            Frame::new(pic(6, false), Some(Duration::from_millis(1))),
        ];
        let bytes = encode_all(&seq, &opts).unwrap();
        assert!(is_sequence_file(&bytes));
        assert_eq!(info(&bytes).unwrap().frames, 3);
        let back = decode_all(&bytes).unwrap();
        assert_eq!(back.len(), 3);
        for (i, (got, want)) in back.iter().zip(&seq).enumerate() {
            assert_eq!(got.delay, want.delay, "sample {i} delay");
            assert_eq!(got.image.planes, want.image.planes, "sample {i} planes");
            assert_eq!(got.image.format, want.image.format, "sample {i} layout");
            assert_eq!(got.image.color, want.image.color, "sample {i} colour");
        }

        // Rejections: nothing; mixed timed / untimed; alpha in a sequence.
        assert!(matches!(encode_all(&[], &opts), Err(Error::InvalidData(_))));
        let mixed = [seq[0].clone(), burst[0].clone()];
        assert!(matches!(
            encode_all(&mixed, &opts),
            Err(Error::InvalidData(_))
        ));
        let timed_alpha = [
            Frame::new(pic(1, true), Some(Duration::from_millis(10))),
            Frame::new(pic(2, true), Some(Duration::from_millis(10))),
        ];
        assert!(matches!(
            encode_all(&timed_alpha, &opts),
            Err(Error::Unsupported(_))
        ));
        // Fallible constructors.
        assert!(matches!(
            AvifImage::from_rgb8(2, 2, vec![0; 11]),
            Err(Error::InvalidData(_))
        ));
        assert!(matches!(
            AvifImage::from_rgba8(0, 2, vec![]),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn identity_rgb_fixture_decodes_to_the_pinned_bytes() {
        let bytes = fixture("identity_rgb_lossless.avif");
        let expect = fixture("identity_rgb_lossless.rgb");
        let img = decode(&bytes).unwrap();
        assert_eq!(img.format(), PixelFormat::Yuv444P);
        assert!(img.color.is_identity_matrix());
        assert_eq!(img.to_rgb8(), expect);
        let rgb = decode_rgb8(&bytes).unwrap();
        assert_eq!((rgb.width, rgb.height), (img.width, img.height));
        assert_eq!(rgb.data, expect);
        let rgba = decode_rgba8(&bytes).unwrap();
        assert_eq!(rgba.data.len(), expect.len() / 3 * 4);
        assert!(rgba.data.chunks_exact(4).all(|p| p[3] == 255));
        let from_reader = decode_from(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(from_reader, img);
    }

    #[test]
    fn lossless_round_trip_is_exact_for_planes_colour_and_metadata() {
        let rgba: Vec<u8> = (0..16 * 8 * 4).map(|i| (i * 31 % 251) as u8).collect();
        let img = AvifImage::from_rgba8(16, 8, rgba.clone())
            .unwrap()
            .with_metadata(Metadata::new(
                Some(vec![0x11; 8]),
                Some(b"MM\0\x2a\0\0\0\x08".to_vec()),
                Some(b"<x:xmpmeta/>".to_vec()),
                None,
            ));
        let bytes = encode(&img, &EncodeOptions::default()).unwrap();
        assert!(crate::probe(&bytes));
        let info = crate::info(&bytes).unwrap();
        assert!(info.has_alpha && info.has_exif && info.has_xmp && info.has_icc);
        assert_eq!(info.format, PixelFormat::Yuva444P);
        assert!(info.color.is_identity_matrix());
        let back = decode(&bytes).unwrap();
        assert_eq!(back.width, 16);
        assert_eq!(back.height, 8);
        assert_eq!(back.format, img.format);
        assert_eq!(back.planes, img.planes);
        assert_eq!(back.color, img.color);
        assert_eq!(back.metadata, img.metadata);
        assert_eq!(back.to_rgba8(), rgba);
        // Metadata embedding can be switched off per block.
        let bare = encode(
            &img,
            &EncodeOptions::default()
                .with_embed_exif(false)
                .with_embed_xmp(false)
                .with_embed_icc(false),
        )
        .unwrap();
        let info = crate::info(&bare).unwrap();
        assert!(!info.has_exif && !info.has_xmp && !info.has_icc);
        assert!(decode(&bare).unwrap().metadata.is_empty());
        let mut sink = Vec::new();
        encode_to(&img, &EncodeOptions::default(), &mut sink).unwrap();
        assert_eq!(sink, bytes);
    }

    #[test]
    fn rgb_one_call_paths_code_420_by_default() {
        let rgb: Vec<u8> = (0..16 * 8 * 3).map(|i| (i * 7 % 256) as u8).collect();
        let bytes = encode_rgb8(16, 8, &rgb, &EncodeOptions::default()).unwrap();
        let info = crate::info(&bytes).unwrap();
        assert_eq!(info.format, PixelFormat::Yuv420P);
        assert_eq!(info.color, ColorInfo::miaf_default());
        let back = decode_rgb8(&bytes).unwrap();
        assert_eq!((back.width, back.height), (16, 8));
        // Lossless luma, box-filtered chroma: close, not exact.
        let err: u32 = back
            .data
            .iter()
            .zip(&rgb)
            .map(|(a, b)| u32::from(a.abs_diff(*b)))
            .sum();
        assert!(err / (16 * 8 * 3) < 24, "mean error {}", err / (16 * 8 * 3));
        // Odd extents fall back to 4:4:4, and 4:4:4 at q 0 is lossless
        // up to the forward/inverse matrix rounding.
        let odd: Vec<u8> = (0..5 * 3 * 3).map(|i| (i * 11 % 256) as u8).collect();
        let bytes = encode_rgb8(5, 3, &odd, &EncodeOptions::default()).unwrap();
        assert_eq!(crate::info(&bytes).unwrap().format, PixelFormat::Yuv444P);
        let back = decode_rgb8(&bytes).unwrap();
        assert!(back.data.iter().zip(&odd).all(|(a, b)| a.abs_diff(*b) <= 2));
        // RGBA: alpha as the auxiliary, exact.
        let rgba: Vec<u8> = (0..8 * 8 * 4).map(|i| (i * 13 % 256) as u8).collect();
        let bytes = encode_rgba8(8, 8, &rgba, &EncodeOptions::default()).unwrap();
        let info = crate::info(&bytes).unwrap();
        assert!(info.has_alpha);
        assert_eq!(info.format, PixelFormat::Yuva420P);
        let back = decode_rgba8(&bytes).unwrap();
        assert!(back
            .data
            .chunks_exact(4)
            .zip(rgba.chunks_exact(4))
            .all(|(a, b)| a[3] == b[3]));
        // Quality from the options changes the output.
        let lossy = encode_rgb8(16, 8, &rgb, &EncodeOptions::default().with_quality(50)).unwrap();
        assert_ne!(
            lossy,
            encode_rgb8(16, 8, &rgb, &EncodeOptions::default()).unwrap()
        );
        assert!(decode(&lossy).is_ok());
        // Wrong byte count is refused.
        assert!(matches!(
            encode_rgb8(4, 4, &[0; 10], &EncodeOptions::default()),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn encode_refuses_unsupported_layouts_without_converting() {
        // Odd 4:2:0 extents: AV1 could, this encoder does not.
        let img = AvifImage::new(
            3,
            3,
            PixelFormat::Yuv420P,
            vec![
                Plane::new(3, vec![0; 9]),
                Plane::new(2, vec![0; 4]),
                Plane::new(2, vec![0; 4]),
            ],
        )
        .unwrap();
        assert!(matches!(
            encode(&img, &EncodeOptions::default()),
            Err(Error::Unsupported(_))
        ));
        // Plane count mismatch (fields assigned after construction).
        let mut img =
            AvifImage::new(4, 4, PixelFormat::Gray8, vec![Plane::new(4, vec![0; 16])]).unwrap();
        img.format = PixelFormat::Yuv420P;
        assert!(matches!(
            encode(&img, &EncodeOptions::default()),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn deep_gray_and_packed_ya_round_trip() {
        // 10-bit gray + alpha → Ya16Le after decode; encode takes it back.
        let words: Vec<u16> = (0..8 * 4).map(|i| (i * 33 % 1024) as u16).collect();
        let alpha: Vec<u16> = (0..8 * 4).map(|i| (1023 - i * 29 % 1024) as u16).collect();
        let still = StillImage::yuv(
            8,
            4,
            10,
            StillChroma::Monochrome,
            words.clone(),
            vec![],
            vec![],
        )
        .unwrap()
        .with_alpha(alpha.clone())
        .unwrap();
        let bytes = crate::still::encode_still(&still, &EncodeOptions::default()).unwrap();
        let img = decode(&bytes).unwrap();
        assert_eq!(img.format, PixelFormat::Ya16Le);
        assert_eq!(img.bit_depth, 10);
        assert_eq!(crate::info(&bytes).unwrap().format, PixelFormat::Ya16Le);
        let again = encode(&img, &EncodeOptions::default()).unwrap();
        let back = decode(&again).unwrap();
        assert_eq!(back.planes, img.planes);
        assert_eq!(back.bit_depth, 10);
        // The RGBA view scales 10 → 8 bits.
        let rgba = img.to_rgba8();
        assert_eq!(rgba.len(), 8 * 4 * 4);
        assert_eq!(rgba[0], ((u32::from(words[0]) * 255 + 511) / 1023) as u8);
        assert_eq!(rgba[3], ((u32::from(alpha[0]) * 255 + 511) / 1023) as u8);
    }

    #[test]
    fn sequence_decodes_every_sample_with_delays() {
        let bytes = fixture("alpha_video.avif");
        let info = crate::info(&bytes).unwrap();
        assert!(info.is_sequence);
        let frames = decode_all(&bytes).unwrap();
        assert_eq!(frames.len(), info.frames as usize);
        assert!(frames
            .iter()
            .all(|f| f.delay.is_some_and(|d| d > Duration::ZERO)));
        assert!(frames
            .iter()
            .all(|f| f.image.width == frames[0].image.width));
        let first = decode(&bytes).unwrap();
        assert_eq!(first, frames[0].image);
        let rgb = first.to_rgb8();
        assert_eq!(rgb.len(), first.width as usize * first.height as usize * 3);
    }

    #[test]
    fn stills_decode_all_as_one_frame() {
        let bytes = fixture("kimono_rotate90.avif");
        let frames = decode_all(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].delay, None);
        assert_eq!(frames[0].image, decode(&bytes).unwrap());
    }

    #[test]
    fn limits_and_strictness_refuse_before_decoding() {
        let bytes = fixture("monochrome.avif");
        let info = crate::info(&bytes).unwrap();
        let too_small = DecodeOptions::default().with_max_width(Some(info.width - 1));
        assert!(matches!(
            decode_with(&bytes, &too_small),
            Err(Error::LimitExceeded(_))
        ));
        let few_pixels = DecodeOptions::default().with_max_pixels(Some(16));
        assert!(matches!(
            decode_with(&bytes, &few_pixels),
            Err(Error::LimitExceeded(_))
        ));
        let few_bytes = DecodeOptions::default().with_max_bytes(Some(bytes.len() as u64 - 1));
        assert!(matches!(
            decode_with(&bytes, &few_bytes),
            Err(Error::LimitExceeded(_))
        ));
        assert!(matches!(
            decode_all_with(&bytes, &few_bytes),
            Err(Error::LimitExceeded(_))
        ));
        // The fixture declares the AVIF brand, so strict passes.
        let strict = decode_with(&bytes, &DecodeOptions::default().with_strict(true)).unwrap();
        assert_eq!(strict, decode(&bytes).unwrap());
        // A file whose ftyp carries only HEIF structural brands is
        // refused under strict and accepted otherwise.
        let mut structural = bytes.clone();
        let pos = structural
            .windows(4)
            .position(|w| w == b"avif")
            .expect("brand in ftyp");
        structural[pos..pos + 4].copy_from_slice(b"mif1");
        while let Some(p) = structural[..64].windows(4).position(|w| w == b"avif") {
            structural[p..p + 4].copy_from_slice(b"mif1");
        }
        assert!(decode(&structural).is_ok());
        assert!(matches!(
            decode_with(&structural, &DecodeOptions::default().with_strict(true)),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn framework_frame_conversions_round_trip() {
        let img = AvifImage::from_rgba8(4, 2, (0..32).collect())
            .unwrap()
            .with_bit_depth(8);
        let vf = VideoFrame::from(img.clone());
        // Four image planes; the colour signal rides as a side-channel
        // record behind them.
        assert_eq!(vf.image_plane_count(), 4);
        assert_eq!(
            vf.color_signal(),
            Some(color_signal_for(Some(&img.color.to_colr())))
        );
        let mut params = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
        params.width = Some(4);
        params.height = Some(2);
        params.pixel_format = Some(oxideav_core::PixelFormat::Yuva444P);
        let back = AvifImage::from_video_frame(&vf, &params).unwrap();
        assert_eq!(back, img);
        // The decoder's own label for the same picture works too.
        params.pixel_format = Some(oxideav_core::PixelFormat::Gbrap8);
        let back = AvifImage::try_from((&vf, &params)).unwrap();
        assert_eq!(back, img);
        // Missing geometry is an error, not a guess.
        params.width = None;
        assert!(AvifImage::from_video_frame(&vf, &params).is_err());
        // Ya16Le carries its depth on the significant-bits channel.
        let ya = AvifImage::new(2, 1, PixelFormat::Ya16Le, vec![Plane::new(8, vec![0; 8])])
            .unwrap()
            .with_bit_depth(12);
        let vf = VideoFrame::from(ya.clone());
        assert_eq!(vf.significant_bits(), Some(&[12u8][..]));
        let mut params = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
        params.width = Some(2);
        params.height = Some(1);
        params.pixel_format = Some(oxideav_core::PixelFormat::Ya16Le);
        assert_eq!(
            AvifImage::from_video_frame(&vf, &params).unwrap().bit_depth,
            12
        );
    }
}
