//! AVIF `Decoder` implementation.
//!
//! The decoder does the full container-side composition pass: it parses
//! HEIF box hierarchy, decodes the primary item's AV1 OBU stream via
//! [`oxideav_av1::Av1Decoder`], then stitches grid tiles (HEIF §6.6.2),
//! applies `clap` / `irot` / `imir` post-transforms, and composites an
//! auxiliary alpha plane when one is present. Decode errors from the
//! underlying AV1 crate bubble up unchanged.

use oxideav_core::frame::VideoFrame;
use oxideav_core::Decoder;
use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, PixelFormat, Result, TimeBase};

use oxideav_av1::{Av1CodecConfig, Av1Decoder};

use crate::alpha::{composite_alpha, find_alpha_item_id};
use crate::box_parser::{b, BoxType};
use crate::cicp::{effective_cicp, CicpTriple};
use crate::grid::{composite_grid, ImageGrid};
use crate::meta::{Colr, Ispe, ItemLocation, Meta, Pasp, Pixi, Property};
use crate::parser::parse;
use crate::parser::{
    classify_brands, item_bytes, parse_header, AvifHeader, AvifImage, BrandClass, ITEM_TYPE_AV01,
    ITEM_TYPE_GRID,
};
use crate::transform::{apply_clap, apply_imir, apply_irot, crop_top_left};

/// Infer `(format, width, height)` from a decoded AV1 [`VideoFrame`].
/// `oxideav-av1` emits 8-bit planar Y/U/V with `stride == width` per
/// plane and `data.len() == stride * height`, so we can reverse the
/// mapping from plane geometry back to a `PixelFormat`.
fn infer_av1_pixmap(frame: &VideoFrame) -> Result<(PixelFormat, u32, u32)> {
    if frame.planes.is_empty() {
        return Err(Error::invalid("avif: AV1 frame has no planes"));
    }
    let y = &frame.planes[0];
    let width = y.stride as u32;
    if width == 0 {
        return Err(Error::invalid("avif: AV1 frame Y plane has zero stride"));
    }
    let height = (y.data.len() / y.stride) as u32;
    let format = match frame.planes.len() {
        1 => PixelFormat::Gray8,
        3 => {
            let u = &frame.planes[1];
            // 4:2:0 — chroma stride is half luma; chroma data len is
            // chroma_stride * (height / 2 ceil).
            let chroma_h = u.data.len().checked_div(u.stride).unwrap_or(0);
            if u.stride * 2 == y.stride && chroma_h * 2 >= height as usize {
                if chroma_h as u32 == height.div_ceil(2) {
                    PixelFormat::Yuv420P
                } else {
                    PixelFormat::Yuv422P
                }
            } else if u.stride == y.stride {
                PixelFormat::Yuv444P
            } else {
                return Err(Error::unsupported(format!(
                    "avif: cannot infer AV1 frame format (Y stride {}, U stride {}, U rows {})",
                    y.stride, u.stride, chroma_h
                )));
            }
        }
        n => {
            return Err(Error::unsupported(format!(
                "avif: AV1 frame has {n} planes, expected 1 or 3"
            )))
        }
    };
    Ok((format, width, height))
}

const AV1C: BoxType = b(b"av1C");
const ISPE: BoxType = b(b"ispe");
const COLR: BoxType = b(b"colr");
const IROT: BoxType = b(b"irot");
const IMIR: BoxType = b(b"imir");
const CLAP: BoxType = b(b"clap");
const DIMG: BoxType = b(b"dimg");

/// High-level view of an AVIF file after the HEIF pass — useful for
/// callers that want to inspect dimensions + colour info without
/// constructing a full `Decoder`.
#[derive(Clone, Debug)]
pub struct AvifInfo {
    pub width: u32,
    pub height: u32,
    /// Per-channel bit depth from the `pixi` property (HEIF §6.5.6).
    /// Empty when no `pixi` is associated with the primary item — in
    /// that case callers can fall back to the AV1 sequence-header bit
    /// depth.
    pub bits_per_channel: Vec<u8>,
    /// Pixel aspect ratio from the `pasp` property (HEIF §6.5.4 /
    /// ISO/IEC 14496-12 §8.5.2.1.1). `None` when absent (square pixel
    /// is the implicit default).
    pub pasp: Option<Pasp>,
    pub av1c: Vec<u8>,
    pub obu_bytes: Vec<u8>,
    /// True when the primary item is a grid (composite) item.
    pub is_grid: bool,
    /// True when an alpha auxiliary is attached to the primary item.
    pub has_alpha: bool,
    /// Brand classification from the file's `ftyp` box (av1-avif §6 +
    /// §8, ISO/IEC 23000-22 §7).
    pub brands: BrandClass,
    /// Colour information attached to the primary item, if any
    /// (`colr` box: nclx CICP triple or ICC payload). For grid
    /// primaries we surface the property attached to the grid item if
    /// present, falling back to the first tile's `colr`.
    pub colour: Option<Colr>,
}

