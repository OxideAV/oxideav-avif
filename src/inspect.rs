//! Standalone container-side inspection: parse the HEIF box hierarchy,
//! resolve the primary item, surface dimensions / colour / pixi / pasp /
//! alpha-presence in [`AvifInfo`].
//!
//! No `oxideav-core` dependency — this module works whether or not the
//! `registry` feature is enabled. The pixel-decoding [`crate::decoder`]
//! module sits on top of this and adds the AV1 + composition pipeline.

use crate::alpha::find_alpha_item_id;
use crate::box_parser::{b, BoxType};
use crate::cicp::{effective_cicp, CicpTriple};
use crate::error::{AvifError as Error, Result};
use crate::grid::ImageGrid;
use crate::meta::{Cclv, Clli, Colr, Ispe, Mdcv, Meta, Pasp, Pixi, Property};
use crate::parser::{
    classify_brands, item_bytes, parse, parse_header, AvifHeader, AvifImage, BrandClass,
    ITEM_TYPE_GRID,
};

const AV1C: BoxType = b(b"av1C");
const COLR: BoxType = b(b"colr");
const MDCV: BoxType = b(b"mdcv");
const CLLI: BoxType = b(b"clli");
const CCLV: BoxType = b(b"cclv");

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
    /// Mastering display colour volume (SMPTE ST 2086 / CTA-861-G).
    /// Present when the primary item (or grid item / first tile)
    /// carries an `mdcv` item property. Indicates the HDR display the
    /// content was mastered on: primaries, white point, max/min
    /// luminance. `None` for SDR content without `mdcv`.
    pub mdcv: Option<Mdcv>,
    /// Content light level info (MaxCLL / MaxFALL in cd/m²). Present
    /// when a `clli` item property is attached to the primary item.
    /// `None` for SDR content or HDR content that omits the field.
    pub clli: Option<Clli>,
    /// Colour volume luminance hint (`cclv` — draft av1-avif extension).
    /// Same semantics as `clli`; some encoders emit this in lieu of or
    /// alongside `clli`. `None` when the box is absent.
    pub cclv: Option<Cclv>,
    /// Bit depth derived from `av1C` — `None` when `av1c` is empty.
    /// 8 = standard, 10 or 12 = HDR. Avoids callers having to re-parse
    /// the `av1C` record to know the coded depth.
    pub bit_depth: Option<u8>,
    /// Monochrome flag from `av1C` — `true` for 4:0:0 (Y-only) streams.
    /// When `pixi` carries a single channel and this flag is `true` the
    /// two signals agree; callers can trust either.
    pub monochrome: bool,
    /// Chroma subsampling from `av1C`: `(subsampling_x, subsampling_y)`.
    /// `(true, true)` = 4:2:0; `(true, false)` = 4:2:2;
    /// `(false, false)` = 4:4:4. `None` when `av1c` is empty or
    /// monochrome (subsampling is undefined for 4:0:0).
    pub chroma_subsampling: Option<(bool, bool)>,
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

    /// True when any HDR metadata box (`mdcv`, `clli`, or `cclv`) is
    /// attached to the primary item. High-level gate for downstream
    /// consumers that only need to know "is this HDR" without inspecting
    /// individual boxes.
    pub fn has_hdr_metadata(&self) -> bool {
        self.mdcv.is_some() || self.clli.is_some() || self.cclv.is_some()
    }

    /// Effective MaxCLL in cd/m² — prefers `clli` over `cclv` when both
    /// are present. Returns `None` when neither box is attached.
    pub fn max_cll(&self) -> Option<u16> {
        self.clli
            .map(|c| c.max_content_light_level)
            .or_else(|| self.cclv.map(|c| c.max_content_light_level))
    }

    /// Effective MaxFALL in cd/m² — prefers `clli` over `cclv` when both
    /// are present. Returns `None` when neither box is attached.
    pub fn max_fall(&self) -> Option<u16> {
        self.clli
            .map(|c| c.max_pic_average_light_level)
            .or_else(|| self.cclv.map(|c| c.max_pic_average_light_level))
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

/// Decode `av1C` bytes into `(bit_depth, monochrome, chroma_subsampling)`.
/// Returns `(None, false, None)` on parse failure so callers degrade
/// gracefully rather than erroring out on this auxiliary field.
fn decode_av1c_flags(av1c: &[u8]) -> (Option<u8>, bool, Option<(bool, bool)>) {
    if av1c.len() < 3 {
        return (None, false, None);
    }
    let b2 = av1c[2];
    let high_bitdepth = ((b2 >> 6) & 1) != 0;
    let twelve_bit = ((b2 >> 5) & 1) != 0;
    let monochrome = ((b2 >> 4) & 1) != 0;
    let chroma_subsampling_x = ((b2 >> 3) & 1) != 0;
    let chroma_subsampling_y = ((b2 >> 2) & 1) != 0;

    let bit_depth = if twelve_bit {
        12
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let subsampling = if monochrome {
        None
    } else {
        Some((chroma_subsampling_x, chroma_subsampling_y))
    };
    (Some(bit_depth), monochrome, subsampling)
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
    let (bit_depth, monochrome, chroma_subsampling) = decode_av1c_flags(&av1c);
    // HDR metadata from item properties.
    let mdcv = img.mdcv;
    let clli = img.clli;
    let cclv = img.cclv;
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
        mdcv,
        clli,
        cclv,
        bit_depth,
        monochrome,
        chroma_subsampling,
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
    // HDR metadata follows same fallback pattern: grid item first, tile 0 second.
    let mdcv = match hdr.meta.property_for(primary_id, &MDCV) {
        Some(Property::Mdcv(m)) => Some(*m),
        _ => match hdr.meta.property_for(first_tile_id, &MDCV) {
            Some(Property::Mdcv(m)) => Some(*m),
            _ => None,
        },
    };
    let clli = match hdr.meta.property_for(primary_id, &CLLI) {
        Some(Property::Clli(c)) => Some(*c),
        _ => match hdr.meta.property_for(first_tile_id, &CLLI) {
            Some(Property::Clli(c)) => Some(*c),
            _ => None,
        },
    };
    let cclv = match hdr.meta.property_for(primary_id, &CCLV) {
        Some(Property::Cclv(c)) => Some(*c),
        _ => match hdr.meta.property_for(first_tile_id, &CCLV) {
            Some(Property::Cclv(c)) => Some(*c),
            _ => None,
        },
    };
    let (bit_depth, monochrome, chroma_subsampling) = decode_av1c_flags(&av1c);
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
        mdcv,
        clli,
        cclv,
        bit_depth,
        monochrome,
        chroma_subsampling,
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

    /// `AvifInfo` carries `bit_depth` + `monochrome` + `chroma_subsampling`
    /// decoded from `av1C`. The monochrome.avif fixture is 8-bit mono (4:0:0)
    /// so `bit_depth` = 8, `monochrome` = true, `chroma_subsampling` = None.
    #[test]
    fn inspect_av1c_flags_decoded() {
        let info = inspect(FIXTURE).expect("inspect");
        // av1C is present and well-formed → bit_depth populated.
        let bd = info
            .bit_depth
            .expect("bit_depth should be populated from av1C");
        // Monochrome.avif is 8-bit.
        assert_eq!(bd, 8, "monochrome fixture should be 8-bit");
        // Monochrome means no chroma subsampling info.
        assert!(
            info.monochrome,
            "monochrome fixture should set monochrome=true"
        );
        assert!(
            info.chroma_subsampling.is_none(),
            "monochrome stream has no chroma planes"
        );
    }

    /// SDR fixture has no HDR metadata — all three HDR fields are `None`
    /// and `has_hdr_metadata()` returns false.
    #[test]
    fn inspect_sdr_fixture_has_no_hdr_metadata() {
        let info = inspect(FIXTURE).expect("inspect");
        assert!(
            !info.has_hdr_metadata(),
            "SDR monochrome fixture must not have HDR metadata"
        );
        assert!(info.mdcv.is_none(), "mdcv should be None for SDR fixture");
        assert!(info.clli.is_none(), "clli should be None for SDR fixture");
        assert!(info.cclv.is_none(), "cclv should be None for SDR fixture");
        assert!(info.max_cll().is_none(), "max_cll() should be None for SDR");
        assert!(
            info.max_fall().is_none(),
            "max_fall() should be None for SDR"
        );
    }

    /// `decode_av1c_flags` correctly extracts 10-bit / 12-bit flags.
    #[test]
    fn decode_av1c_flags_hdr_bit_depths() {
        // Build synthetic av1C bytes:
        // byte 0 = 0x81 (marker + version=1)
        // byte 1 = 0x00 (seq_profile=0, level=0)
        // byte 2 encodes bitdepth: high_bitdepth(1 bit 6), twelve_bit(1 bit 5),
        //   monochrome(0 bit 4), subsampling_x(1 bit 3), subsampling_y(1 bit 2)
        // 10-bit: high_bitdepth=1, twelve_bit=0 → byte2 = 0b0100_1100 = 0x4c
        let av1c_10bit = [0x81, 0x00, 0x4c, 0x00];
        let (bd, mono, sub) = decode_av1c_flags(&av1c_10bit);
        assert_eq!(bd, Some(10));
        assert!(!mono);
        assert_eq!(sub, Some((true, true))); // subsampling_x + y set

        // 12-bit: high_bitdepth=1, twelve_bit=1 → byte2 = 0b0110_1100 = 0x6c
        let av1c_12bit = [0x81, 0x00, 0x6c, 0x00];
        let (bd, mono, sub) = decode_av1c_flags(&av1c_12bit);
        assert_eq!(bd, Some(12));
        assert!(!mono);
        assert!(sub.is_some());

        // Monochrome 8-bit: high=0, twelve=0, mono=1 → byte2 = 0b0001_0000 = 0x10
        let av1c_mono = [0x81, 0x00, 0x10, 0x00];
        let (bd, mono, sub) = decode_av1c_flags(&av1c_mono);
        assert_eq!(bd, Some(8));
        assert!(mono);
        assert!(sub.is_none()); // monochrome → no chroma subsampling

        // 8-bit 4:4:4 (subsampling_x=0, subsampling_y=0, mono=0): byte2=0x00
        let av1c_444 = [0x81, 0x00, 0x00, 0x00];
        let (bd, mono, sub) = decode_av1c_flags(&av1c_444);
        assert_eq!(bd, Some(8));
        assert!(!mono);
        assert_eq!(sub, Some((false, false))); // 4:4:4

        // Too short: < 3 bytes → graceful None.
        let (bd, mono, sub) = decode_av1c_flags(&[0x81]);
        assert!(bd.is_none());
        assert!(!mono);
        assert!(sub.is_none());
    }
}
