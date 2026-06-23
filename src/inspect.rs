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
use crate::meta::{
    Amve, Cclv, Clli, Colr, Ispe, Mdcv, Meta, Pasp, Pixi, Property, ITEM_TYPE_EXIF, ITEM_TYPE_MIME,
};
use crate::parser::{
    classify_brands, item_bytes_with_idat, parse, parse_header, AvifHeader, AvifImage, BrandClass,
    ITEM_TYPE_GRID,
};

const AV1C: BoxType = b(b"av1C");
const COLR: BoxType = b(b"colr");
const MDCV: BoxType = b(b"mdcv");
const CLLI: BoxType = b(b"clli");
const CCLV: BoxType = b(b"cclv");
const AMVE: BoxType = b(b"amve");

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
    /// Ambient viewing environment HDR metadata (`amve`, AVIF §6.5.36).
    /// Present when the primary item (or grid item / first tile) carries
    /// an `amve` item property. Describes the *viewer's* nominal ambient
    /// environment (illuminance + ambient-light chromaticity) — distinct
    /// from `mdcv`/`clli`, which describe the *content's* mastering
    /// environment. `None` when the box is absent.
    pub amve: Option<Amve>,
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
    /// Item IDs of every thumbnail (`thmb` iref source) attached to the
    /// primary item. HEIF / ISOBMFF §8.11.12: a `thmb` iref's `from_id`
    /// is the thumbnail item; its `to_ids` lists the master image(s)
    /// the thumbnail represents. Multiple thumbnails of varying sizes
    /// can be attached, hence a `Vec`. Empty when the file ships no
    /// thumbnails.
    pub thumbnail_item_ids: Vec<u32>,
    /// Item ID of the Exif metadata item linked to the primary, when
    /// present. Detection rules:
    ///
    /// 1. Find every item linked by a `cdsc` (content-description) iref
    ///    whose `to_ids` includes the primary item.
    /// 2. Among those, pick the one whose `infe` declares
    ///    `item_type == 'Exif'`, OR whose `item_type == 'mime'` with a
    ///    `content_type` of `application/octet-stream` or `image/tiff`
    ///    (both forms appear in the wild — see ISO/IEC 23008-12
    ///    §A.2.1 and §A.2.2 for the native and generic-mime carriers).
    pub exif_item_id: Option<u32>,
    /// Item ID of the XMP metadata item linked to the primary, when
    /// present. Detection rule: a `cdsc` iref source whose `infe`
    /// declares `item_type == 'mime'` with `content_type ==
    /// "application/rdf+xml"` (the canonical XMP MIME type per
    /// ISO/IEC 23008-12 §A.3.2).
    pub xmp_item_id: Option<u32>,
    /// True when the alpha auxiliary is signalled as premultiplied via
    /// a HEIF `prem` iref (ISO/IEC 23008-12 §6.10.1.1). `false` when no
    /// alpha is present, or when alpha is present but the encoder
    /// didn't add the `prem` signal (the alpha is then straight /
    /// unassociated).
    pub premultiplied_alpha: bool,
    /// Every auxiliary item attached to the primary via an `auxl`
    /// iref, paired with its classified [`crate::meta::AuxKind`].
    /// Alpha typically lives in the first / only entry; depth maps
    /// and HDR gain maps appear here as separate entries with the
    /// matching kind.
    ///
    /// Empty when no auxC-bearing auxiliary is attached. Spec:
    /// HEIF §6.5.8 + av1-avif §4.1 / §4.4 (depth) / Apple HDR gain-map.
    pub aux_items: Vec<(u32, crate::meta::AuxKind)>,
    /// Convenience: the URN of the first alpha auxiliary item (when
    /// `has_alpha` is true). Distinguishes the MPEG and HEVC URN
    /// spellings without a re-walk of the meta.
    pub alpha_aux_kind: Option<crate::meta::AuxKind>,
    /// Item id of an attached depth-map auxiliary, when the primary
    /// item has one (HEIF §6.5.8 — `urn:mpeg:mpegB:cicp:systems:auxiliary:depth`
    /// or the HEVC spelling `urn:mpeg:hevc:2015:auxid:2`).
    pub depth_map_item_id: Option<u32>,
    /// Item id of an attached Apple HDR gain-map auxiliary
    /// (`urn:com:apple:photo:2020:aux:hdrgainmap`).
    pub hdr_gain_map_item_id: Option<u32>,
    /// Number of [`crate::derived::EntityGroup`] entries the file
    /// carries in a top-level `grpl`. Zero for the typical AVIF file
    /// that ships no groups list. Spec: HEIF §9.4.
    pub entity_group_count: usize,
    /// `mif1` brand compliance audit for the file. Surfaced through
    /// inspect for callers that want strict-mif1 mode without a
    /// separate audit pass. Spec: HEIF §10.2.1.1.
    pub mif1_compliance: crate::derived::Mif1Compliance,
    /// Operating-point selector (`a1op`) attached to the primary item,
    /// when present (av1-avif §2.3.2.1). Carries the `op_index` the
    /// reader should process for a scalable AV1 Image Item. The property
    /// is mandated essential, so a reader that cannot honour the index
    /// must reject the item. `None` for the common single-operating-point
    /// case.
    pub operating_point: Option<crate::meta::A1op>,
    /// AV1 layered-image indexing (`a1lx`) attached to the primary item,
    /// when present (av1-avif §2.3.2.3). Documents per-layer byte sizes
    /// so a caller can extract individual layers of an operating point.
    /// Non-essential; `None` for non-layered items.
    pub layered_index: Option<crate::meta::A1lx>,
    /// Item IDs of every Sample Transform Derived Image Item carried in
    /// the file (av1-avif v1.2.0 §4.2.3). Detection: `infe.item_type ==
    /// 'sato'`. The descriptor bytes live in `mdat` (resolve via
    /// [`item_payload_bytes`]) and parse with
    /// [`crate::derived::SampleTransform::parse`] given the parallel
    /// `dimg` iref's reference_count. Empty for files without any
    /// sample-transform derivation.
    pub sato_item_ids: Vec<u32>,
    /// Item IDs of every Tone Map Derived Image Item carried in the
    /// file (av1-avif v1.2.0 §4.2.2 — `'tmap'`). Detection: `infe.item_type
    /// == 'tmap'`. The ISO 21496-1 gain map metadata descriptor body
    /// each item points at via its `iloc` is parsed by
    /// [`crate::derived::GainMapMetadata::parse`]; the one-call
    /// extractor [`gain_map_metadata`] combines the byte resolve and
    /// parse for a tmap item id picked out of this list. Empty for
    /// files without any tone-map derivation. The av1-avif §4.2.2
    /// file-shape `should` constraints (altr pairing + hidden gain-map)
    /// are surfaced separately via [`Self::tone_map_compliance`].
    pub tmap_item_ids: Vec<u32>,
    /// av1-avif §4.2.2 compliance audit results, one entry per `'tmap'`
    /// item in [`Self::tmap_item_ids`] (same order). Each entry reports
    /// whether the file pairs the tmap with its base image item in an
    /// `'altr'` entity group and whether the gain-map input image
    /// item(s) are flagged hidden. Empty when no tmap items are
    /// present. Both checks are `should`s, not `shall`s — see
    /// [`crate::derived::ToneMapCompliance`] for the strict-mode
    /// interpretation.
    pub tone_map_compliance: Vec<crate::derived::ToneMapCompliance>,
    /// av1-avif §7 transformative-property `shall`-level audit results,
    /// one entry per `'grid'` item in the file (in `iinf` declaration
    /// order). Each entry lists offending `(tile_item_id,
    /// property_kind)` pairs for transformative properties (`'clap'` /
    /// `'irot'` / `'imir'`) attached to any tile in the grid's
    /// derivation chain. The compliant case is an empty `offenders`
    /// vector; combine with [`Self::grid_derivations_strict_compliant`]
    /// for a one-call gate.
    ///
    /// Spec: av1-avif v1.2.0 §7 — "Transformative properties shall not
    /// be associated with items in a derivation chain that serves as an
    /// input to a grid derived image item." Per-tile transformative
    /// properties are only permitted on the grid item itself.
    pub grid_derivation_compliance: Vec<crate::derived::GridDerivationAudit>,
    /// Item IDs of every Identity Derived Image Item carried in the file
    /// (HEIF §6.6.2.1 — `'iden'`). Detection: `infe.item_type == 'iden'`.
    /// `iden` items have no body — the output is the source image with
    /// the iden's own transformative properties applied. Empty for files
    /// without any identity derivation.
    pub iden_item_ids: Vec<u32>,
    /// HEIF §6.6.2.1 + §6.6.1 `shall`-level audit results, one entry per
    /// `'iden'` item in [`Self::iden_item_ids`] (same order). Each entry
    /// reports whether the iden's `'dimg'` reference_count is exactly 1,
    /// whether at most one `'dimg'` iref entry shares its `from_item_ID`,
    /// and whether the item has no body. Empty when no iden items are
    /// present. All three checks are `shall`s — see
    /// [`crate::derived::IdenCompliance`] for the strict-mode
    /// interpretation. Combine with [`Self::iden_strict_compliant`] for
    /// a one-call gate.
    pub iden_compliance: Vec<crate::derived::IdenCompliance>,
    /// av1-avif v1.2.0 §4.1 `shall`-level audit results, one entry per
    /// `(alpha, master)` pairing declared by an `'auxl'` iref whose
    /// source is classified as an AV1 Alpha Image Item (alpha URN
    /// prefix on its `auxC`). Each entry reports the bit depth decoded
    /// from each item's `av1C` and whether they agree. Empty when no
    /// AV1 Alpha Image Items are present.
    ///
    /// Spec: av1-avif v1.2.0 §4.1 — "An AV1 Alpha Image Item
    /// (respectively an AV1 Alpha Image Sequence) shall be encoded
    /// with the same bit depth as the associated master AV1 Image
    /// Item (respectively AV1 Image Sequence)." Combine with
    /// [`Self::alpha_bit_depth_strict_compliant`] for a one-call gate.
    pub alpha_bit_depth_compliance: Vec<crate::derived::AlphaBitDepthAudit>,
    /// av1-avif v1.2.0 §2.1 `shall`-level audit results, one entry per
    /// `'av01'` item in the file (in `iinf` declaration order). Each
    /// entry reports the Sequence Header OBU count walked from the
    /// item's payload and structural failure flags (missing iloc,
    /// truncated OBU stream, an OBU with `obu_has_size_field == 0`).
    /// Empty when the file ships no AV1 Image Items.
    ///
    /// Spec: av1-avif v1.2.0 §2.1 — "The AV1 Image Item Data shall
    /// have exactly one Sequence Header OBU." Combine with
    /// [`Self::sequence_header_obu_strict_compliant`] for a one-call
    /// gate.
    pub sequence_header_obu_compliance: Vec<crate::derived::SequenceHeaderObuAudit>,
    /// av1-avif v1.2.0 §8.2 (`MA1B`) / §8.3 (`MA1A`) profile
    /// `shall`-level audit, one entry per `(AV1 Image Item, declared
    /// profile)` pairing. Each record inspects the item's `av1C` for
    /// the `(seq_profile, seq_level_idx_0)` pair and reports whether
    /// it satisfies the declared profile's bounds (Baseline: Main +
    /// level ≤ 5.1; Advanced: ≤ High + level ≤ 6.0).
    ///
    /// Empty when (a) the file ships no AV1 Image Items, or (b) the
    /// file declares neither `MA1B` nor `MA1A` in its `ftyp`
    /// compatible-brands list. Combine with
    /// [`Self::avif_profile_strict_compliant`] for a one-call gate.
    pub avif_profile_compliance: Vec<crate::derived::AvifProfileCompliance>,
    /// Fully resolved `'iovl'` image-overlay derivations (HEIF §6.6.2.2),
    /// one entry per `iovl` item in `iinf` declaration order. Each carries
    /// the parsed descriptor (canvas dimensions + fill colour) plus, per
    /// input, the resolved placement rectangle and clip region against the
    /// canvas — all computed from the box graph alone (no AV1 decode).
    /// Empty for files without any overlay derivation. See
    /// [`crate::derived::OverlayResolution`].
    pub overlay_resolutions: Vec<crate::derived::OverlayResolution>,
    /// Fully resolved `'iden'` identity derivations (HEIF §6.6.2.1), one
    /// entry per `iden` item in `iinf` declaration order. Each carries the
    /// single source item id, the source's reconstructed dimensions, the
    /// transform chain the iden item applies, and the resulting output
    /// dimensions. Empty for files without any identity derivation. See
    /// [`crate::derived::IdenResolution`].
    pub iden_resolutions: Vec<crate::derived::IdenResolution>,
    /// Fully resolved `'tmap'` tone-map (gain-map) derivations (av1-avif
    /// §4.2.2), one entry per `tmap` item in `iinf` declaration order. Each
    /// carries the base image input id, the gain-map input id(s), the
    /// rendered (base) dimensions, and each gain map's coded extents — all
    /// from the box graph alone (no AV1 decode). The structured byte-level
    /// gain-map metadata is parsed separately by
    /// [`crate::derived::GainMapMetadata`]; this surfaces the derivation
    /// *geometry*. Empty for files without a tone-map derivation. See
    /// [`crate::derived::ToneMapResolution`].
    pub tone_map_resolutions: Vec<crate::derived::ToneMapResolution>,
    /// Fully resolved `'grid'` tile derivations (ISO/IEC 23008-12 §6.6.2.3),
    /// one entry per `grid` item in `iinf` declaration order. Each carries
    /// the parsed descriptor (row/column counts + output dimensions), the
    /// common tile dimensions, and per-tile row-major canvas placements with
    /// right/bottom-trim awareness — all from the box graph alone (no AV1
    /// decode). Empty for files without a grid derivation. See
    /// [`crate::derived::GridResolution`].
    pub grid_resolutions: Vec<crate::derived::GridResolution>,
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

    /// True when an ambient-viewing-environment (`amve`) item property is
    /// attached to the primary item. Distinct from [`has_hdr_metadata`],
    /// which reports the *content's* mastering metadata — `amve` describes
    /// the *viewer's* nominal ambient environment.
    ///
    /// [`has_hdr_metadata`]: Self::has_hdr_metadata
    pub fn has_ambient_viewing_environment(&self) -> bool {
        self.amve.is_some()
    }

    /// True when at least one thumbnail item is linked to the primary
    /// via a `thmb` iref. Shorthand for `!thumbnail_item_ids.is_empty()`.
    pub fn has_thumbnails(&self) -> bool {
        !self.thumbnail_item_ids.is_empty()
    }

    /// True when either Exif or XMP metadata is attached to the primary
    /// via a `cdsc` iref. Shorthand gate for downstream consumers that
    /// only need a "should I extract metadata" hint.
    pub fn has_descriptive_metadata(&self) -> bool {
        self.exif_item_id.is_some() || self.xmp_item_id.is_some()
    }

    /// True when an attached auxiliary item declares the depth-map URN
    /// (HEIF §6.5.8).
    pub fn has_depth_map(&self) -> bool {
        self.depth_map_item_id.is_some()
    }

    /// True when an attached auxiliary item declares Apple's HDR
    /// gain-map URN.
    pub fn has_hdr_gain_map(&self) -> bool {
        self.hdr_gain_map_item_id.is_some()
    }

    /// True when the file ships at least one Sample Transform Derived
    /// Image Item (av1-avif §4.2.3 — `sato`). The descriptor for each
    /// item ID in [`Self::sato_item_ids`] can be parsed with
    /// [`crate::derived::SampleTransform::parse`].
    pub fn has_sample_transform(&self) -> bool {
        !self.sato_item_ids.is_empty()
    }

    /// True when the file ships at least one Tone Map Derived Image
    /// Item (av1-avif §4.2.2 — `tmap`).
    pub fn has_tone_map(&self) -> bool {
        !self.tmap_item_ids.is_empty()
    }

    /// True when every `'tmap'` item in the file passes the av1-avif
    /// §4.2.2 `should`-level audit (paired with its base item via an
    /// `'altr'` group and every gain-map input marked hidden).
    ///
    /// Trivially `true` when the file ships no tmap items
    /// ([`Self::has_tone_map`] is `false`) — callers that want a
    /// presence + compliance signal should combine the two.
    pub fn tone_map_strict_compliant(&self) -> bool {
        self.tone_map_compliance.iter().all(|c| c.is_compliant())
    }

    /// True when every `'grid'` item in the file passes the av1-avif §7
    /// transformative-property `shall` audit (no tile in any grid's
    /// `dimg` derivation chain carries `'clap'` / `'irot'` / `'imir'`).
    ///
    /// Trivially `true` when the file ships no grid items (the
    /// constraint applies to grid derivation chains; a file with no
    /// grids has no such chains). Callers that want a presence +
    /// compliance signal should combine with [`Self::is_grid`].
    pub fn grid_derivations_strict_compliant(&self) -> bool {
        self.grid_derivation_compliance
            .iter()
            .all(|g| g.is_compliant())
    }

    /// True when the file ships at least one Identity Derived Image
    /// Item (HEIF §6.6.2.1 — `'iden'`).
    pub fn has_iden(&self) -> bool {
        !self.iden_item_ids.is_empty()
    }

    /// True when every `'iden'` item in the file passes the HEIF
    /// §6.6.2.1 + §6.6.1 `shall`-level audit (exactly one `'dimg'`
    /// input, exactly one `'dimg'` iref entry with that
    /// `from_item_ID`, and no item body).
    ///
    /// Trivially `true` when the file ships no iden items
    /// ([`Self::has_iden`] is `false`) — callers that want a presence
    /// + compliance signal should combine the two.
    pub fn iden_strict_compliant(&self) -> bool {
        self.iden_compliance.iter().all(|i| i.is_compliant())
    }

    /// True when the file carries at least one resolved `'iovl'` overlay
    /// derivation ([`Self::overlay_resolutions`] non-empty).
    pub fn has_overlay(&self) -> bool {
        !self.overlay_resolutions.is_empty()
    }

    /// The resolved overlay derivation for `iovl_item_id`, if present.
    pub fn overlay_for(&self, iovl_item_id: u32) -> Option<&crate::derived::OverlayResolution> {
        self.overlay_resolutions
            .iter()
            .find(|o| o.iovl_item_id == iovl_item_id)
    }

    /// The resolved identity derivation for `iden_item_id`, if present.
    pub fn iden_resolution_for(
        &self,
        iden_item_id: u32,
    ) -> Option<&crate::derived::IdenResolution> {
        self.iden_resolutions
            .iter()
            .find(|i| i.iden_item_id == iden_item_id)
    }

    /// The resolved tone-map (gain-map) derivation for `tmap_item_id`, if
    /// present. Pairs with [`Self::tone_map_compliance`] (the av1-avif
    /// §4.2.2 `should`-level audit) and [`gain_map_metadata`] (the
    /// byte-level ISO 21496-1 descriptor): this accessor exposes the
    /// derivation *geometry* (base / gain-map item ids + rendered extents).
    pub fn tone_map_resolution_for(
        &self,
        tmap_item_id: u32,
    ) -> Option<&crate::derived::ToneMapResolution> {
        self.tone_map_resolutions
            .iter()
            .find(|t| t.tmap_item_id == tmap_item_id)
    }

    /// True when the file carries at least one resolved `'grid'` tile
    /// derivation ([`Self::grid_resolutions`] non-empty).
    pub fn has_grid(&self) -> bool {
        !self.grid_resolutions.is_empty()
    }

    /// The resolved grid tile derivation for `grid_item_id`, if present.
    /// Exposes the row/column layout, common tile dimensions, and per-tile
    /// canvas placements (with right/bottom-trim awareness) without an AV1
    /// decode (ISO/IEC 23008-12 §6.6.2.3).
    pub fn grid_resolution_for(
        &self,
        grid_item_id: u32,
    ) -> Option<&crate::derived::GridResolution> {
        self.grid_resolutions
            .iter()
            .find(|g| g.grid_item_id == grid_item_id)
    }

    /// True when every AV1 Alpha Image Item's pairing with its master
    /// AV1 Image Item passes the av1-avif §4.1 bit-depth-match `shall`
    /// (and both items carry an `av1C` whose flag byte is reachable).
    ///
    /// Trivially `true` when the file ships no alpha auxiliaries — the
    /// constraint applies per `(alpha, master)` pairing; a file with
    /// none has no such pairings. Callers that want a presence +
    /// compliance signal should combine with [`Self::has_alpha`].
    pub fn alpha_bit_depth_strict_compliant(&self) -> bool {
        self.alpha_bit_depth_compliance
            .iter()
            .all(|a| a.is_compliant())
    }

    /// True when every AV1 Image Item in the file passes the av1-avif
    /// v1.2.0 §2.1 `shall` "The AV1 Image Item Data shall have exactly
    /// one Sequence Header OBU." A pass requires that the audit
    /// could walk the item's bytes (no `missing_iloc`), the OBU
    /// stream framing was well-formed (no `truncated_obu`, no
    /// `has_size_field_zero`), and exactly one Sequence Header OBU
    /// was counted.
    ///
    /// Trivially `true` for files with no AV1 Image Items (a
    /// degenerate case — AVIF requires the primary item be an av01
    /// or a derivation rooted on av01s).
    pub fn sequence_header_obu_strict_compliant(&self) -> bool {
        self.sequence_header_obu_compliance
            .iter()
            .all(|a| a.is_compliant())
    }

    /// True when every AV1 Image Item passes the av1-avif v1.2.0 §8.2
    /// (`MA1B`) / §8.3 (`MA1A`) profile `shall`-level constraints for
    /// every brand the file declares.
    ///
    /// Trivially `true` when [`Self::avif_profile_compliance`] is
    /// empty — either the file makes no profile claim (neither `MA1B`
    /// nor `MA1A` in the compatible-brands list) or the file has no
    /// AV1 Image Items. Callers that want a presence + compliance
    /// signal should check `brands.is_baseline_profile ||
    /// brands.is_advanced_profile` first.
    pub fn avif_profile_strict_compliant(&self) -> bool {
        self.avif_profile_compliance
            .iter()
            .all(|a| a.is_compliant())
    }

    /// True when the file's `ftyp` claims the `mif1` brand and every
    /// HEIF §10.2.1.1 mandatory child box is present in `meta`. False
    /// when the file claims `mif1` but is missing required boxes, OR
    /// when the file makes no `mif1` claim — call sites that want
    /// "is this strict-mif1" should check `mif1_compliance.claims_mif1`
    /// directly.
    pub fn is_strict_mif1(&self) -> bool {
        self.mif1_compliance.claims_mif1 && self.mif1_compliance.is_compliant()
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
    let mif1_compliance = crate::parser::audit_mif1(file)?;
    if primary_info.item_type == ITEM_TYPE_GRID {
        build_info_grid(&hdr, primary_id, brands, mif1_compliance)
    } else if primary_info.item_type == crate::meta::ITEM_TYPE_IOVL
        || primary_info.item_type == crate::meta::ITEM_TYPE_IDEN
        || primary_info.item_type == crate::meta::ITEM_TYPE_TMAP
        || primary_info.item_type == crate::meta::ITEM_TYPE_SATO
    {
        // A `'tmap'` primary (gain-map layout) resolves to its base image
        // input's extents (av1-avif §4.2.2); a `'sato'` primary resolves to
        // its own `ispe` (its inputs share those extents, §4.2.3.1). Both
        // borrow a representative `av1C` via the shared `first_coded_leaf`
        // walk, exactly like `iovl`/`iden`.
        build_info_derived(&hdr, primary_id, brands, mif1_compliance)
    } else {
        let img = parse(file)?;
        build_info(
            &img,
            find_alpha_item_id(&hdr.meta, primary_id).is_some(),
            brands,
            mif1_compliance,
            file,
        )
    }
}