impl AvifInfo {
    /// Number of channels per pixel — `bits_per_channel.len()`. Returns
    /// 0 when the primary item lacks a `pixi` property.
    pub fn num_channels(&self) -> usize {
        self.bits_per_channel.len()
    }

    /// Maximum bit depth across all channels, or 0 when no `pixi` is
    /// attached. Useful for readers picking an output buffer width
    /// (8 vs 16 bit) without parsing `av1C`.
    pub fn max_bit_depth(&self) -> u8 {
        self.bits_per_channel.iter().copied().max().unwrap_or(0)
    }

    /// True when the file declares a single-channel pixi — the typical
    /// signal for an AVIF monochrome image.
    pub fn is_monochrome(&self) -> bool {
        self.num_channels() == 1
    }

    /// True when the `pasp` property is either absent or declares
    /// `1:1` (or any equal-spacing) pixels. Callers that ignore non-
    /// square pixels can branch on this single check.
    pub fn has_square_pixels(&self) -> bool {
        match self.pasp {
            None => true,
            Some(p) => p.is_square(),
        }
    }

    /// Resolve the effective CICP signalling quadruple for the primary
    /// item: parse the `colr` nclx triple if present, fold to the
    /// spec-mandated `Unspecified` quadruple `(2, 2, 2, false)`
    /// otherwise. Spec: av1-avif §2.1, ITU-T H.273 §8.
    ///
    /// Per av1-avif §4.2.3.1 AVIF readers do not apply colour
    /// transforms to the decoded pixels — the CICP triple is purely
    /// signalling. Use this to drive a downstream colour-managed
    /// renderer or transcoder; do NOT use it as a license to insert
    /// matrix / transfer adjustments into the decoded sample buffer.
    ///
    /// When the primary item carries an embedded ICC profile
    /// ([`Colr::Icc`]) the triple folds to `Unspecified` — the ICC
    /// profile is the authoritative colour description in that case
    /// and the caller should consult `info.colour` for its bytes.
    pub fn effective_cicp(&self) -> CicpTriple {
        effective_cicp(self.colour.as_ref())
    }
}

pub fn inspect(file: &[u8]) -> Result<AvifInfo> {
    // Grid primaries fail parse() but succeed parse_header().
    let hdr = parse_header(file)?;
    let primary_id = hdr
        .meta
        .primary_item_id
        .ok_or_else(|| Error::invalid("avif: missing pitm"))?;
    let primary_info = hdr
        .meta
        .item_by_id(primary_id)
        .ok_or_else(|| Error::invalid("avif: pitm references unknown item"))?;
    let brands = classify_brands(&hdr.major_brand, &hdr.compatible_brands)?;
    if primary_info.item_type == ITEM_TYPE_GRID {
        build_info_grid(&hdr, primary_id, brands)
    } else {
        let img = parse(file)?;
        build_info(
            &img,
            find_alpha_item_id(&hdr.meta, primary_id).is_some(),
            brands,
        )
    }
}

fn build_info(img: &AvifImage<'_>, has_alpha: bool, brands: BrandClass) -> Result<AvifInfo> {
    let av1c = img
        .av1c
        .clone()
        .ok_or_else(|| Error::invalid("avif: primary item missing av1C property"))?;
    let Ispe { width, height } = img
        .ispe
        .ok_or_else(|| Error::invalid("avif: primary item missing ispe property"))?;
    let bits_per_channel = img
        .pixi
        .as_ref()
        .map(|Pixi { bits_per_channel }| bits_per_channel.clone())
        .unwrap_or_default();
    Ok(AvifInfo {
        width,
        height,
        bits_per_channel,
        pasp: img.pasp,
        av1c,
        obu_bytes: img.primary_item_data.to_vec(),
        is_grid: false,
        has_alpha,
        brands,
        colour: img.colr.clone(),
    })
}

