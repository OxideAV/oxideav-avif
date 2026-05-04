//! AVIF `Decoder` implementation — registry-gated.
//!
//! The decoder does the full container-side composition pass: it parses
//! HEIF box hierarchy, decodes the primary item's AV1 OBU stream via
//! [`oxideav_av1::Av1Decoder`], then stitches grid tiles (HEIF §6.6.2),
//! applies `clap` / `irot` / `imir` post-transforms, and composites an
//! auxiliary alpha plane when one is present. Decode errors from the
//! underlying AV1 crate bubble up unchanged.
//!
//! This module is gated behind the default-on `registry` Cargo feature
//! because it pulls in `oxideav_av1` (which transitively pulls in
//! `oxideav_core`) and exposes the `oxideav_core::Decoder` trait surface.
//! With the feature off the standalone container parser
//! ([`crate::inspect`], [`crate::parse`], [`crate::parse_header`],
//! [`crate::parse_avis`], plus the composition layer in
//! [`crate::grid`] / [`crate::alpha`] / [`crate::transform`] working on
//! [`crate::image::AvifFrame`]) is still the public surface.

use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::Decoder;
use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, PixelFormat, Result, TimeBase};

use oxideav_av1::{Av1CodecConfig, Av1Decoder};

use crate::alpha::{composite_alpha, find_alpha_item_id};
use crate::box_parser::{b, BoxType};
use crate::grid::{composite_grid, ImageGrid};
use crate::image::{AvifFrame, AvifPixelFormat, AvifPlane};
use crate::inspect::{build_info, build_info_grid, AvifInfo};
use crate::meta::{ItemLocation, Property};
use crate::parser::{
    classify_brands, item_bytes, parse, parse_header, AvifHeader, ITEM_TYPE_AV01, ITEM_TYPE_GRID,
};
use crate::transform::{apply_clap, apply_imir, apply_irot, crop_top_left};

/// Re-export the `inspect` entry point so the registry-gated public API
/// keeps its historical shape (`oxideav_avif::inspect`).
pub use crate::inspect::{inspect, transforms_for};

// `inspect`, `transforms_for`, and `AvifInfo` live in [`crate::inspect`]
// — they don't need the `oxideav-core`/`oxideav-av1` dependency tree.
// The decoder simply re-exports them from there.

/// Map an [`AvifPixelFormat`] (crate-local) to the framework
/// [`PixelFormat`]. The mapping is total because every variant of
/// `AvifPixelFormat` corresponds to one variant of `PixelFormat`.
fn to_core_pix(fmt: AvifPixelFormat) -> PixelFormat {
    match fmt {
        AvifPixelFormat::Yuv420P => PixelFormat::Yuv420P,
        AvifPixelFormat::Yuv422P => PixelFormat::Yuv422P,
        AvifPixelFormat::Yuv444P => PixelFormat::Yuv444P,
        AvifPixelFormat::Gray8 => PixelFormat::Gray8,
        AvifPixelFormat::Yuva420P => PixelFormat::Yuva420P,
        AvifPixelFormat::Ya8 => PixelFormat::Ya8,
    }
}

/// Inverse of [`to_core_pix`] for the small set of formats the AV1
/// decoder actually emits. The composition path only ever feeds frames
/// through its own [`AvifPixelFormat`] variants.
fn from_core_pix(fmt: PixelFormat) -> Result<AvifPixelFormat> {
    match fmt {
        PixelFormat::Yuv420P => Ok(AvifPixelFormat::Yuv420P),
        PixelFormat::Yuv422P => Ok(AvifPixelFormat::Yuv422P),
        PixelFormat::Yuv444P => Ok(AvifPixelFormat::Yuv444P),
        PixelFormat::Gray8 => Ok(AvifPixelFormat::Gray8),
        PixelFormat::Yuva420P => Ok(AvifPixelFormat::Yuva420P),
        PixelFormat::Ya8 => Ok(AvifPixelFormat::Ya8),
        other => Err(Error::unsupported(format!(
            "avif: AV1 decoder emitted unsupported PixelFormat {other:?}"
        ))),
    }
}

/// Convert a framework [`VideoFrame`] (returned by `oxideav_av1`) into
/// the crate-local [`AvifFrame`] the composition layer consumes. Plane
/// data is moved, not copied.
fn core_to_avif_frame(vf: VideoFrame) -> AvifFrame {
    AvifFrame {
        pts: vf.pts,
        planes: vf
            .planes
            .into_iter()
            .map(|p| AvifPlane {
                stride: p.stride,
                data: p.data,
            })
            .collect(),
    }
}