/// Walk every `cdsc` iref whose `to_ids` contains `target_id` and
/// classify the source items as Exif / XMP based on the `infe` shape.
///
/// Returns `(exif_item_id, xmp_item_id)`. Either may be `None`. When
/// multiple Exif or XMP items are linked to the same target (rare in
/// practice — encoders ship one of each), the first encountered wins.
///
/// The Exif side accepts two encodings seen in the wild:
///
/// * `item_type == 'Exif'` (HEIF §A.2.1 native form).
/// * `item_type == 'mime'` with `content_type` matching
///   `application/octet-stream` or `image/tiff` — encoders that prefer
///   the generic MIME carrier wrap the Exif TIFF blob this way.
///
/// XMP follows the canonical form: `item_type == 'mime'` with
/// `content_type == "application/rdf+xml"`.
fn resolve_metadata_items(meta: &Meta, target_id: u32) -> (Option<u32>, Option<u32>) {
    const CDSC: BoxType = b(b"cdsc");
    let mut exif = None;
    let mut xmp = None;
    for src in meta.iref_sources_of(&CDSC, target_id) {
        let Some(info) = meta.item_by_id(src) else {
            continue;
        };
        if info.item_type == ITEM_TYPE_EXIF {
            if exif.is_none() {
                exif = Some(src);
            }
            continue;
        }
        if info.item_type == ITEM_TYPE_MIME {
            let ct = info.content_type.as_deref().unwrap_or("");
            // Case-insensitive match on the MIME root; encoders disagree
            // on capitalisation ("Application/rdf+xml" has been seen).
            let ct_lower = ct.to_ascii_lowercase();
            let is_xmp =
                ct_lower == "application/rdf+xml" || ct_lower.starts_with("application/rdf+xml");
            let is_exif_mime = ct_lower == "application/octet-stream"
                || ct_lower == "image/tiff"
                || ct_lower == "image/x-exif";
            if is_xmp && xmp.is_none() {
                xmp = Some(src);
            } else if is_exif_mime && exif.is_none() {
                exif = Some(src);
            }
        }
    }
    (exif, xmp)
}