fn build_info_grid(hdr: &AvifHeader<'_>, primary_id: u32, brands: BrandClass) -> Result<AvifInfo> {
    // Pull grid item bytes, parse the descriptor.
    let loc = hdr
        .meta
        .location_by_id(primary_id)
        .ok_or_else(|| Error::invalid("avif: grid item missing in iloc"))?;
    let grid_bytes = item_bytes(hdr.file, loc)?;
    let grid = ImageGrid::parse(grid_bytes)?;
    // Tile list.
    let tile_ids = hdr.meta.iref_targets(&DIMG, primary_id);
    if tile_ids.is_empty() {
        return Err(Error::invalid("avif: grid item has no dimg iref"));
    }
    let first_tile_id = tile_ids[0];
    // Pull the first tile's av1C + dimensions to report.
    let av1c = match hdr.meta.property_for(first_tile_id, &AV1C) {
        Some(Property::Av1C(bytes)) => bytes.clone(),
        _ => {
            return Err(Error::invalid(
                "avif: first grid tile missing av1C property",
            ))
        }
    };
    // HEIF §6.5.6 (`pixi`) and §6.5.4 (`pasp`) are descriptive
    // properties that describe the **reconstructed** image — for a
    // grid that's the assembled canvas, not any individual tile. The
    // spec lets the writer attach them either to the grid item
    // (canonical) or rely on the tile-0 association (the per-tile
    // values are required to be uniform across tiles, so tile 0 is
    // representative). We probe the grid item first and fall back to
    // tile 0 — same fallback shape as `colr` below.
    let bits_per_channel = match hdr.meta.property_for(primary_id, b"pixi") {
        Some(Property::Pixi(pixi)) => pixi.bits_per_channel.clone(),
        _ => match hdr.meta.property_for(first_tile_id, b"pixi") {
            Some(Property::Pixi(pixi)) => pixi.bits_per_channel.clone(),
            _ => Vec::new(),
        },
    };
    let pasp = match hdr.meta.property_for(primary_id, b"pasp") {
        Some(Property::Pasp(p)) => Some(*p),
        _ => match hdr.meta.property_for(first_tile_id, b"pasp") {
            Some(Property::Pasp(p)) => Some(*p),
            _ => None,
        },
    };
    // Per av1-avif §4.2.1 / HEIF §6.5.5: a `colr` describing a grid
    // derived image item may be attached to the grid item itself
    // (canonical placement — describes the reconstructed canvas) or,
    // when the writer omitted it on the grid, inherited from tile 0.
    // The av1-avif input-image-items uniformity rule
    // (§4.2.3.1 — same color information across all inputs) applies
    // to derived items broadly, so picking tile 0 when the grid lacks
    // its own `colr` reproduces the writer's intent.
    let colour = match hdr.meta.property_for(primary_id, &COLR) {
        Some(Property::Colr(c)) => Some(c.clone()),
        _ => match hdr.meta.property_for(first_tile_id, &COLR) {
            Some(Property::Colr(c)) => Some(c.clone()),
            _ => None,
        },
    };
    Ok(AvifInfo {
        width: grid.output_width,
        height: grid.output_height,
        bits_per_channel,
        pasp,
        av1c,
        obu_bytes: Vec::new(),
        is_grid: true,
        has_alpha: find_alpha_item_id(&hdr.meta, primary_id).is_some(),
        brands,
        colour,
    })
}

/// `Decoder` trait impl registered under codec id `avif`.
pub struct AvifDecoder {
    codec_id: CodecId,
    /// Frames ready to hand out via `receive_frame()`.
    pending: Vec<Frame>,
    /// The AvifInfo of the last decoded file, retained for `info()`.
    info: Option<AvifInfo>,
}

impl AvifDecoder {
    pub fn new(codec_id: CodecId) -> Self {
        Self {
            codec_id,
            pending: Vec::new(),
            info: None,
        }
    }