/// Inverse of [`core_to_avif_frame`] — used when handing the composited
/// frame back to the framework via the `Decoder::receive_frame` trait.
fn avif_to_core_frame(af: AvifFrame) -> VideoFrame {
    VideoFrame {
        pts: af.pts,
        planes: af
            .planes
            .into_iter()
            .map(|p| VideoPlane {
                stride: p.stride,
                data: p.data,
            })
            .collect(),
    }
}

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
const IROT: BoxType = b(b"irot");
const IMIR: BoxType = b(b"imir");
const CLAP: BoxType = b(b"clap");
const DIMG: BoxType = b(b"dimg");

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
        let hdr = parse_header(file).map_err(core_err)?;
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
        let brands = classify_brands(&hdr.major_brand, &hdr.compatible_brands).map_err(core_err)?;
        let (color_frame, color_format, mut width, mut height, info) =
            if primary_info.item_type == ITEM_TYPE_GRID {
                let (f, fmt, w, h) = decode_grid_primary(&hdr, primary_id)?;
                let info = build_info_grid(&hdr, primary_id, brands).map_err(core_err)?;
                (f, fmt, w, h, info)
            } else if primary_info.item_type == ITEM_TYPE_AV01 {
                let img = parse(file).map_err(core_err)?;
                let (f, fmt, w, h) = decode_av01_item(
                    img.primary_item_data,
                    img.av1c
                        .as_deref()
                        .ok_or_else(|| Error::invalid("avif: primary item missing av1C"))?,
                    img.ispe.map(|e| (e.width, e.height)),
                )?;
                let has_alpha = find_alpha_item_id(&hdr.meta, primary_id).is_some();
                let info = build_info(&img, has_alpha, brands).map_err(core_err)?;
                (f, fmt, w, h, info)
            } else {
                return Err(Error::unsupported(format!(
                    "avif: primary item type '{}' not supported",
                    String::from_utf8_lossy(&primary_info.item_type)
                )));
            };

        // Move into crate-local AvifFrame for the composition layer.
        let mut frame = core_to_avif_frame(color_frame);
        let mut format = from_core_pix(color_format)?;

        // Alpha composite, if an alpha auxiliary item is present.
        if let Some(alpha_id) = find_alpha_item_id(&hdr.meta, primary_id) {
            let (alpha_frame, alpha_format, _aw, _ah) = decode_alpha_item(&hdr, alpha_id)?;
            let alpha_avif = core_to_avif_frame(alpha_frame);
            let alpha_avif_fmt = from_core_pix(alpha_format)?;
            let (composited, fmt) =
                composite_alpha(&frame, format, width, height, &alpha_avif, alpha_avif_fmt)
                    .map_err(core_err)?;
            frame = composited;
            format = fmt;
        }

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
                frame = crop_top_left(&frame, format, width, height, ispe.width, ispe.height)
                    .map_err(core_err)?;
                width = ispe.width;
                height = ispe.height;
            }
        }
        if let Some(Property::Clap(clap)) = hdr.meta.property_for(primary_id, &CLAP) {
            let (f, w, h) = apply_clap(&frame, format, width, height, clap).map_err(core_err)?;
            frame = f;
            width = w;
            height = h;
        }
        if let Some(Property::Irot(irot)) = hdr.meta.property_for(primary_id, &IROT) {
            let (f, w, h) = apply_irot(&frame, format, width, height, irot).map_err(core_err)?;
            frame = f;
            width = w;
            height = h;
        }
        if let Some(Property::Imir(imir)) = hdr.meta.property_for(primary_id, &IMIR) {
            let (f, w, h) = apply_imir(&frame, format, width, height, imir).map_err(core_err)?;
            frame = f;
            width = w;
            height = h;
        }

        let _ = (width, height, &mut format);
        self.pending.push(Frame::Video(avif_to_core_frame(frame)));
        self.info = Some(info.clone());
        Ok(info)
    }

    pub fn info(&self) -> Option<&AvifInfo> {
        self.info.as_ref()
    }
}

/// Bridge: convert a crate-local [`crate::error::AvifError`] into the
/// framework `Error` variants. The decoder calls this on every
/// container-side `Result<T>` so the trait surface still returns
/// `oxideav_core::Result`.
fn core_err(e: crate::error::AvifError) -> Error {
    match e {
        crate::error::AvifError::InvalidData(s) => Error::InvalidData(s),
        crate::error::AvifError::Unsupported(s) => Error::Unsupported(s),
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
    let grid_bytes = item_bytes(hdr.file, loc).map_err(core_err)?;
    let grid = ImageGrid::parse(grid_bytes).map_err(core_err)?;
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
    let mut tiles: Vec<AvifFrame> = Vec::with_capacity(tile_ids.len());
    let mut tile_format: Option<AvifPixelFormat> = None;
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
        let tile_bytes = item_bytes(hdr.file, tile_loc).map_err(core_err)?;
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
        let (tile_core, fmt_core, mut fw, mut fh) = decode_av01_item(tile_bytes, &av1c, ispe_dims)?;
        let mut tile = core_to_avif_frame(tile_core);
        let fmt = from_core_pix(fmt_core)?;
        // Clamp tile to ispe dims if the AV1 decoder emitted a padded
        // output.
        if let Some((iw, ih)) = ispe_dims {
            if iw > 0 && ih > 0 && iw <= fw && ih <= fh && (iw != fw || ih != fh) {
                tile = crop_top_left(&tile, fmt, fw, fh, iw, ih).map_err(core_err)?;
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
        tiles.push(tile);
    }
    let format = tile_format.expect("at least one tile present");
    let (tile_w, tile_h) = tile_dims.expect("at least one tile present");
    let composited = composite_grid(&grid, &tiles, format, tile_w, tile_h).map_err(core_err)?;
    Ok((
        avif_to_core_frame(composited),
        to_core_pix(format),
        grid.output_width,
        grid.output_height,
    ))
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
    let bytes = item_bytes(hdr.file, loc).map_err(core_err)?;
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