/// Extract the raw item bytes for a given item ID from an AVIF file.
/// Useful for callers that have a populated [`AvifInfo`] and want to
/// pull the Exif or XMP payload out for further processing. For
/// multi-extent items this allocates and concatenates per HEIF §8.11.3.3;
/// for single-extent items this is a zero-copy slice copied into a
/// `Vec<u8>` (the API returns owned bytes for uniformity).
///
/// Resolves items stored at file offsets (`construction_method == 0`),
/// inside the `meta` box's `idat` (`construction_method == 1`), and via
/// item offsets into a referenced item's data (`construction_method ==
/// 2`, the `'iloc'` iref naming the data-origin item — ISO/IEC 14496-12
/// §8.11.3). Errors when the item is missing from `iloc`, when a cm=1
/// item references an absent `idat`, when a cm=2 item has no `'iloc'`
/// iref / an out-of-range `extent_index` / a self-reference, or when an
/// extent runs off the end of its backing buffer.
///
/// For Exif items (`item_type == 'Exif'`), HEIF §A.2.1 specifies that
/// the first 4 bytes of the resolved payload are a big-endian
/// `exif_tiff_header_offset` indicating where the TIFF header starts
/// inside the payload. Callers that want just the TIFF blob should skip
/// `4 + offset` bytes. We return the raw item bytes verbatim so callers
/// see the prefix; stripping is a downstream concern.
///
/// For `mime` items the returned bytes are the raw blob — no prefix /
/// no encoding-aware transform (the `content_encoding` field on the
/// matching [`crate::meta::ItemInfo`] tells callers whether to gunzip
/// the result; HEIF in the wild always ships `content_encoding` empty,
/// so the raw blob is usually directly consumable).
pub fn item_payload_bytes(file: &[u8], item_id: u32) -> Result<Vec<u8>> {
    let hdr = parse_header(file)?;
    // Resolve across all three construction methods: file-offset (0),
    // idat-offset (1) and item-offset (2, the `'iloc'` iref naming the
    // data-origin item). The cm=2-aware resolver consults the whole
    // `Meta` so metadata items (Exif / XMP / mime / tmap) stored as item
    // offsets into another item resolve too.
    crate::parser::item_bytes_owned_full(file, &hdr.meta, item_id)
}