    /// Parse an AVIF file and decode the primary item. Grid + alpha +
    /// transform post-processing is applied before the frame is queued.
    /// Returns the resolved `AvifInfo` on success.
    pub fn decode_file(&mut self, file: &[u8]) -> Result<AvifInfo> {
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

        // Decode the primary frame, either via the grid path or the
        // single-item path.
        let brands = classify_brands(&hdr.major_brand, &hdr.compatible_brands)?;
        let (color_frame, color_format, mut width, mut height, info) =
            if primary_info.item_type == ITEM_TYPE_GRID {
                let (f, fmt, w, h) = decode_grid_primary(&hdr, primary_id)?;
                let info = build_info_grid(&hdr, primary_id, brands)?;
                (f, fmt, w, h, info)
            } else if primary_info.item_type == ITEM_TYPE_AV01 {
                let img = parse(file)?;
                let (f, fmt, w, h) = decode_av01_item(
                    img.primary_item_data,
                    img.av1c
                        .as_deref()
                        .ok_or_else(|| Error::invalid("avif: primary item missing av1C"))?,
                    img.ispe.map(|e| (e.width, e.height)),
                )?;
                let has_alpha = find_alpha_item_id(&hdr.meta, primary_id).is_some();
                let info = build_info(&img, has_alpha, brands)?;
                (f, fmt, w, h, info)
            } else {
                return Err(Error::unsupported(format!(
                    "avif: primary item type '{}' not supported",
                    String::from_utf8_lossy(&primary_info.item_type)
                )));
            };

        // Alpha composite, if an alpha auxiliary item is present.
        let (mut frame, mut format) = match find_alpha_item_id(&hdr.meta, primary_id) {
            Some(alpha_id) => {
                let (alpha_frame, alpha_format, _aw, _ah) = decode_alpha_item(&hdr, alpha_id)?;
                let (composited, fmt) = composite_alpha(
                    &color_frame,
                    color_format,
                    width,
                    height,
                    &alpha_frame,
                    alpha_format,
                )?;
                (composited, fmt)
            }
            None => (color_frame, color_format),
        };

        // Post-transforms: clap -> irot -> imir, per §6.5.10 application
        // order.
        // ispe-based crop against coded dimensions: if the AV1 decoder
        // emitted a padded frame the ispe width/height clamps it back
        // to the declared display rect.
        if let Some(Property::Ispe(ispe)) = hdr.meta.property_for(primary_id, &ISPE) {
            if (ispe.width, ispe.height) != (width, height)
                && ispe.width <= width
                && ispe.height <= height
                && ispe.width > 0
                && ispe.height > 0
            {
                frame = crop_top_left(&frame, format, width, height, ispe.width, ispe.height)?;
                width = ispe.width;
                height = ispe.height;
            }
        }
        if let Some(Property::Clap(clap)) = hdr.meta.property_for(primary_id, &CLAP) {
            let (f, w, h) = apply_clap(&frame, format, width, height, clap)?;
            frame = f;
            width = w;
            height = h;
        }
        if let Some(Property::Irot(irot)) = hdr.meta.property_for(primary_id, &IROT) {
            let (f, w, h) = apply_irot(&frame, format, width, height, irot)?;
            frame = f;
            width = w;
            height = h;
        }
        if let Some(Property::Imir(imir)) = hdr.meta.property_for(primary_id, &IMIR) {
            let (f, w, h) = apply_imir(&frame, format, width, height, imir)?;
            frame = f;
            width = w;
            height = h;
        }

        let _ = (width, height, &mut format);
        self.pending.push(Frame::Video(frame));
        self.info = Some(info.clone());
        Ok(info)
    }

    pub fn info(&self) -> Option<&AvifInfo> {
        self.info.as_ref()
    }
}

