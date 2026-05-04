//! Standalone container-side inspection: parse the HEIF box hierarchy,
//! resolve the primary item, surface dimensions / colour / pixi / pasp /
//! alpha-presence in [`AvifInfo`].
//!
//! No `oxideav-core` dependency — this module works whether or not the
//! `registry` feature is enabled. The pixel-decoding [`crate::decoder`]
//! module sits on top of this and adds the AV1 + composition pipeline.

use crate::box_parser::{b, BoxType};
use crate::cicp::{effective_cicp, CicpTriple};
use crate::error::{AvifError as Error, Result};
use crate::grid::ImageGrid;
use crate::meta::{Colr, Ispe, Meta, Pasp, Pixi, Property};
use crate::parser::{
    classify_brands, item_bytes, parse, parse_header, AvifHeader, AvifImage, BrandClass,
    ITEM_TYPE_GRID,
};
use crate::{alpha::find_alpha_item_id};

const AV1C: BoxType = b(b"av1C");
const COLR: BoxType = b(b"colr");

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

/// Walk the `ftyp` + `meta` boxes of an AVIF file and build an
/// [`AvifInfo`] for the primary item. Handles both single-item and grid
/// primaries; returns `Error::InvalidData` when the file lacks a `pitm`
/// or the primary item is not resolvable.
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

pub(crate) fn build_info(
    img: &AvifImage<'_>,
    has_alpha: bool,
    brands: BrandClass,
) -> Result<AvifInfo> {
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

pub(crate) fn build_info_grid(
    hdr: &AvifHeader<'_>,
    primary_id: u32,
    brands: BrandClass,
) -> Result<AvifInfo> {
    // Pull grid item bytes, parse the descriptor.
    let loc = hdr
        .meta
        .location_by_id(primary_id)
        .ok_or_else(|| Error::invalid("avif: grid item missing in iloc"))?;
    let grid_bytes = item_bytes(hdr.file, loc)?;
    let grid = ImageGrid::parse(grid_bytes)?;
    // Tile list.
    let dimg = b(b"dimg");
    let tile_ids = hdr.meta.iref_targets(&dimg, primary_id);
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

/// Walk a `Meta` and extract every transform + auxiliary signal the
/// decoder applies, in the order they should run. Useful for external
/// callers that want to mirror the pipeline without depending on the
/// `registry` feature.
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
}