/// Resolve a `'tmap'` item's payload bytes and parse them as an ISO
/// 21496-1:2025 Annex C.2 gain map metadata descriptor.
///
/// One-call wrapper that combines [`item_payload_bytes`] (to pull the
/// raw descriptor body out of `mdat` per the item's `iloc`) with
/// [`crate::derived::GainMapMetadata::parse`] (to decode the binary
/// layout). Callers that already hold the payload bytes can skip this
/// and call `GainMapMetadata::parse` directly.
///
/// Pick `tmap_item_id` from [`AvifInfo::tmap_item_ids`] — every entry
/// in that list is guaranteed to have an `infe` declaring `item_type ==
/// 'tmap'`. Passing an arbitrary item id is accepted (the call returns
/// whatever the byte resolver finds), but the parse will reject the
/// payload as malformed when the resolved bytes do not match the C.2
/// layout — callers that want a strict-checked extractor should gate
/// on `tmap_item_ids` membership first.
///
/// Errors propagate from both stages: an [`crate::error::AvifError::InvalidData`]
/// when the item is missing from `iloc`, when the iloc construction
/// method isn't file-offset (0), when an extent runs off the end of
/// `file`, or when the descriptor body violates a C.2.3 `shall`
/// constraint (zero rational denominator, zero `gamma_numerator`,
/// `writer_version < minimum_version`, truncated payload).
/// [`crate::error::AvifError::Unsupported`] when the descriptor's
/// `minimum_version` is one this parser doesn't recognise — the spec
/// directs such a reader to display the base image rather than treat
/// the bytes as malformed.
pub fn gain_map_metadata(
    file: &[u8],
    tmap_item_id: u32,
) -> Result<crate::derived::GainMapMetadata> {
    let bytes = item_payload_bytes(file, tmap_item_id)?;
    crate::derived::GainMapMetadata::parse(&bytes)
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
    mif1_compliance: crate::derived::Mif1Compliance,
    file: &[u8],
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
    let amve = img.amve;
    let primary_id = img.primary_item_id;
    let thumbnail_item_ids = img.meta.iref_sources_of(b"thmb", primary_id);
    let (exif_item_id, xmp_item_id) = resolve_metadata_items(&img.meta, primary_id);
    // Per HEIF §6.10.1.1, premultiplication is signalled by a `prem`
    // iref whose `from_id` is the alpha auxiliary and whose `to_ids`
    // contains the primary item. Find the alpha first, then check.
    let premultiplied_alpha = if has_alpha {
        match find_alpha_item_id(&img.meta, primary_id) {
            // The alpha item is the `from_id` of the `prem` iref;
            // `prem`'s `to_ids` lists the colour image(s) it premuls.
            // Walk every `prem` iref and look for one whose from matches
            // our alpha and whose to contains the primary.
            Some(alpha_id) => img.meta.irefs.iter().any(|e| {
                &e.reference_type == b"prem"
                    && e.from_id == alpha_id
                    && e.to_ids.contains(&primary_id)
            }),
            None => false,
        }
    } else {
        false
    };
    let aux_items = img.meta.aux_items_for(primary_id);
    let alpha_aux_kind = aux_items
        .iter()
        .find(|(_, k)| matches!(k, crate::meta::AuxKind::Alpha))
        .map(|(_, k)| *k);
    let depth_map_item_id = aux_items
        .iter()
        .find(|(_, k)| matches!(k, crate::meta::AuxKind::DepthMap))
        .map(|(id, _)| *id);
    let hdr_gain_map_item_id = aux_items
        .iter()
        .find(|(_, k)| matches!(k, crate::meta::AuxKind::HdrGainMap))
        .map(|(id, _)| *id);
    let entity_group_count = img.meta.groups().map(|g| g.len()).unwrap_or(0);
    let operating_point = match img.meta.property_for(primary_id, b"a1op") {
        Some(Property::A1op(a)) => Some(*a),
        _ => None,
    };
    let layered_index = match img.meta.property_for(primary_id, b"a1lx") {
        Some(Property::A1lx(a)) => Some(*a),
        _ => None,
    };
    let sato_item_ids = img.meta.item_ids_of_type(&crate::meta::ITEM_TYPE_SATO);
    let tmap_item_ids = img.meta.item_ids_of_type(&crate::meta::ITEM_TYPE_TMAP);
    let tone_map_compliance = crate::derived::audit_tone_map(&img.meta);
    let grid_derivation_compliance = crate::derived::audit_grid_derivations(&img.meta);
    let iden_item_ids = img.meta.item_ids_of_type(&crate::meta::ITEM_TYPE_IDEN);
    let iden_compliance = crate::derived::audit_iden_derivations(&img.meta);
    let alpha_bit_depth_compliance = crate::derived::audit_alpha_bit_depth(&img.meta);
    let sequence_header_obu_compliance = crate::derived::audit_sequence_header_obu(&img.meta, file);
    let avif_profile_compliance = crate::derived::audit_avif_profile_compliance(&img.meta, &brands);
    // `Meta::parse` already captured the meta box's `idat` (ItemDataBox)
    // payload; reuse it rather than re-walking the file.
    let idat = img.meta.idat.as_deref();
    let overlay_resolutions = crate::derived::resolve_overlays(&img.meta, file, idat);
    let iden_resolutions = crate::derived::resolve_iden_derivations(&img.meta, file, idat);
    let tone_map_resolutions = crate::derived::resolve_tone_maps(&img.meta, file, idat);
    let grid_resolutions = crate::derived::resolve_grids(&img.meta, file, idat);
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
        amve,
        bit_depth,
        monochrome,
        chroma_subsampling,
        thumbnail_item_ids,
        exif_item_id,
        xmp_item_id,
        premultiplied_alpha,
        aux_items,
        alpha_aux_kind,
        depth_map_item_id,
        hdr_gain_map_item_id,
        entity_group_count,
        mif1_compliance,
        operating_point,
        layered_index,
        sato_item_ids,
        tmap_item_ids,
        tone_map_compliance,
        grid_derivation_compliance,
        iden_item_ids,
        iden_compliance,
        alpha_bit_depth_compliance,
        sequence_header_obu_compliance,
        avif_profile_compliance,
        overlay_resolutions,
        iden_resolutions,
        tone_map_resolutions,
        grid_resolutions,
    })
}