/// Decode a single av01 item's OBU bitstream into a `VideoFrame` plus
/// its inferred `(format, width, height)` triple. The slim
/// [`VideoFrame`] no longer carries those fields, so we recover them
/// from plane geometry.
fn decode_av01_item(
    obu_bytes: &[u8],
    av1c: &[u8],
    ispe: Option<(u32, u32)>,
) -> Result<(VideoFrame, PixelFormat, u32, u32)> {
    let _cfg = Av1CodecConfig::parse(av1c)?; // eagerly validate
    let mut params = CodecParameters::video(CodecId::new("av1"));
    if let Some((w, h)) = ispe {
        params.width = Some(w);
        params.height = Some(h);
    }
    params.extradata = av1c.to_vec();
    let mut av1 = Av1Decoder::new(params);
    let pkt = Packet::new(0, TimeBase::new(1, 90_000), obu_bytes.to_vec());
    av1.send_packet(&pkt)?;
    let frame = match av1.receive_frame()? {
        Frame::Video(v) => v,
        other => {
            return Err(Error::unsupported(format!(
                "avif: AV1 decoder returned non-video frame: {other:?}"
            )))
        }
    };
    let (format, width, height) = infer_av1_pixmap(&frame)?;
    Ok((frame, format, width, height))
}

/// Decode a grid-type primary item: decode each tile through the av01
/// path, then composite into the declared output rectangle. Returns
/// the composited frame plus its `(format, width, height)` triple.
fn decode_grid_primary(
    hdr: &AvifHeader<'_>,
    grid_id: u32,
) -> Result<(VideoFrame, PixelFormat, u32, u32)> {
    let loc = hdr
        .meta
        .location_by_id(grid_id)
        .ok_or_else(|| Error::invalid("avif: grid item missing in iloc"))?;
    let grid_bytes = item_bytes(hdr.file, loc)?;
    let grid = ImageGrid::parse(grid_bytes)?;
    let tile_ids = hdr.meta.iref_targets(&DIMG, grid_id);
    if tile_ids.is_empty() {
        return Err(Error::invalid("avif: grid item has no dimg iref"));
    }
    if tile_ids.len() != grid.expected_tile_count() {
        return Err(Error::invalid(format!(
            "avif: grid declares {} tiles but dimg lists {}",
            grid.expected_tile_count(),
            tile_ids.len()
        )));
    }
    let mut tiles = Vec::with_capacity(tile_ids.len());
    let mut tile_format: Option<PixelFormat> = None;
    let mut tile_dims: Option<(u32, u32)> = None;
    for (i, tid) in tile_ids.iter().enumerate() {
        let tile_info = hdr
            .meta
            .item_by_id(*tid)
            .ok_or_else(|| Error::invalid(format!("avif: grid tile {i} id {tid} unknown")))?;
        if tile_info.item_type != ITEM_TYPE_AV01 {
            return Err(Error::unsupported(format!(
                "avif: grid tile {i} item_type '{}' != 'av01'",
                String::from_utf8_lossy(&tile_info.item_type)
            )));
        }
        let tile_loc = hdr
            .meta
            .location_by_id(*tid)
            .ok_or_else(|| Error::invalid(format!("avif: grid tile {i} missing iloc")))?;
        let tile_bytes = item_bytes(hdr.file, tile_loc)?;
        let av1c = match hdr.meta.property_for(*tid, &AV1C) {
            Some(Property::Av1C(bytes)) => bytes.clone(),
            _ => {
                return Err(Error::invalid(format!(
                    "avif: grid tile {i} missing av1C property"
                )))
            }
        };
        let ispe_dims = match hdr.meta.property_for(*tid, &ISPE) {
            Some(Property::Ispe(e)) => Some((e.width, e.height)),
            _ => None,
        };
        let (mut frame, fmt, mut fw, mut fh) = decode_av01_item(tile_bytes, &av1c, ispe_dims)?;
        // Clamp tile to ispe dims if the AV1 decoder emitted a padded
        // output.
        if let Some((iw, ih)) = ispe_dims {
            if iw > 0 && ih > 0 && iw <= fw && ih <= fh && (iw != fw || ih != fh) {
                frame = crop_top_left(&frame, fmt, fw, fh, iw, ih)?;
                fw = iw;
                fh = ih;
            }
        }
        if let Some(want_fmt) = tile_format {
            if want_fmt != fmt {
                return Err(Error::invalid(format!(
                    "avif: grid tile {i} format {fmt:?} differs from tile 0 {want_fmt:?}"
                )));
            }
        } else {
            tile_format = Some(fmt);
        }
        if let Some((tw, th)) = tile_dims {
            if (tw, th) != (fw, fh) {
                return Err(Error::invalid(format!(
                    "avif: grid tile {i} dims {fw}x{fh} differ from tile 0 {tw}x{th}"
                )));
            }
        } else {
            tile_dims = Some((fw, fh));
        }
        tiles.push(frame);
    }
    let format = tile_format.expect("at least one tile present");
    let (tile_w, tile_h) = tile_dims.expect("at least one tile present");
    let composited = composite_grid(&grid, &tiles, format, tile_w, tile_h)?;
    Ok((composited, format, grid.output_width, grid.output_height))
}