pub(crate) fn build_info_grid(
    hdr: &AvifHeader<'_>,
    primary_id: u32,
    brands: BrandClass,
    mif1_compliance: crate::derived::Mif1Compliance,
) -> Result<AvifInfo> {
    // Pull grid item bytes, parse the descriptor.
    let loc = hdr
        .meta
        .location_by_id(primary_id)
        .ok_or_else(|| Error::invalid("avif: grid item missing in iloc"))?;
    let grid_bytes = item_bytes_with_idat(hdr.file, hdr.meta.idat.as_deref(), loc)?;
    let grid = ImageGrid::parse(&grid_bytes)?;
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
    let amve = match hdr.meta.property_for(primary_id, &AMVE) {
        Some(Property::Amve(a)) => Some(*a),
        _ => match hdr.meta.property_for(first_tile_id, &AMVE) {
            Some(Property::Amve(a)) => Some(*a),
            _ => None,
        },
    };
    let (bit_depth, monochrome, chroma_subsampling) = decode_av1c_flags(&av1c);
    let thumbnail_item_ids = hdr.meta.iref_sources_of(b"thmb", primary_id);
    let (exif_item_id, xmp_item_id) = resolve_metadata_items(&hdr.meta, primary_id);
    let has_alpha = find_alpha_item_id(&hdr.meta, primary_id).is_some();
    let premultiplied_alpha = if has_alpha {
        match find_alpha_item_id(&hdr.meta, primary_id) {
            Some(alpha_id) => hdr.meta.irefs.iter().any(|e| {
                &e.reference_type == b"prem"
                    && e.from_id == alpha_id
                    && e.to_ids.contains(&primary_id)
            }),
            None => false,
        }
    } else {
        false
    };
    let aux_items = hdr.meta.aux_items_for(primary_id);
    let alpha_aux_kind = aux_items
        .iter()
        .find(|(_, k)| matches!(k, crate::meta::AuxKind::Alpha))
        .map(|(_, k)| *k);
    let depth_map_item_id = aux_items
        .iter()
        .find(|(_, k)| matches!(k, crate::meta::AuxKind::DepthMap))
        .map(|(id, _)| *id);
    let hdr_gain_map_item_id = aux_items
        .iter()
        .find(|(_, k)| matches!(k, crate::meta::AuxKind::HdrGainMap))
        .map(|(id, _)| *id);
    let entity_group_count = hdr.meta.groups().map(|g| g.len()).unwrap_or(0);
    let operating_point = match hdr.meta.property_for(primary_id, b"a1op") {
        Some(Property::A1op(a)) => Some(*a),
        _ => None,
    };
    let layered_index = match hdr.meta.property_for(primary_id, b"a1lx") {
        Some(Property::A1lx(a)) => Some(*a),
        _ => None,
    };
    let sato_item_ids = hdr.meta.item_ids_of_type(&crate::meta::ITEM_TYPE_SATO);
    let tmap_item_ids = hdr.meta.item_ids_of_type(&crate::meta::ITEM_TYPE_TMAP);
    let tone_map_compliance = crate::derived::audit_tone_map(&hdr.meta);
    let grid_derivation_compliance = crate::derived::audit_grid_derivations(&hdr.meta);
    let iden_item_ids = hdr.meta.item_ids_of_type(&crate::meta::ITEM_TYPE_IDEN);
    let iden_compliance = crate::derived::audit_iden_derivations(&hdr.meta);
    let alpha_bit_depth_compliance = crate::derived::audit_alpha_bit_depth(&hdr.meta);
    let sequence_header_obu_compliance =
        crate::derived::audit_sequence_header_obu(&hdr.meta, hdr.file);
    let avif_profile_compliance = crate::derived::audit_avif_profile_compliance(&hdr.meta, &brands);
    let idat = hdr.meta.idat.as_deref();
    let overlay_resolutions = crate::derived::resolve_overlays(&hdr.meta, hdr.file, idat);
    let iden_resolutions = crate::derived::resolve_iden_derivations(&hdr.meta, hdr.file, idat);
    let tone_map_resolutions = crate::derived::resolve_tone_maps(&hdr.meta, hdr.file, idat);
    let grid_resolutions = crate::derived::resolve_grids(&hdr.meta, hdr.file, idat);
    Ok(AvifInfo {
        width: grid.output_width,
        height: grid.output_height,
        bits_per_channel,
        pasp,
        av1c,
        obu_bytes: Vec::new(),
        is_grid: true,
        has_alpha,
        brands,
        colour,
        mdcv,
        clli,
        cclv,
        amve,
        bit_depth,
        monochrome,
        chroma_subsampling,
        thumbnail_item_ids,
        exif_item_id,
        xmp_item_id,
        premultiplied_alpha,
        aux_items,
        alpha_aux_kind,
        depth_map_item_id,
        hdr_gain_map_item_id,
        entity_group_count,
        mif1_compliance,
        operating_point,
        layered_index,
        sato_item_ids,
        tmap_item_ids,
        tone_map_compliance,
        grid_derivation_compliance,
        iden_item_ids,
        iden_compliance,
        alpha_bit_depth_compliance,
        sequence_header_obu_compliance,
        avif_profile_compliance,
        overlay_resolutions,
        iden_resolutions,
        tone_map_resolutions,
        grid_resolutions,
    })
}

/// Walk the `'dimg'` derivation chain from `item_id` down to the first
/// coded `'av01'` leaf, returning its item id. Grid / iovl / iden / sato /
/// tmap items recurse into their inputs; a coded item returns itself. Bounded
/// by [`crate::derived::MAX_DERIVATION_DEPTH`] to break `dimg` cycles.
///
/// Used to find a representative coded item from which a derived-primary
/// `AvifInfo` can borrow the `av1C` configuration record (bit depth,
/// monochrome flag, chroma subsampling) — the derivation's inputs are
/// required to share colour/format information (av1-avif §4.2.3.1), so the
/// first leaf is representative.
fn first_coded_leaf(meta: &Meta, item_id: u32, depth: u32) -> Option<u32> {
    if depth > crate::derived::MAX_DERIVATION_DEPTH {
        return None;
    }
    let item = meta.item_by_id(item_id)?;
    if item.item_type == crate::parser::ITEM_TYPE_AV01 {
        return Some(item_id);
    }
    // Any derived item: descend into its first `dimg` input.
    let inputs = meta.iref_targets(b"dimg", item_id);
    for src in inputs {
        if let Some(leaf) = first_coded_leaf(meta, src, depth + 1) {
            return Some(leaf);
        }
    }
    None
}

/// Build an [`AvifInfo`] for a file whose primary item is a non-grid
/// derived image — an `'iovl'` overlay (HEIF §6.6.2.2), an `'iden'`
/// identity derivation (§6.6.2.1), or a `'tmap'` tone-map / gain-map
/// derivation (av1-avif §4.2.2, whose reconstructed extents are the base
/// input's). Reports the derivation's reconstructed output dimensions
/// (resolved from the box graph) and borrows the representative `av1C`
/// from the first coded leaf in the derivation chain (the base image for a
/// `'tmap'`).
///
/// Mirrors [`build_info_grid`]'s property-fallback shape (primary item
/// first, then the representative coded leaf) for the descriptive
/// properties that describe the reconstructed image (`pixi`, `pasp`,
/// `colr`, HDR metadata).
pub(crate) fn build_info_derived(
    hdr: &AvifHeader<'_>,
    primary_id: u32,
    brands: BrandClass,
    mif1_compliance: crate::derived::Mif1Compliance,
) -> Result<AvifInfo> {
    let idat = hdr.meta.idat.as_deref();
    let (width, height) = crate::derived::reconstructed_dims(&hdr.meta, primary_id, hdr.file, idat)
        .ok_or_else(|| {
            Error::invalid("avif: could not resolve derived primary output dimensions")
        })?;
    // The derived primary's own output image folds in its transformative
    // properties (§6.3).
    let (width, height) =
        crate::derived::output_dims_from_reconstructed(&hdr.meta, primary_id, width, height);

    let leaf_id = first_coded_leaf(&hdr.meta, primary_id, 0)
        .ok_or_else(|| Error::invalid("avif: derived primary has no coded av01 leaf for av1C"))?;
    let av1c = match hdr.meta.property_for(leaf_id, &AV1C) {
        Some(Property::Av1C(bytes)) => bytes.clone(),
        _ => {
            return Err(Error::invalid(
                "avif: derived primary's coded leaf missing av1C property",
            ))
        }
    };
    let bits_per_channel = match hdr.meta.property_for(primary_id, b"pixi") {
        Some(Property::Pixi(p)) => p.bits_per_channel.clone(),
        _ => match hdr.meta.property_for(leaf_id, b"pixi") {
            Some(Property::Pixi(p)) => p.bits_per_channel.clone(),
            _ => Vec::new(),
        },
    };
    let pasp = match hdr.meta.property_for(primary_id, b"pasp") {
        Some(Property::Pasp(p)) => Some(*p),
        _ => match hdr.meta.property_for(leaf_id, b"pasp") {
            Some(Property::Pasp(p)) => Some(*p),
            _ => None,
        },
    };
    let colour = match hdr.meta.property_for(primary_id, &COLR) {
        Some(Property::Colr(c)) => Some(c.clone()),
        _ => match hdr.meta.property_for(leaf_id, &COLR) {
            Some(Property::Colr(c)) => Some(c.clone()),
            _ => None,
        },
    };
    let mdcv = match hdr.meta.property_for(primary_id, &MDCV) {
        Some(Property::Mdcv(m)) => Some(*m),
        _ => match hdr.meta.property_for(leaf_id, &MDCV) {
            Some(Property::Mdcv(m)) => Some(*m),
            _ => None,
        },
    };
    let clli = match hdr.meta.property_for(primary_id, &CLLI) {
        Some(Property::Clli(c)) => Some(*c),
        _ => match hdr.meta.property_for(leaf_id, &CLLI) {
            Some(Property::Clli(c)) => Some(*c),
            _ => None,
        },
    };
    let cclv = match hdr.meta.property_for(primary_id, &CCLV) {
        Some(Property::Cclv(c)) => Some(*c),
        _ => match hdr.meta.property_for(leaf_id, &CCLV) {
            Some(Property::Cclv(c)) => Some(*c),
            _ => None,
        },
    };
    let amve = match hdr.meta.property_for(primary_id, &AMVE) {
        Some(Property::Amve(a)) => Some(*a),
        _ => match hdr.meta.property_for(leaf_id, &AMVE) {
            Some(Property::Amve(a)) => Some(*a),
            _ => None,
        },
    };
    let (bit_depth, monochrome, chroma_subsampling) = decode_av1c_flags(&av1c);
    let thumbnail_item_ids = hdr.meta.iref_sources_of(b"thmb", primary_id);
    let (exif_item_id, xmp_item_id) = resolve_metadata_items(&hdr.meta, primary_id);
    let has_alpha = find_alpha_item_id(&hdr.meta, primary_id).is_some();
    let premultiplied_alpha = if has_alpha {
        match find_alpha_item_id(&hdr.meta, primary_id) {
            Some(alpha_id) => hdr.meta.irefs.iter().any(|e| {
                &e.reference_type == b"prem"
                    && e.from_id == alpha_id
                    && e.to_ids.contains(&primary_id)
            }),
            None => false,
        }
    } else {
        false
    };
    let aux_items = hdr.meta.aux_items_for(primary_id);
    let alpha_aux_kind = aux_items
        .iter()
        .find(|(_, k)| matches!(k, crate::meta::AuxKind::Alpha))
        .map(|(_, k)| *k);
    let depth_map_item_id = aux_items
        .iter()
        .find(|(_, k)| matches!(k, crate::meta::AuxKind::DepthMap))
        .map(|(id, _)| *id);
    let hdr_gain_map_item_id = aux_items
        .iter()
        .find(|(_, k)| matches!(k, crate::meta::AuxKind::HdrGainMap))
        .map(|(id, _)| *id);
    let entity_group_count = hdr.meta.groups().map(|g| g.len()).unwrap_or(0);
    let operating_point = match hdr.meta.property_for(primary_id, b"a1op") {
        Some(Property::A1op(a)) => Some(*a),
        _ => None,
    };
    let layered_index = match hdr.meta.property_for(primary_id, b"a1lx") {
        Some(Property::A1lx(a)) => Some(*a),
        _ => None,
    };
    let sato_item_ids = hdr.meta.item_ids_of_type(&crate::meta::ITEM_TYPE_SATO);
    let tmap_item_ids = hdr.meta.item_ids_of_type(&crate::meta::ITEM_TYPE_TMAP);
    let tone_map_compliance = crate::derived::audit_tone_map(&hdr.meta);
    let grid_derivation_compliance = crate::derived::audit_grid_derivations(&hdr.meta);
    let iden_item_ids = hdr.meta.item_ids_of_type(&crate::meta::ITEM_TYPE_IDEN);
    let iden_compliance = crate::derived::audit_iden_derivations(&hdr.meta);
    let alpha_bit_depth_compliance = crate::derived::audit_alpha_bit_depth(&hdr.meta);
    let sequence_header_obu_compliance =
        crate::derived::audit_sequence_header_obu(&hdr.meta, hdr.file);
    let avif_profile_compliance = crate::derived::audit_avif_profile_compliance(&hdr.meta, &brands);
    let overlay_resolutions = crate::derived::resolve_overlays(&hdr.meta, hdr.file, idat);
    let iden_resolutions = crate::derived::resolve_iden_derivations(&hdr.meta, hdr.file, idat);
    let tone_map_resolutions = crate::derived::resolve_tone_maps(&hdr.meta, hdr.file, idat);
    let grid_resolutions = crate::derived::resolve_grids(&hdr.meta, hdr.file, idat);
    Ok(AvifInfo {
        width,
        height,
        bits_per_channel,
        pasp,
        av1c,
        obu_bytes: Vec::new(),
        is_grid: false,
        has_alpha,
        brands,
        colour,
        mdcv,
        clli,
        cclv,
        amve,
        bit_depth,
        monochrome,
        chroma_subsampling,
        thumbnail_item_ids,
        exif_item_id,
        xmp_item_id,
        premultiplied_alpha,
        aux_items,
        alpha_aux_kind,
        depth_map_item_id,
        hdr_gain_map_item_id,
        entity_group_count,
        mif1_compliance,
        operating_point,
        layered_index,
        sato_item_ids,
        tmap_item_ids,
        tone_map_compliance,
        grid_derivation_compliance,
        iden_item_ids,
        iden_compliance,
        alpha_bit_depth_compliance,
        sequence_header_obu_compliance,
        avif_profile_compliance,
        overlay_resolutions,
        iden_resolutions,
        tone_map_resolutions,
        grid_resolutions,
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

    /// The Microsoft `monochrome.avif` conformance fixture ships a
    /// native Exif metadata item (`item_type == 'Exif'`, id 2) linked
    /// to the primary via a `cdsc` iref — pinning the end-to-end
    /// resolution path (iinf + iref + cdsc enumeration → `exif_item_id`)
    /// on real bytes rather than only on synthetic Meta values. The
    /// same fixture ships no XMP item, no thumbnails, and no `prem`
    /// signal.
    #[test]
    fn inspect_fixture_resolves_native_exif_metadata_item() {
        let info = inspect(FIXTURE).expect("inspect");
        assert!(info.thumbnail_item_ids.is_empty());
        assert_eq!(
            info.exif_item_id,
            Some(2),
            "monochrome.avif fixture must surface its Exif metadata item via cdsc"
        );
        assert!(
            info.xmp_item_id.is_none(),
            "monochrome.avif ships no XMP item"
        );
        assert!(!info.premultiplied_alpha);
        assert!(!info.has_thumbnails());
        assert!(
            info.has_descriptive_metadata(),
            "Exif item presence implies has_descriptive_metadata() is true"
        );
        // And the resolved item bytes can be extracted directly via the
        // crate's public helper. HEIF §A.2.1: the first 4 bytes are a
        // big-endian exif_tiff_header_offset; the rest is a TIFF/Exif
        // blob that opens with the `II` (little-endian) or `MM` (big-
        // endian) TIFF byte-order marker.
        let exif_bytes = item_payload_bytes(FIXTURE, info.exif_item_id.unwrap())
            .expect("extract exif item bytes");
        assert!(
            exif_bytes.len() > 4,
            "exif payload at least carries the 4-byte tiff_header_offset"
        );
        // Per §A.2.1 the offset addresses the TIFF header start inside
        // the payload; the (4 + offset)-th byte onward must begin with
        // the TIFF byte-order marker.
        let off = u32::from_be_bytes(exif_bytes[0..4].try_into().unwrap()) as usize;
        let tiff_start = 4 + off;
        assert!(
            tiff_start + 2 <= exif_bytes.len(),
            "tiff offset {off} fits inside payload of {} bytes",
            exif_bytes.len()
        );
        let bom = &exif_bytes[tiff_start..tiff_start + 2];
        assert!(
            bom == b"II" || bom == b"MM",
            "TIFF header BOM must be II or MM, got {bom:?}"
        );
    }

    use crate::meta::{IrefEntry, ItemInfo};

    fn make_item(id: u32, item_type: &[u8; 4]) -> ItemInfo {
        ItemInfo {
            id,
            item_type: *item_type,
            name: String::new(),
            content_type: None,
            content_encoding: None,
            item_uri_type: None,
            flags: 0,
        }
    }

    fn make_mime_item(id: u32, content_type: &str) -> ItemInfo {
        ItemInfo {
            id,
            item_type: *b"mime",
            name: String::new(),
            content_type: Some(content_type.to_string()),
            content_encoding: None,
            item_uri_type: None,
            flags: 0,
        }
    }

    /// Native Exif item: `item_type == 'Exif'` linked via `cdsc` iref to
    /// the primary. Resolves as Exif.
    #[test]
    fn resolve_metadata_picks_native_exif_item() {
        let meta = Meta {
            items: vec![make_item(2, b"Exif")],
            irefs: vec![IrefEntry {
                reference_type: *b"cdsc",
                from_id: 2,
                to_ids: vec![1],
            }],
            ..Meta::default()
        };
        let (exif, xmp) = resolve_metadata_items(&meta, 1);
        assert_eq!(exif, Some(2));
        assert!(xmp.is_none());
    }

    /// `mime`-wrapped Exif: `item_type == 'mime'` +
    /// `content_type == "application/octet-stream"` (one of the
    /// real-world generic-MIME Exif carriers). Same outcome as native
    /// Exif: resolves as Exif.
    #[test]
    fn resolve_metadata_picks_mime_wrapped_exif() {
        let meta = Meta {
            items: vec![make_mime_item(3, "application/octet-stream")],
            irefs: vec![IrefEntry {
                reference_type: *b"cdsc",
                from_id: 3,
                to_ids: vec![1],
            }],
            ..Meta::default()
        };
        let (exif, xmp) = resolve_metadata_items(&meta, 1);
        assert_eq!(exif, Some(3));
        assert!(xmp.is_none());
    }

    /// XMP item: `mime` + `application/rdf+xml`. Resolves as XMP.
    #[test]
    fn resolve_metadata_picks_xmp_mime_item() {
        let meta = Meta {
            items: vec![make_mime_item(4, "application/rdf+xml")],
            irefs: vec![IrefEntry {
                reference_type: *b"cdsc",
                from_id: 4,
                to_ids: vec![1],
            }],
            ..Meta::default()
        };
        let (exif, xmp) = resolve_metadata_items(&meta, 1);
        assert!(exif.is_none());
        assert_eq!(xmp, Some(4));
    }

    /// A file shipping both Exif and XMP attached to the primary: both
    /// fields populate.
    #[test]
    fn resolve_metadata_picks_both_exif_and_xmp() {
        let meta = Meta {
            items: vec![
                make_item(2, b"Exif"),
                make_mime_item(3, "application/rdf+xml"),
            ],
            irefs: vec![
                IrefEntry {
                    reference_type: *b"cdsc",
                    from_id: 2,
                    to_ids: vec![1],
                },
                IrefEntry {
                    reference_type: *b"cdsc",
                    from_id: 3,
                    to_ids: vec![1],
                },
            ],
            ..Meta::default()
        };
        let (exif, xmp) = resolve_metadata_items(&meta, 1);
        assert_eq!(exif, Some(2));
        assert_eq!(xmp, Some(3));
    }

    /// Case-insensitive content-type matching: "Application/RDF+XML"
    /// still resolves as XMP. Encoders in the wild disagree on
    /// capitalisation.
    #[test]
    fn resolve_metadata_xmp_match_is_case_insensitive() {
        let meta = Meta {
            items: vec![make_mime_item(4, "Application/RDF+XML")],
            irefs: vec![IrefEntry {
                reference_type: *b"cdsc",
                from_id: 4,
                to_ids: vec![1],
            }],
            ..Meta::default()
        };
        let (_, xmp) = resolve_metadata_items(&meta, 1);
        assert_eq!(xmp, Some(4));
    }

    /// An item not linked by `cdsc` does NOT resolve — `iinf` alone is
    /// insufficient, the iref is the binding signal.
    #[test]
    fn resolve_metadata_ignores_items_not_linked_via_cdsc() {
        let meta = Meta {
            items: vec![make_item(2, b"Exif")],
            // No iref — Exif item exists in iinf but isn't linked.
            irefs: vec![],
            ..Meta::default()
        };
        let (exif, xmp) = resolve_metadata_items(&meta, 1);
        assert!(exif.is_none());
        assert!(xmp.is_none());
    }

    /// A `cdsc` iref pointing at a different target does NOT bind to the
    /// primary. The walker is target-scoped.
    #[test]
    fn resolve_metadata_only_targets_the_requested_item() {
        let meta = Meta {
            items: vec![make_item(2, b"Exif")],
            irefs: vec![IrefEntry {
                reference_type: *b"cdsc",
                from_id: 2,
                to_ids: vec![5], // not primary (1)
            }],
            ..Meta::default()
        };
        let (exif, _) = resolve_metadata_items(&meta, 1);
        assert!(exif.is_none());
        // Same iref does bind item 5, however.
        let (exif5, _) = resolve_metadata_items(&meta, 5);
        assert_eq!(exif5, Some(2));
    }

    /// `gain_map_metadata` against an unknown item id surfaces the
    /// "missing in iloc" `InvalidData` error from `item_payload_bytes` —
    /// the resolution stage runs first, so a non-existent tmap id never
    /// reaches the descriptor parser. Pinned against the monochrome
    /// conformance fixture, which has no `'tmap'` item; every id outside
    /// the file's known set is therefore guaranteed to fail at iloc.
    #[test]
    fn gain_map_metadata_unknown_id_is_invalid_data() {
        let err = gain_map_metadata(FIXTURE, 9999).unwrap_err();
        match err {
            Error::InvalidData(msg) => {
                assert!(
                    msg.contains("missing in iloc"),
                    "expected iloc-miss error message, got: {msg}"
                );
            }
            other => panic!("expected InvalidData on unknown item id, got {other:?}"),
        }
    }
}