/// Decode the alpha auxiliary item into a `VideoFrame`. The item must
/// be an AV1-coded monochrome image; the returned frame's format is
/// `PixelFormat::Gray8`.
fn decode_alpha_item(
    hdr: &AvifHeader<'_>,
    alpha_id: u32,
) -> Result<(VideoFrame, PixelFormat, u32, u32)> {
    let loc: &ItemLocation = hdr
        .meta
        .location_by_id(alpha_id)
        .ok_or_else(|| Error::invalid("avif: alpha item missing in iloc"))?;
    let bytes = item_bytes(hdr.file, loc)?;
    let av1c = match hdr.meta.property_for(alpha_id, &AV1C) {
        Some(Property::Av1C(b)) => b.clone(),
        _ => return Err(Error::invalid("avif: alpha item missing av1C property")),
    };
    let ispe = match hdr.meta.property_for(alpha_id, &ISPE) {
        Some(Property::Ispe(e)) => Some((e.width, e.height)),
        _ => None,
    };
    decode_av01_item(bytes, &av1c, ispe)
}

/// Walk a `Meta` and extract every transform + auxiliary signal the
/// decoder applies, in the order they should run. Useful for external
/// callers that want to mirror the pipeline.
pub fn transforms_for(meta: &Meta, item_id: u32) -> Vec<&Property> {
    let mut out = Vec::new();
    let Some(assoc) = meta.assoc_by_id(item_id) else {
        return out;
    };
    for pa in &assoc.entries {
        let Some(prop) = meta.properties.get(pa.index as usize) else {
            continue;
        };
        match prop {
            Property::Clap(_) | Property::Irot(_) | Property::Imir(_) => out.push(prop),
            _ => {}
        }
    }
    out
}

impl Decoder for AvifDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        // Every AVIF packet is a complete file.
        self.decode_file(&packet.data).map(|_| ())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if self.pending.is_empty() {
            return Err(Error::NeedMore);
        }
        Ok(self.pending.remove(0))
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.pending.clear();
        self.info = None;
        Ok(())
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(AvifDecoder::new(params.codec_id.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/monochrome.avif");

    #[test]
    fn inspect_extracts_primary_item() {
        let info = inspect(FIXTURE).expect("inspect");
        assert!(info.width > 0 && info.height > 0);
        // av1C always starts with the marker/version byte 0x81.
        assert_eq!(info.av1c[0], 0x81);
        assert!(!info.is_grid);
        assert!(!info.has_alpha);
    }

    #[test]
    fn decoder_surfaces_av1_errors_unwrapped() {
        // When the underlying av1 crate can't decode the bitstream the
        // decoder must surface its error verbatim — no "blocked by av1
        // limitations" wrapping. Whether the fixture decodes cleanly
        // depends on the av1 crate version on crates.io; both outcomes
        // are legitimate.
        let mut d = AvifDecoder::new(CodecId::new(crate::CODEC_ID_STR));
        let pkt = Packet::new(0, TimeBase::new(1, 1), FIXTURE.to_vec());
        match d.send_packet(&pkt) {
            Ok(()) => {
                let frame = d
                    .receive_frame()
                    .expect("receive_frame after send_packet success");
                let vf = match frame {
                    Frame::Video(v) => v,
                    other => panic!("expected VideoFrame, got {other:?}"),
                };
                assert!(!vf.planes.is_empty());
                // Width inferred from the Y plane stride; height from
                // the plane data length.
                let y = &vf.planes[0];
                assert!(y.stride > 0);
                let inferred_h = y.data.len() / y.stride;
                assert!(inferred_h > 0);
            }
            Err(Error::Unsupported(s)) => {
                // Must NOT contain the old "blocked by av1 decoder
                // limitations" wrapper — the whole point of Phase 8.1 is
                // that avif surfaces av1's native error verbatim.
                assert!(
                    !s.contains("blocked by av1 decoder limitations"),
                    "error should pass through raw, got: {s}"
                );
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
