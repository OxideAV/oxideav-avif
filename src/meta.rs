//! `meta` box hierarchy parser — ISO/IEC 14496-12 §8.11 + ISO/IEC 23008-12
//! (HEIF) §6 / §9 (item properties). Restricted to the subset AVIF
//! actually populates:
//!
//! * `hdlr` — identifies the meta as a picture collection (`pict`).
//! * `pitm` — primary item id.
//! * `iinf`/`infe` — per-item type (`av01`, `Exif`, aux items, …).
//! * `iloc` — per-item byte offsets + lengths inside `mdat` (or the
//!   file, for `construction_method == 0`).
//! * `iprp`/`ipco`/`ipma` — property store + per-item property
//!   associations. We surface the AVIF-relevant properties directly:
//!   `av1C`, `ispe`, `colr`, `pixi`, `pasp`.

use crate::error::{AvifError as Error, Result};

use crate::box_parser::{
    b, find_box, iter_boxes, parse_box_header, parse_full_box, read_cstr, read_u16, read_u32,
    read_var_uint, type_str, BoxType,
};

const HDLR: BoxType = b(b"hdlr");
const PITM: BoxType = b(b"pitm");
const IINF: BoxType = b(b"iinf");
const INFE: BoxType = b(b"infe");
const ILOC: BoxType = b(b"iloc");
const IPRP: BoxType = b(b"iprp");
const IPCO: BoxType = b(b"ipco");
const IPMA: BoxType = b(b"ipma");
const IREF: BoxType = b(b"iref");
const GRPL: BoxType = b(b"grpl");
const IDAT: BoxType = b(b"idat");

/// HEIF / ISOBMFF item-type four-CC carrying a generic MIME-tagged blob
/// in the `mdat` (Exif/XMP item carriers when the writer chose the
/// `mime` flavour). ISO/IEC 14496-12 §8.11.6.2.
pub const ITEM_TYPE_MIME: BoxType = b(b"mime");
/// HEIF / ISOBMFF item-type four-CC for URI-tagged items. Rare in AVIF
/// but legal per ISO/IEC 14496-12 §8.11.6.2.
pub const ITEM_TYPE_URI: BoxType = b(b"uri ");
/// HEIF item-type four-CC for an Exif metadata payload. The first 4
/// bytes of the resolved item bytes are a big-endian offset to the
/// Exif TIFF header (HEIF §A.2.1); the remaining bytes are a standard
/// TIFF / Exif blob. Files in the wild also wrap Exif as a `mime`
/// item with `content_type == "application/octet-stream"` — both
/// forms are detected.
pub const ITEM_TYPE_EXIF: BoxType = b(b"Exif");

const AV1C: BoxType = b(b"av1C");
const ISPE: BoxType = b(b"ispe");
const COLR: BoxType = b(b"colr");
const PIXI: BoxType = b(b"pixi");
const PASP: BoxType = b(b"pasp");
const IROT: BoxType = b(b"irot");
const IMIR: BoxType = b(b"imir");
const CLAP: BoxType = b(b"clap");
const AUXC: BoxType = b(b"auxC");
const MDCV: BoxType = b(b"mdcv");
const CLLI: BoxType = b(b"clli");
const CCLV: BoxType = b(b"cclv");
/// HEIF §6.5.7 — Relative Location property.
const RLOC: BoxType = b(b"rloc");
/// HEIF §6.5.11 — Layer Selector property.
const LSEL: BoxType = b(b"lsel");
/// av1-avif §2.3.2.1 — Operating Point Selector property.
const A1OP: BoxType = b(b"a1op");
/// av1-avif §2.3.2.3 — AV1 Layered Image Indexing property.
const A1LX: BoxType = b(b"a1lx");

/// HEIF §6.6.2.2 — image overlay derived-image type.
pub const ITEM_TYPE_IOVL: BoxType = b(b"iovl");
/// HEIF §6.6.2.1 — identity-transform derived-image type.
pub const ITEM_TYPE_IDEN: BoxType = b(b"iden");
/// av1-avif v1.2.0 §4.2.3.1 — Sample Transform derived-image type.
pub const ITEM_TYPE_SATO: BoxType = b(b"sato");
/// av1-avif v1.2.0 §4.2.2 — Tone Map derived-image type. The
/// descriptor body itself is defined in HEIF; AVIF reuses it.
pub const ITEM_TYPE_TMAP: BoxType = b(b"tmap");

/// Auxiliary-image type URN classification (HEIF §6.5.8, av1-avif §4).
///
/// The `auxC.aux_type` field is a URN identifying what the auxiliary
/// image represents. The well-known values are:
///
/// * `Alpha` — `urn:mpeg:mpegB:cicp:systems:auxiliary:alpha` (also
///   `urn:mpeg:hevc:2015:auxid:1` from HEVC HEIF). The alpha plane
///   for the colour image referenced by `auxl`.
/// * `DepthMap` — `urn:mpeg:mpegB:cicp:systems:auxiliary:depth` (also
///   `urn:mpeg:hevc:2015:auxid:2`). Per-pixel depth in a monochrome
///   auxiliary item.
/// * `HdrGainMap` — `urn:com:apple:photo:2020:aux:hdrgainmap`. An
///   Apple HDR gain-map auxiliary (proprietary but widely used by
///   iPhone-emitted HEIC files).
/// * `Other` — recognised auxC carrier but the URN is one we don't
///   classify. The raw URN is still available on `AuxC.aux_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuxKind {
    Alpha,
    DepthMap,
    HdrGainMap,
    Other,
}

/// Well-known auxC URN: alpha plane (HEIF §6.5.8 / av1-avif §4.1).
pub const AUX_URN_ALPHA_MPEG: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";
/// Well-known auxC URN: alpha plane (HEVC HEIF flavour).
pub const AUX_URN_ALPHA_HEVC: &str = "urn:mpeg:hevc:2015:auxid:1";
/// Well-known auxC URN: depth map (HEIF §6.5.8).
pub const AUX_URN_DEPTH_MPEG: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:depth";
/// Well-known auxC URN: depth map (HEVC HEIF flavour).
pub const AUX_URN_DEPTH_HEVC: &str = "urn:mpeg:hevc:2015:auxid:2";
/// Apple HDR gain-map auxC URN (proprietary; emitted by iPhone HEIC).
pub const AUX_URN_HDR_GAINMAP: &str = "urn:com:apple:photo:2020:aux:hdrgainmap";

/// One `infe` entry.
///
/// Spec: ISO/IEC 14496-12 §8.11.6 (ItemInfoEntry). Version 2 / 3 entries
/// carry an `item_type` plus, depending on that type, additional fields:
///
/// * `item_type == 'mime'` (HEIF metadata items such as Exif / XMP
///   wrapped as raw bytes): the entry also ships `content_type`
///   (MIME type — `application/rdf+xml` for XMP, `application/octet-stream`
///   for Exif TIFF blobs in some writers) and an optional `content_encoding`
///   (HTTP content-encoding — empty string means raw).
/// * `item_type == 'uri '` (URI metadata items, rare in AVIF): the entry
///   ships an absolute URI in `item_uri_type` that identifies the
///   payload format.
///
/// For every other `item_type` (`av01`, `grid`, `Exif`, `auxl` targets,
/// …) these fields are `None` and the payload bytes (resolved through
/// the matching [`ItemLocation`]) are interpreted by the consumer.
#[derive(Clone, Debug)]
pub struct ItemInfo {
    pub id: u32,
    pub item_type: BoxType,
    pub name: String,
    /// MIME content-type (only populated when `item_type == 'mime'`).
    pub content_type: Option<String>,
    /// Optional content-encoding tag (only populated when
    /// `item_type == 'mime'`; empty string is normalised to `None`).
    pub content_encoding: Option<String>,
    /// Absolute URI type indicator (only populated when
    /// `item_type == 'uri '`).
    pub item_uri_type: Option<String>,
}

/// One `iloc` extent (offset + length pair inside the referenced data
/// blob). AVIF primary items are usually single-extent.
#[derive(Clone, Debug)]
pub struct IlocExtent {
    pub offset: u64,
    pub length: u64,
}

/// One `iloc` entry.
#[derive(Clone, Debug)]
pub struct ItemLocation {
    pub id: u32,
    /// 0 = file offset, 1 = idat offset, 2 = item offset (iref).
    pub construction_method: u8,
    pub data_reference_index: u16,
    pub base_offset: u64,
    pub extents: Vec<IlocExtent>,
}

/// One property association list (for a single item).
#[derive(Clone, Debug)]
pub struct ItemPropertyAssociation {
    pub item_id: u32,
    /// Indices into `ItemPropertyContainer::properties` (1-based in the
    /// spec; we store 0-based). `essential` is carried alongside.
    pub entries: Vec<PropertyAssociation>,
}

#[derive(Clone, Copy, Debug)]
pub struct PropertyAssociation {
    pub index: u16,
    pub essential: bool,
}

/// AVIF image spatial extent (`ispe`): unrotated pixel dimensions.
#[derive(Clone, Copy, Debug)]
pub struct Ispe {
    pub width: u32,
    pub height: u32,
}

/// Pixel information (`pixi`) — per-channel bit depth.
///
/// Spec: HEIF §6.5.6. Carries `num_channels` followed by
/// `bits_per_channel` for each channel. Common AVIF values:
///
///   * monochrome 8-bit: `[8]`
///   * RGB / Y'CbCr 8-bit: `[8, 8, 8]`
///   * 10-bit HDR: `[10, 10, 10]`
///   * 12-bit HDR: `[12, 12, 12]`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixi {
    pub bits_per_channel: Vec<u8>,
}

impl Pixi {
    /// `num_channels` field — equivalent to `bits_per_channel.len()`.
    pub fn num_channels(&self) -> usize {
        self.bits_per_channel.len()
    }

    /// Maximum bit depth across all channels. Returns 0 when the
    /// channel list is empty.
    pub fn max_bit_depth(&self) -> u8 {
        self.bits_per_channel.iter().copied().max().unwrap_or(0)
    }

    /// True when every channel reports the same bit depth — the common
    /// AVIF case. Mixed-depth pixi values are legal per HEIF §6.5.6.3
    /// but vanishingly rare in the wild.
    pub fn is_uniform_depth(&self) -> bool {
        match self.bits_per_channel.first() {
            None => false,
            Some(&first) => self.bits_per_channel.iter().all(|&b| b == first),
        }
    }
}

/// Pixel aspect ratio (`pasp`). Spec: ISO/IEC 14496-12 §8.5.2.1.1
/// (PixelAspectRatioBox), referenced from HEIF §6.5.4. The ratio
/// `h_spacing / v_spacing` is the *horizontal-to-vertical* sample spacing
/// of a single pixel in display geometry. A square-pixel image has
/// `h_spacing == v_spacing` (most commonly `1:1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pasp {
    pub h_spacing: u32,
    pub v_spacing: u32,
}

impl Pasp {
    /// True when h_spacing equals v_spacing — the canonical square-pixel
    /// case. A `pasp` declaring `(0, 0)` (meaningless per the spec) is
    /// reported as non-square so callers don't divide by zero downstream.
    pub fn is_square(&self) -> bool {
        self.h_spacing != 0 && self.v_spacing != 0 && self.h_spacing == self.v_spacing
    }

    /// Pixel aspect ratio as an `f64`. Returns `None` if v_spacing is 0
    /// (would otherwise divide by zero).
    pub fn ratio(&self) -> Option<f64> {
        if self.v_spacing == 0 {
            None
        } else {
            Some(self.h_spacing as f64 / self.v_spacing as f64)
        }
    }
}

/// Colour information (`colr`): NCLX or ICC bytes as-is.
#[derive(Clone, Debug)]
pub enum Colr {
    Nclx {
        colour_primaries: u16,
        transfer_characteristics: u16,
        matrix_coefficients: u16,
        full_range: bool,
    },
    Icc(Vec<u8>),
    Unknown(BoxType),
}

/// Image rotation (`irot`) — counter-clockwise, 0..3 × 90°.
#[derive(Clone, Copy, Debug)]
pub struct Irot {
    pub angle: u8,
}

/// Image mirror (`imir`). `axis == 0` flips about the horizontal axis
/// (top↔bottom); `axis == 1` flips about the vertical axis (left↔right).
/// This follows AVIF 1.1 / HEIF convention.
#[derive(Clone, Copy, Debug)]
pub struct Imir {
    pub axis: u8,
}

/// Clean aperture (`clap`). Eight 32-bit signed rationals (num/den pairs)
/// describing crop width, crop height, horizontal offset, vertical offset.
#[derive(Clone, Copy, Debug)]
pub struct Clap {
    pub clean_aperture_width_n: i32,
    pub clean_aperture_width_d: i32,
    pub clean_aperture_height_n: i32,
    pub clean_aperture_height_d: i32,
    pub horiz_off_n: i32,
    pub horiz_off_d: i32,
    pub vert_off_n: i32,
    pub vert_off_d: i32,
}

/// Auxiliary item type (`auxC`) — carries a NUL-terminated URN identifying
/// the auxiliary use. For AVIF alpha this is
/// `urn:mpeg:mpegB:cicp:systems:auxiliary:alpha`.
#[derive(Clone, Debug)]
pub struct AuxC {
    pub aux_type: String,
    pub aux_subtype: Vec<u8>,
}

impl AuxC {
    /// Classify the `aux_type` URN into one of the well-known auxiliary
    /// kinds. Returns [`AuxKind::Other`] when the URN doesn't match any
    /// of the alpha / depth / HDR-gain-map entries we recognise; the raw
    /// URN remains available on `aux_type`.
    ///
    /// Matching is exact (no prefix-only matches) so a writer that
    /// extends `urn:mpeg:mpegB:cicp:systems:auxiliary:alpha` with a
    /// trailing tag won't be misclassified as plain alpha.
    pub fn kind(&self) -> AuxKind {
        match self.aux_type.as_str() {
            AUX_URN_ALPHA_MPEG | AUX_URN_ALPHA_HEVC => AuxKind::Alpha,
            AUX_URN_DEPTH_MPEG | AUX_URN_DEPTH_HEVC => AuxKind::DepthMap,
            AUX_URN_HDR_GAINMAP => AuxKind::HdrGainMap,
            _ => AuxKind::Other,
        }
    }

    /// True when this auxC describes an alpha plane (either the MPEG
    /// or HEVC URN spelling).
    pub fn is_alpha(&self) -> bool {
        matches!(self.kind(), AuxKind::Alpha)
    }

    /// True when this auxC describes a depth map.
    pub fn is_depth_map(&self) -> bool {
        matches!(self.kind(), AuxKind::DepthMap)
    }

    /// True when this auxC declares Apple's HDR gain-map URN.
    pub fn is_hdr_gain_map(&self) -> bool {
        matches!(self.kind(), AuxKind::HdrGainMap)
    }
}

/// Relative-location item property (`rloc`) — HEIF §6.5.7.
/// Specifies horizontal + vertical offsets in pixels of the associated
/// image item's reconstructed pixels inside a related image item's
/// pixel grid. The "related" item is conventionally the one that has
/// this item as a `dimg` derived input (e.g. a tile inside its grid).
///
/// Spec: ISO/IEC 23008-12 §6.5.7.2 — `unsigned int(32) horizontal_offset;`
/// + `unsigned int(32) vertical_offset;` inside a FullBox header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rloc {
    pub horizontal_offset: u32,
    pub vertical_offset: u32,
}

/// Layer-selector item property (`lsel`) — HEIF §6.5.11.
/// Picks one reconstructed image among several produced by a multi-layer
/// image item. `essential` shall be 1 for an `lsel` property; if the
/// reader cannot interpret it, the item is considered unrecognised.
///
/// Spec: ISO/IEC 23008-12 §6.5.11.2 — `unsigned int(16) layer_id;`
/// inside an ItemProperty (no FullBox header).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lsel {
    pub layer_id: u16,
}

/// Operating Point Selector item property (`a1op`) — av1-avif §2.3.2.1.
///
/// Selects which AV1 operating point the reader should process for a
/// scalable / multi-layer AV1 Image Item. The spec mandates that when
/// this property is associated it **shall be marked as essential**, so a
/// reader that cannot honour the selected operating point must not
/// process the item (av1-avif §2.3.2.1.2 + MIAF §7.3.5 essential-property
/// semantics).
///
/// Syntax: `ItemProperty('a1op')` (NO FullBox header) carrying a single
/// `unsigned int(8) op_index`. `op_index` shall be in
/// `0..=operating_points_cnt_minus_1` of the AV1 sequence header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct A1op {
    /// Index of the operating point to be processed for this item.
    pub op_index: u8,
}

/// AV1 Layered Image Indexing item property (`a1lx`) — av1-avif §2.3.2.3.
///
/// Documents the size in bytes of each layer (except the last) inside
/// the AV1 Image Item Data, so a reader can determine the byte ranges
/// needed to process one or more layers of an operating point without
/// parsing the OBU stream. The spec mandates this property **shall not
/// be marked as essential** — a reader that ignores it can still decode
/// the full item.
///
/// Syntax: `ItemProperty('a1lx')` (NO FullBox header):
///
/// ```text
/// unsigned int(7) reserved = 0;
/// unsigned int(1) large_size;
/// FieldLength = (large_size + 1) * 16;
/// unsigned int(FieldLength) layer_size[3];
/// ```
///
/// `layer_size` values are in increasing order of `spatial_id`. A value
/// of zero terminates the list — all following values shall also be 0
/// (av1-avif §2.3.2.3.4). The size of the final layer is implicit (item
/// payload length minus the documented prefix), so it is never stored
/// here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct A1lx {
    /// `false` → 16-bit `layer_size` fields; `true` → 32-bit fields.
    pub large_size: bool,
    /// Byte size of layers 0..2 (in increasing `spatial_id` order). A
    /// zero entry, and every entry after it, is unused (the layer is
    /// either absent or the final, implicitly-sized layer).
    pub layer_size: [u32; 3],
}

impl A1lx {
    /// Number of documented (non-zero, leading) layer sizes. Per
    /// av1-avif §2.3.2.3.4 a zero entry terminates the list, so this
    /// counts the leading run of non-zero values. Equals
    /// `(number of layers in the image) - 1`.
    pub fn documented_layers(&self) -> usize {
        self.layer_size.iter().take_while(|&&s| s != 0).count()
    }
}

/// Mastering display colour volume (`mdcv`) — SMPTE ST 2086 / CTA-861-G
/// HDR metadata. Spec: ISO/IEC 14496-12 §12.1.5.3 (MasteringDisplayColourVolumeBox).
///
/// `display_primaries_xy` is `[(Rx,Ry), (Gx,Gy), (Bx,By)]` in
/// `u16` units of `1/50000` (CIE 1931). `white_point_xy` is the
/// white-point in the same units. Luminance values are in `u32` units
/// of `1/10000 cd/m²`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mdcv {
    /// Display primaries (R, G, B) in chromaticity units × 50000.
    pub display_primaries_xy: [(u16, u16); 3],
    /// White point in chromaticity units × 50000.
    pub white_point_xy: (u16, u16),
    /// Maximum display luminance in units of 1/10000 cd/m².
    pub max_display_mastering_luminance: u32,
    /// Minimum display luminance in units of 1/10000 cd/m².
    pub min_display_mastering_luminance: u32,
}

/// Content light level info (`clli`) — maximum frame-average and
/// maximum content light levels. Spec: ISO/IEC 14496-12 §12.1.5.4
/// (ContentLightLevelBox). Both values are in cd/m².
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clli {
    /// Max content light level (MaxCLL) in cd/m².
    pub max_content_light_level: u16,
    /// Max frame-average light level (MaxFALL) in cd/m².
    pub max_pic_average_light_level: u16,
}

/// Colour volume luminance (`cclv`) — supplemental HDR luminance hint.
/// Spec: AOM AV1 Metadata OBU HDR dynamic metadata / AVIF extension
/// (draft av1-avif §9.4). Carries `max_cll` + `max_fall` in the same
/// binary layout as `clli` but in the `ipco` item-property plane.
///
/// Encoders that implement the draft sometimes write `cclv` alongside
/// or in place of `clli`; both carry identical semantics — treat them
/// the same downstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cclv {
    /// Max content light level in cd/m².
    pub max_content_light_level: u16,
    /// Max frame-average light level in cd/m².
    pub max_pic_average_light_level: u16,
}

/// One property box, kept typed for the boxes AVIF cares about + a raw
/// fallback so an unknown property still gets an index for association.
#[derive(Clone, Debug)]
pub enum Property {
    Av1C(Vec<u8>),
    Ispe(Ispe),
    Colr(Colr),
    Pixi(Pixi),
    Pasp(Pasp),
    Irot(Irot),
    Imir(Imir),
    Clap(Clap),
    AuxC(AuxC),
    /// Mastering display colour volume (SMPTE ST 2086 HDR metadata).
    Mdcv(Mdcv),
    /// Content light level info (MaxCLL / MaxFALL in cd/m²).
    Clli(Clli),
    /// Colour volume luminance — draft AVIF supplement equivalent to `clli`.
    Cclv(Cclv),
    /// Relative-location property (HEIF §6.5.7).
    Rloc(Rloc),
    /// Layer-selector property (HEIF §6.5.11).
    Lsel(Lsel),
    /// Operating-point selector property (av1-avif §2.3.2.1).
    A1op(A1op),
    /// AV1 layered-image indexing property (av1-avif §2.3.2.3).
    A1lx(A1lx),
    Other(BoxType, Vec<u8>),
}

impl Property {
    pub fn kind(&self) -> BoxType {
        match self {
            Property::Av1C(_) => AV1C,
            Property::Ispe(_) => ISPE,
            Property::Colr(_) => COLR,
            Property::Pixi(_) => PIXI,
            Property::Pasp(_) => PASP,
            Property::Irot(_) => IROT,
            Property::Imir(_) => IMIR,
            Property::Clap(_) => CLAP,
            Property::AuxC(_) => AUXC,
            Property::Mdcv(_) => MDCV,
            Property::Clli(_) => CLLI,
            Property::Cclv(_) => CCLV,
            Property::Rloc(_) => RLOC,
            Property::Lsel(_) => LSEL,
            Property::A1op(_) => A1OP,
            Property::A1lx(_) => A1LX,
            Property::Other(t, _) => *t,
        }
    }
}

/// One entry in `iref` — a typed reference whose `from_id` is the source
/// item and `to_ids` is the list of target items (e.g. `dimg` -> tile
/// items for a grid, `auxl` -> alpha item).
#[derive(Clone, Debug)]
pub struct IrefEntry {
    pub reference_type: BoxType,
    pub from_id: u32,
    pub to_ids: Vec<u32>,
}

/// Everything we pulled out of `meta`.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub handler: Option<BoxType>,
    pub primary_item_id: Option<u32>,
    pub items: Vec<ItemInfo>,
    pub locations: Vec<ItemLocation>,
    pub properties: Vec<Property>,
    pub associations: Vec<ItemPropertyAssociation>,
    pub irefs: Vec<IrefEntry>,
    /// Raw `grpl` (GroupsListBox) payload bytes when present, ready for
    /// [`crate::derived::parse_grpl`] consumption. We don't eagerly parse
    /// because most AVIF files don't ship a `grpl` and the parsed
    /// representation belongs in a callers-need-it module. Spec:
    /// ISO/IEC 23008-12 §9.4.2.
    pub grpl: Option<Vec<u8>>,
    /// Raw `idat` (ItemDataBox) payload bytes when present. Used by
    /// derived items (overlay, grid) whose descriptor lives in `idat`
    /// rather than `mdat`. Spec: ISO/IEC 14496-12 §8.11.11.
    pub idat: Option<Vec<u8>>,
}

impl Meta {
    /// Parse the raw payload of the top-level `meta` box (i.e. the bytes
    /// *after* its 4-byte FullBox prefix).
    pub fn parse(meta_payload: &[u8]) -> Result<Self> {
        let (_version, _flags, body) = parse_full_box(meta_payload)?;
        let mut me = Meta::default();
        for hdr in iter_boxes(body) {
            let hdr = hdr?;
            let payload = &body[hdr.payload_start..hdr.end()];
            match &hdr.box_type {
                x if x == &HDLR => {
                    me.handler = Some(parse_hdlr(payload)?);
                }
                x if x == &PITM => {
                    me.primary_item_id = Some(parse_pitm(payload)?);
                }
                x if x == &IINF => {
                    me.items = parse_iinf(payload)?;
                }
                x if x == &ILOC => {
                    me.locations = parse_iloc(payload)?;
                }
                x if x == &IPRP => {
                    let (props, assocs) = parse_iprp(payload)?;
                    me.properties = props;
                    me.associations = assocs;
                }
                x if x == &IREF => {
                    me.irefs = parse_iref(payload)?;
                }
                x if x == &GRPL => {
                    me.grpl = Some(payload.to_vec());
                }
                x if x == &IDAT => {
                    me.idat = Some(payload.to_vec());
                }
                _ => {}
            }
        }
        Ok(me)
    }

    /// Return the list of target item IDs referenced from `from_id` via
    /// an iref entry of the given `reference_type` (e.g. `b"dimg"` for
    /// grid tiles, `b"auxl"` for alpha auxiliaries).
    pub fn iref_targets(&self, reference_type: &BoxType, from_id: u32) -> Vec<u32> {
        for e in &self.irefs {
            if &e.reference_type == reference_type && e.from_id == from_id {
                return e.to_ids.clone();
            }
        }
        Vec::new()
    }

    /// Return the source of the first iref of `reference_type` whose
    /// `to_ids` contains `to_id`. Useful for finding the alpha auxiliary
    /// that points at the primary item via `auxl`.
    pub fn iref_source_of(&self, reference_type: &BoxType, to_id: u32) -> Option<u32> {
        for e in &self.irefs {
            if &e.reference_type == reference_type && e.to_ids.contains(&to_id) {
                return Some(e.from_id);
            }
        }
        None
    }

    /// Return every source of an iref of `reference_type` whose
    /// `to_ids` contains `to_id`. Multiple iref entries can point at a
    /// single item (e.g. several thumbnails of one master image), so a
    /// list is returned. Returns `Vec::new()` when nothing matches.
    pub fn iref_sources_of(&self, reference_type: &BoxType, to_id: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for e in &self.irefs {
            if &e.reference_type == reference_type && e.to_ids.contains(&to_id) {
                out.push(e.from_id);
            }
        }
        out
    }

    /// True when the alpha auxiliary attached to `to_id` is signalled as
    /// premultiplied per HEIF iref type `prem`. The `prem` iref's
    /// `from_id` is the alpha item and `to_ids` contains the colour
    /// image. Spec: ISO/IEC 23008-12 (HEIF) §6.10.1.1 — `prem` is the
    /// canonical signal that the colour values have been premultiplied
    /// by the alpha.
    pub fn is_alpha_premultiplied_for(&self, to_id: u32) -> bool {
        const PREM: BoxType = b(b"prem");
        self.iref_source_of(&PREM, to_id).is_some()
    }

    pub fn item_by_id(&self, id: u32) -> Option<&ItemInfo> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn location_by_id(&self, id: u32) -> Option<&ItemLocation> {
        self.locations.iter().find(|l| l.id == id)
    }

    pub fn assoc_by_id(&self, id: u32) -> Option<&ItemPropertyAssociation> {
        self.associations.iter().find(|a| a.item_id == id)
    }

    /// Return the first property of `kind` associated with `item_id`.
    pub fn property_for<'a>(&'a self, item_id: u32, kind: &BoxType) -> Option<&'a Property> {
        let assoc = self.assoc_by_id(item_id)?;
        for pa in &assoc.entries {
            let prop = self.properties.get(pa.index as usize)?;
            if &prop.kind() == kind {
                return Some(prop);
            }
        }
        None
    }

    /// Enumerate the box types of every property associated with `item_id`
    /// that is **marked essential** but lands in [`Property::Other`] —
    /// i.e. an essential property this crate does not interpret.
    ///
    /// Per av1-avif §2.3.2.1.2 and MIAF (ISO/IEC 23000-22) §7.3.5, a
    /// reader that encounters an item with an essential item property it
    /// does not support shall not process that item. This helper lets the
    /// decode path consult that rule without re-walking associations:
    /// every returned `BoxType` is a 4CC the caller's pipeline could not
    /// honour. An empty vector means the item is safe to process (every
    /// essential property is recognised, or all unknown properties are
    /// non-essential and may be ignored).
    ///
    /// `a1lx` is treated as recognised even when its bytes are not acted
    /// upon, because the spec forbids marking it essential; a `clap`,
    /// `irot`, `imir`, `lsel`, `a1op`, etc. that we parse counts as
    /// recognised regardless of the essential bit.
    pub fn unsupported_essential_properties(&self, item_id: u32) -> Vec<BoxType> {
        let Some(assoc) = self.assoc_by_id(item_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for pa in &assoc.entries {
            if !pa.essential {
                continue;
            }
            match self.properties.get(pa.index as usize) {
                // An unknown property carried as `Other` and flagged
                // essential is exactly the case the spec guards against.
                Some(Property::Other(t, _)) => out.push(*t),
                // A property index that points past the container is
                // malformed; treat the missing essential property as
                // unsupported (its 4CC is unknowable, use zeros).
                None => out.push([0, 0, 0, 0]),
                // Any typed property is one we recognise and can honour
                // (or safely ignore if non-transformative).
                Some(_) => {}
            }
        }
        out
    }

    /// True when `item_id` carries an essential item property this crate
    /// cannot interpret (see [`Meta::unsupported_essential_properties`]).
    pub fn has_unsupported_essential_property(&self, item_id: u32) -> bool {
        !self.unsupported_essential_properties(item_id).is_empty()
    }

    /// Parse the raw `grpl` slice into typed entity groups. Returns
    /// `Ok(Vec::new())` when the file has no `grpl` (the common case);
    /// surfaces parse errors only when the box is malformed.
    ///
    /// Spec: ISO/IEC 23008-12 §9.4.
    pub fn groups(&self) -> Result<Vec<crate::derived::EntityGroup>> {
        match &self.grpl {
            None => Ok(Vec::new()),
            Some(bytes) => crate::derived::parse_grpl(bytes),
        }
    }

    /// Enumerate every auxiliary item attached to `to_id` via an `auxl`
    /// iref, paired with its classified [`AuxKind`]. Items that lack
    /// an `auxC` property (malformed) are skipped silently — they
    /// can't be routed without the URN.
    ///
    /// Spec: HEIF §6.5.8 + ISO/IEC 14496-12 §8.11.12. The `auxl`
    /// iref's `from_id` is the auxiliary item; its `to_ids` lists the
    /// image(s) the auxiliary describes.
    pub fn aux_items_for(&self, to_id: u32) -> Vec<(u32, AuxKind)> {
        const AUXL: BoxType = b(b"auxl");
        const AUXC_KIND: BoxType = b(b"auxC");
        let mut out = Vec::new();
        for src in self.iref_sources_of(&AUXL, to_id) {
            if let Some(Property::AuxC(aux)) = self.property_for(src, &AUXC_KIND) {
                out.push((src, aux.kind()));
            }
        }
        out
    }

    /// Item IDs whose `infe` declares `item_type` matching `target`.
    /// Useful for enumerating derived-image carriers (e.g. `sato`,
    /// `iovl`, `iden`, `grid`, `tmap`) without rewalking the meta
    /// item list manually.
    pub fn item_ids_of_type(&self, target: &BoxType) -> Vec<u32> {
        self.items
            .iter()
            .filter(|it| it.item_type == *target)
            .map(|it| it.id)
            .collect()
    }
}

fn parse_hdlr(payload: &[u8]) -> Result<BoxType> {
    let (_v, _f, body) = parse_full_box(payload)?;
    // body layout: pre_defined(4) + handler_type(4) + reserved(12) + name(str)
    if body.len() < 8 {
        return Err(Error::invalid("avif: hdlr too short"));
    }
    let mut t = [0u8; 4];
    t.copy_from_slice(&body[4..8]);
    Ok(t)
}

fn parse_pitm(payload: &[u8]) -> Result<u32> {
    let (version, _flags, body) = parse_full_box(payload)?;
    if version == 0 {
        if body.len() < 2 {
            return Err(Error::invalid("avif: pitm too short"));
        }
        Ok(read_u16(body, 0)? as u32)
    } else {
        if body.len() < 4 {
            return Err(Error::invalid("avif: pitm v1 too short"));
        }
        read_u32(body, 0)
    }
}

fn parse_iinf(payload: &[u8]) -> Result<Vec<ItemInfo>> {
    let (version, _flags, body) = parse_full_box(payload)?;
    let (count, mut cursor) = if version == 0 {
        (read_u16(body, 0)? as u32, 2)
    } else {
        (read_u32(body, 0)?, 4)
    };
    let mut out = Vec::with_capacity(count as usize);
    // Each child is an `infe` box.
    while out.len() < count as usize {
        if cursor >= body.len() {
            return Err(Error::invalid("avif: iinf ran off end"));
        }
        let hdr = parse_box_header(body, cursor)?;
        if hdr.box_type != INFE {
            return Err(Error::invalid(format!(
                "avif: iinf child '{}' != infe",
                type_str(&hdr.box_type)
            )));
        }
        let infe_payload = &body[hdr.payload_start..hdr.end()];
        out.push(parse_infe(infe_payload)?);
        cursor = hdr.end();
    }
    Ok(out)
}

fn parse_infe(payload: &[u8]) -> Result<ItemInfo> {
    let (version, _flags, body) = parse_full_box(payload)?;
    // Versions 2 and 3 are the ones used by AVIF / HEIF. Version 0/1
    // predate item_type and aren't legal for image items.
    let (id, _protection_index, item_type, mut cursor) = match version {
        2 => {
            if body.len() < 8 {
                return Err(Error::invalid("avif: infe v2 too short"));
            }
            let id = read_u16(body, 0)? as u32;
            let protection_index = read_u16(body, 2)?;
            let mut t = [0u8; 4];
            t.copy_from_slice(&body[4..8]);
            (id, protection_index, t, 8usize)
        }
        3 => {
            if body.len() < 10 {
                return Err(Error::invalid("avif: infe v3 too short"));
            }
            let id = read_u32(body, 0)?;
            let protection_index = read_u16(body, 4)?;
            let mut t = [0u8; 4];
            t.copy_from_slice(&body[6..10]);
            (id, protection_index, t, 10usize)
        }
        v => {
            return Err(Error::invalid(format!(
                "avif: unsupported infe version {v}"
            )))
        }
    };
    let (name, next) = read_cstr(body, cursor)?;
    cursor = next;
    // ISO/IEC 14496-12 §8.11.6.2 ItemInfoEntry syntax: for v2/v3 the
    // tail of the box carries type-dependent fields. `mime` items ship
    // `content_type` then optional `content_encoding`; `uri ` items
    // ship `item_uri_type`. Every other type stops after `item_name`.
    let (content_type, content_encoding, item_uri_type) = match &item_type {
        x if x == &ITEM_TYPE_MIME => {
            // content_type is mandatory for 'mime'; content_encoding is
            // optional — when the box ends after content_type the field
            // is treated as absent (§8.11.6.3: an explicit empty string
            // means "no encoding", we collapse that to None for parity
            // so callers don't have to special-case the empty case).
            let (ct, after_ct) = read_cstr(body, cursor)?;
            cursor = after_ct;
            let ce = if cursor < body.len() {
                let (raw, after_ce) = read_cstr(body, cursor)?;
                cursor = after_ce;
                if raw.is_empty() {
                    None
                } else {
                    Some(raw)
                }
            } else {
                None
            };
            (Some(ct), ce, None)
        }
        x if x == &ITEM_TYPE_URI => {
            let (u, after_u) = read_cstr(body, cursor)?;
            cursor = after_u;
            (None, None, Some(u))
        }
        _ => (None, None, None),
    };
    let _ = cursor;
    Ok(ItemInfo {
        id,
        item_type,
        name,
        content_type,
        content_encoding,
        item_uri_type,
    })
}

fn parse_iloc(payload: &[u8]) -> Result<Vec<ItemLocation>> {
    let (version, _flags, body) = parse_full_box(payload)?;
    if body.len() < 2 {
        return Err(Error::invalid("avif: iloc too short"));
    }
    let b0 = body[0];
    let b1 = body[1];
    let offset_size = (b0 >> 4) as usize;
    let length_size = (b0 & 0x0f) as usize;
    let base_offset_size = (b1 >> 4) as usize;
    // v1/v2 also carry index_size in the low nibble; v0 reserved.
    let index_size = if version == 1 || version == 2 {
        (b1 & 0x0f) as usize
    } else {
        0
    };
    let mut cursor = 2usize;
    let item_count = match version {
        0 | 1 => {
            let v = read_u16(body, cursor)? as u32;
            cursor += 2;
            v
        }
        2 => {
            let v = read_u32(body, cursor)?;
            cursor += 4;
            v
        }
        v => return Err(Error::invalid(format!("avif: iloc version {v}"))),
    };
    let mut out = Vec::with_capacity(item_count as usize);
    for _ in 0..item_count {
        // item_id sizing differs by version.
        let item_id = match version {
            0 | 1 => {
                let v = read_u16(body, cursor)? as u32;
                cursor += 2;
                v
            }
            2 => {
                let v = read_u32(body, cursor)?;
                cursor += 4;
                v
            }
            _ => unreachable!(),
        };
        let construction_method = if version == 1 || version == 2 {
            // reserved(12) + construction_method(4), big-endian across 2B.
            let w = read_u16(body, cursor)?;
            cursor += 2;
            (w & 0x0f) as u8
        } else {
            0
        };
        let data_reference_index = read_u16(body, cursor)?;
        cursor += 2;
        let base_offset = read_var_uint(body, cursor, base_offset_size)?;
        cursor += base_offset_size;
        let extent_count = read_u16(body, cursor)?;
        cursor += 2;
        let mut extents = Vec::with_capacity(extent_count as usize);
        for _ in 0..extent_count {
            // v1/v2: optional extent_index before offset/length.
            if (version == 1 || version == 2) && index_size > 0 {
                cursor += index_size;
            }
            let offset = read_var_uint(body, cursor, offset_size)?;
            cursor += offset_size;
            let length = read_var_uint(body, cursor, length_size)?;
            cursor += length_size;
            extents.push(IlocExtent { offset, length });
        }
        out.push(ItemLocation {
            id: item_id,
            construction_method,
            data_reference_index,
            base_offset,
            extents,
        });
    }
    Ok(out)
}

fn parse_iprp(payload: &[u8]) -> Result<(Vec<Property>, Vec<ItemPropertyAssociation>)> {
    // iprp is a plain Box containing ipco then one or more ipma.
    let (ipco_payload, _) =
        find_box(payload, &IPCO)?.ok_or_else(|| Error::invalid("avif: iprp missing ipco"))?;
    let properties = parse_ipco(ipco_payload)?;
    let mut assocs = Vec::new();
    // Multiple ipma boxes may appear; walk them all.
    for hdr in iter_boxes(payload) {
        let hdr = hdr?;
        if hdr.box_type == IPMA {
            let p = &payload[hdr.payload_start..hdr.end()];
            assocs.extend(parse_ipma(p)?);
        }
    }
    Ok((properties, assocs))
}

fn parse_ipco(payload: &[u8]) -> Result<Vec<Property>> {
    let mut out = Vec::new();
    for hdr in iter_boxes(payload) {
        let hdr = hdr?;
        let body = &payload[hdr.payload_start..hdr.end()];
        let prop = match &hdr.box_type {
            x if x == &AV1C => Property::Av1C(body.to_vec()),
            x if x == &ISPE => Property::Ispe(parse_ispe(body)?),
            x if x == &COLR => Property::Colr(parse_colr(body)?),
            x if x == &PIXI => Property::Pixi(parse_pixi(body)?),
            x if x == &PASP => Property::Pasp(parse_pasp(body)?),
            x if x == &IROT => Property::Irot(parse_irot(body)?),
            x if x == &IMIR => Property::Imir(parse_imir(body)?),
            x if x == &CLAP => Property::Clap(parse_clap(body)?),
            x if x == &AUXC => Property::AuxC(parse_auxc(body)?),
            x if x == &MDCV => Property::Mdcv(parse_mdcv(body)?),
            x if x == &CLLI => Property::Clli(parse_clli(body)?),
            x if x == &CCLV => Property::Cclv(parse_cclv(body)?),
            x if x == &RLOC => Property::Rloc(parse_rloc(body)?),
            x if x == &LSEL => Property::Lsel(parse_lsel(body)?),
            x if x == &A1OP => Property::A1op(parse_a1op(body)?),
            x if x == &A1LX => Property::A1lx(parse_a1lx(body)?),
            other => Property::Other(*other, body.to_vec()),
        };
        out.push(prop);
    }
    Ok(out)
}

fn parse_ispe(body: &[u8]) -> Result<Ispe> {
    let (_v, _f, rest) = parse_full_box(body)?;
    if rest.len() < 8 {
        return Err(Error::invalid("avif: ispe too short"));
    }
    Ok(Ispe {
        width: read_u32(rest, 0)?,
        height: read_u32(rest, 4)?,
    })
}

fn parse_colr(body: &[u8]) -> Result<Colr> {
    if body.len() < 4 {
        return Err(Error::invalid("avif: colr too short"));
    }
    let mut tag = [0u8; 4];
    tag.copy_from_slice(&body[..4]);
    match &tag {
        b"nclx" => {
            if body.len() < 4 + 7 {
                return Err(Error::invalid("avif: colr nclx too short"));
            }
            let colour_primaries = read_u16(body, 4)?;
            let transfer_characteristics = read_u16(body, 6)?;
            let matrix_coefficients = read_u16(body, 8)?;
            let full_range = (body[10] & 0x80) != 0;
            Ok(Colr::Nclx {
                colour_primaries,
                transfer_characteristics,
                matrix_coefficients,
                full_range,
            })
        }
        b"rICC" | b"prof" => Ok(Colr::Icc(body[4..].to_vec())),
        other => Ok(Colr::Unknown(*other)),
    }
}

fn parse_pixi(body: &[u8]) -> Result<Pixi> {
    let (_v, _f, rest) = parse_full_box(body)?;
    if rest.is_empty() {
        return Err(Error::invalid("avif: pixi too short"));
    }
    let n = rest[0] as usize;
    if rest.len() < 1 + n {
        return Err(Error::invalid("avif: pixi channels truncated"));
    }
    Ok(Pixi {
        bits_per_channel: rest[1..1 + n].to_vec(),
    })
}

fn parse_pasp(body: &[u8]) -> Result<Pasp> {
    if body.len() < 8 {
        return Err(Error::invalid("avif: pasp too short"));
    }
    Ok(Pasp {
        h_spacing: read_u32(body, 0)?,
        v_spacing: read_u32(body, 4)?,
    })
}

fn parse_irot(body: &[u8]) -> Result<Irot> {
    if body.is_empty() {
        return Err(Error::invalid("avif: irot empty"));
    }
    Ok(Irot {
        angle: body[0] & 0x03,
    })
}

fn parse_imir(body: &[u8]) -> Result<Imir> {
    if body.is_empty() {
        return Err(Error::invalid("avif: imir empty"));
    }
    Ok(Imir {
        axis: body[0] & 0x01,
    })
}

fn parse_clap(body: &[u8]) -> Result<Clap> {
    if body.len() < 32 {
        return Err(Error::invalid("avif: clap too short"));
    }
    Ok(Clap {
        clean_aperture_width_n: read_u32(body, 0)? as i32,
        clean_aperture_width_d: read_u32(body, 4)? as i32,
        clean_aperture_height_n: read_u32(body, 8)? as i32,
        clean_aperture_height_d: read_u32(body, 12)? as i32,
        horiz_off_n: read_u32(body, 16)? as i32,
        horiz_off_d: read_u32(body, 20)? as i32,
        vert_off_n: read_u32(body, 24)? as i32,
        vert_off_d: read_u32(body, 28)? as i32,
    })
}

fn parse_auxc(body: &[u8]) -> Result<AuxC> {
    let (_v, _f, rest) = parse_full_box(body)?;
    let (aux_type, next) = read_cstr(rest, 0)?;
    let aux_subtype = rest.get(next..).unwrap_or(&[]).to_vec();
    Ok(AuxC {
        aux_type,
        aux_subtype,
    })
}

/// Parse `mdcv` (MasteringDisplayColourVolumeBox). Layout per ISO/IEC 14496-12
/// §12.1.5.3: 6 × u16 chromaticity values (Rx,Ry,Gx,Gy,Bx,By in units of
/// 1/50000 CIE 1931) + 2 × u16 white point + 2 × u32 luminance (max/min in
/// units of 1/10000 cd/m²). Total: 24 bytes, no FullBox header.
fn parse_mdcv(body: &[u8]) -> Result<Mdcv> {
    if body.len() < 24 {
        return Err(Error::invalid(format!(
            "avif: mdcv too short ({} < 24)",
            body.len()
        )));
    }
    let rx = read_u16(body, 0)?;
    let ry = read_u16(body, 2)?;
    let gx = read_u16(body, 4)?;
    let gy = read_u16(body, 6)?;
    let bx = read_u16(body, 8)?;
    let by_ = read_u16(body, 10)?;
    let wx = read_u16(body, 12)?;
    let wy = read_u16(body, 14)?;
    let max_lum = read_u32(body, 16)?;
    let min_lum = read_u32(body, 20)?;
    Ok(Mdcv {
        display_primaries_xy: [(rx, ry), (gx, gy), (bx, by_)],
        white_point_xy: (wx, wy),
        max_display_mastering_luminance: max_lum,
        min_display_mastering_luminance: min_lum,
    })
}

/// Parse `clli` (ContentLightLevelBox). Layout per ISO/IEC 14496-12
/// §12.1.5.4: two u16 values — MaxCLL and MaxFALL in cd/m². No FullBox header.
fn parse_clli(body: &[u8]) -> Result<Clli> {
    if body.len() < 4 {
        return Err(Error::invalid(format!(
            "avif: clli too short ({} < 4)",
            body.len()
        )));
    }
    Ok(Clli {
        max_content_light_level: read_u16(body, 0)?,
        max_pic_average_light_level: read_u16(body, 2)?,
    })
}

/// Parse `cclv` (ColourVolumeLuminanceBox — draft av1-avif extension).
/// Same binary layout as `clli`: two u16 values (MaxCLL, MaxFALL). Some
/// encoders write this instead of or in addition to `clli`.
fn parse_cclv(body: &[u8]) -> Result<Cclv> {
    if body.len() < 4 {
        return Err(Error::invalid(format!(
            "avif: cclv too short ({} < 4)",
            body.len()
        )));
    }
    Ok(Cclv {
        max_content_light_level: read_u16(body, 0)?,
        max_pic_average_light_level: read_u16(body, 2)?,
    })
}

/// Parse `rloc` (RelativeLocationProperty — HEIF §6.5.7). FullBox(v=0,
/// f=0) followed by two big-endian `unsigned int(32)` offsets in pixels.
fn parse_rloc(body: &[u8]) -> Result<Rloc> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: rloc version {version} != 0")));
    }
    if rest.len() < 8 {
        return Err(Error::invalid(format!(
            "avif: rloc too short ({} < 8)",
            rest.len()
        )));
    }
    Ok(Rloc {
        horizontal_offset: read_u32(rest, 0)?,
        vertical_offset: read_u32(rest, 4)?,
    })
}

/// Parse `lsel` (LayerSelectorProperty — HEIF §6.5.11). ItemProperty
/// (NO FullBox header) containing a single big-endian `unsigned int(16)`
/// `layer_id`.
fn parse_lsel(body: &[u8]) -> Result<Lsel> {
    if body.len() < 2 {
        return Err(Error::invalid(format!(
            "avif: lsel too short ({} < 2)",
            body.len()
        )));
    }
    Ok(Lsel {
        layer_id: read_u16(body, 0)?,
    })
}

/// Parse `a1op` (OperatingPointSelectorProperty — av1-avif §2.3.2.1).
/// ItemProperty (NO FullBox header) carrying a single
/// `unsigned int(8) op_index`.
fn parse_a1op(body: &[u8]) -> Result<A1op> {
    if body.is_empty() {
        return Err(Error::invalid("avif: a1op too short (0 < 1)"));
    }
    Ok(A1op { op_index: body[0] })
}

/// Parse `a1lx` (AV1LayeredImageIndexingProperty — av1-avif §2.3.2.3).
/// ItemProperty (NO FullBox header):
///
/// ```text
/// unsigned int(7) reserved = 0;
/// unsigned int(1) large_size;
/// FieldLength = (large_size + 1) * 16;
/// unsigned int(FieldLength) layer_size[3];
/// ```
///
/// `large_size == 0` → three 16-bit sizes (7 bytes total);
/// `large_size == 1` → three 32-bit sizes (13 bytes total). The reserved
/// 7 bits of the first byte are ignored on read.
fn parse_a1lx(body: &[u8]) -> Result<A1lx> {
    if body.is_empty() {
        return Err(Error::invalid("avif: a1lx too short (0 < 1)"));
    }
    let large_size = (body[0] & 0x01) != 0;
    let field_bytes = if large_size { 4 } else { 2 };
    let need = 1 + field_bytes * 3;
    if body.len() < need {
        return Err(Error::invalid(format!(
            "avif: a1lx too short ({} < {need})",
            body.len()
        )));
    }
    let mut layer_size = [0u32; 3];
    for (i, slot) in layer_size.iter_mut().enumerate() {
        let at = 1 + i * field_bytes;
        *slot = if large_size {
            read_u32(body, at)?
        } else {
            u32::from(read_u16(body, at)?)
        };
    }
    Ok(A1lx {
        large_size,
        layer_size,
    })
}

/// Parse an `iref` box: FullBox header followed by a sequence of typed
/// child boxes (`SingleItemTypeReferenceBox`), each carrying `from_item_ID`,
/// `reference_count`, and `reference_count` × `to_item_ID`. v0 uses 16-bit
/// item IDs; v1 uses 32-bit. Spec: ISO/IEC 14496-12 §8.11.12.
fn parse_iref(payload: &[u8]) -> Result<Vec<IrefEntry>> {
    let (version, _flags, body) = parse_full_box(payload)?;
    if version != 0 && version != 1 {
        return Err(Error::invalid(format!("avif: iref version {version}")));
    }
    let mut out = Vec::new();
    for hdr in iter_boxes(body) {
        let hdr = hdr?;
        let child = &body[hdr.payload_start..hdr.end()];
        let mut cursor = 0usize;
        let from_id = if version == 0 {
            let v = read_u16(child, cursor)? as u32;
            cursor += 2;
            v
        } else {
            let v = read_u32(child, cursor)?;
            cursor += 4;
            v
        };
        let ref_count = read_u16(child, cursor)? as usize;
        cursor += 2;
        let mut to_ids = Vec::with_capacity(ref_count);
        for _ in 0..ref_count {
            let v = if version == 0 {
                let x = read_u16(child, cursor)? as u32;
                cursor += 2;
                x
            } else {
                let x = read_u32(child, cursor)?;
                cursor += 4;
                x
            };
            to_ids.push(v);
        }
        out.push(IrefEntry {
            reference_type: hdr.box_type,
            from_id,
            to_ids,
        });
    }
    Ok(out)
}

fn parse_ipma(payload: &[u8]) -> Result<Vec<ItemPropertyAssociation>> {
    let (version, flags, body) = parse_full_box(payload)?;
    if body.len() < 4 {
        return Err(Error::invalid("avif: ipma too short"));
    }
    let entry_count = read_u32(body, 0)?;
    let mut cursor = 4usize;
    let mut out = Vec::with_capacity(entry_count as usize);
    let index_is_large = (flags & 1) != 0;
    for _ in 0..entry_count {
        let item_id = if version < 1 {
            let v = read_u16(body, cursor)? as u32;
            cursor += 2;
            v
        } else {
            let v = read_u32(body, cursor)?;
            cursor += 4;
            v
        };
        if cursor >= body.len() {
            return Err(Error::invalid("avif: ipma truncated at assoc count"));
        }
        let n = body[cursor] as usize;
        cursor += 1;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let (index, essential) = if index_is_large {
                let w = read_u16(body, cursor)?;
                cursor += 2;
                let essential = (w & 0x8000) != 0;
                // Spec: 1-based 15-bit index. Convert to 0-based.
                let raw = (w & 0x7fff) as i32 - 1;
                if raw < 0 {
                    return Err(Error::invalid("avif: ipma index 0"));
                }
                (raw as u16, essential)
            } else {
                if cursor >= body.len() {
                    return Err(Error::invalid("avif: ipma truncated at entry"));
                }
                let w = body[cursor];
                cursor += 1;
                let essential = (w & 0x80) != 0;
                let raw = (w & 0x7f) as i32 - 1;
                if raw < 0 {
                    return Err(Error::invalid("avif: ipma index 0"));
                }
                (raw as u16, essential)
            };
            entries.push(PropertyAssociation { index, essential });
        }
        out.push(ItemPropertyAssociation { item_id, entries });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colr_nclx() {
        // bt709 / sRGB / bt709 / full-range.
        let buf = [
            b'n', b'c', b'l', b'x', 0x00, 0x01, 0x00, 0x0d, 0x00, 0x05, 0x80,
        ];
        let c = parse_colr(&buf).unwrap();
        match c {
            Colr::Nclx {
                colour_primaries,
                transfer_characteristics,
                matrix_coefficients,
                full_range,
            } => {
                assert_eq!(colour_primaries, 1);
                assert_eq!(transfer_characteristics, 13);
                assert_eq!(matrix_coefficients, 5);
                assert!(full_range);
            }
            _ => panic!("expected nclx"),
        }
    }

    #[test]
    fn ispe_round_trip() {
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&100u32.to_be_bytes());
        buf.extend_from_slice(&200u32.to_be_bytes());
        let ispe = parse_ispe(&buf).unwrap();
        assert_eq!(ispe.width, 100);
        assert_eq!(ispe.height, 200);
    }

    #[test]
    fn irot_imir_masks_reserved_bits() {
        let irot = parse_irot(&[0xff]).unwrap();
        assert_eq!(irot.angle, 3);
        let imir = parse_imir(&[0xff]).unwrap();
        assert_eq!(imir.axis, 1);
    }

    #[test]
    fn clap_reads_all_fields() {
        let mut buf = Vec::new();
        for i in 0..8u32 {
            buf.extend_from_slice(&i.to_be_bytes());
        }
        let c = parse_clap(&buf).unwrap();
        assert_eq!(c.clean_aperture_width_n, 0);
        assert_eq!(c.clean_aperture_width_d, 1);
        assert_eq!(c.vert_off_d, 7);
    }

    #[test]
    fn auxc_extracts_urn() {
        let mut buf = vec![0u8; 4]; // FullBox v=0 flags=0
        buf.extend_from_slice(b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0");
        let a = parse_auxc(&buf).unwrap();
        assert!(a
            .aux_type
            .starts_with("urn:mpeg:mpegB:cicp:systems:auxiliary:alpha"));
    }

    #[test]
    fn pixi_parses_3_channel_8bit() {
        // FullBox(v=0, f=0) + num_channels=3 + [8,8,8].
        let mut buf = vec![0u8; 4];
        buf.push(3);
        buf.extend_from_slice(&[8, 8, 8]);
        let pixi = parse_pixi(&buf).unwrap();
        assert_eq!(pixi.num_channels(), 3);
        assert_eq!(pixi.bits_per_channel, vec![8, 8, 8]);
        assert_eq!(pixi.max_bit_depth(), 8);
        assert!(pixi.is_uniform_depth());
    }

    #[test]
    fn pixi_parses_10bit_hdr() {
        let mut buf = vec![0u8; 4];
        buf.push(3);
        buf.extend_from_slice(&[10, 10, 10]);
        let pixi = parse_pixi(&buf).unwrap();
        assert_eq!(pixi.max_bit_depth(), 10);
        assert!(pixi.is_uniform_depth());
    }

    #[test]
    fn pixi_handles_mixed_depth() {
        let mut buf = vec![0u8; 4];
        buf.push(3);
        buf.extend_from_slice(&[8, 10, 12]);
        let pixi = parse_pixi(&buf).unwrap();
        assert_eq!(pixi.max_bit_depth(), 12);
        assert!(!pixi.is_uniform_depth());
    }

    #[test]
    fn pixi_rejects_truncated_channel_list() {
        // Declares 4 channels but only ships 2 bytes.
        let mut buf = vec![0u8; 4];
        buf.push(4);
        buf.extend_from_slice(&[8, 8]);
        assert!(parse_pixi(&buf).is_err());
    }

    #[test]
    fn pixi_zero_channels_parses_empty() {
        // num_channels=0 is degenerate but technically encodable. The
        // parser should not panic; downstream consumers can detect the
        // empty list.
        let mut buf = vec![0u8; 4];
        buf.push(0);
        let pixi = parse_pixi(&buf).unwrap();
        assert_eq!(pixi.num_channels(), 0);
        assert_eq!(pixi.max_bit_depth(), 0);
        assert!(!pixi.is_uniform_depth());
    }

    #[test]
    fn pasp_parses_square_and_anamorphic() {
        // 1:1 square pixels.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&1u32.to_be_bytes());
        let p = parse_pasp(&buf).unwrap();
        assert!(p.is_square());
        assert_eq!(p.ratio(), Some(1.0));
        // 16:11 anamorphic (DV NTSC widescreen).
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(&11u32.to_be_bytes());
        let p = parse_pasp(&buf).unwrap();
        assert!(!p.is_square());
        let r = p.ratio().unwrap();
        assert!((r - 16.0 / 11.0).abs() < 1e-9);
    }

    #[test]
    fn pasp_zero_v_spacing_is_safe() {
        // (4, 0) is malformed but must not divide-by-zero. ratio=None,
        // is_square=false.
        let mut buf = Vec::new();
        buf.extend_from_slice(&4u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        let p = parse_pasp(&buf).unwrap();
        assert_eq!(p.ratio(), None);
        assert!(!p.is_square());
    }

    #[test]
    fn pasp_rejects_truncated() {
        let buf = vec![0u8; 4]; // need 8
        assert!(parse_pasp(&buf).is_err());
    }

    #[test]
    fn iref_v0_round_trip() {
        // FullBox(v=0, f=0) + one `auxl` child referencing from_id=2 to {1}.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // fullbox
                                           // Child box header: size(4) + type(4)
        let child_payload: Vec<u8> = {
            let mut cp = Vec::new();
            cp.extend_from_slice(&2u16.to_be_bytes()); // from_id
            cp.extend_from_slice(&1u16.to_be_bytes()); // ref_count
            cp.extend_from_slice(&1u16.to_be_bytes()); // to_id = 1
            cp
        };
        let child_size = (8 + child_payload.len()) as u32;
        body.extend_from_slice(&child_size.to_be_bytes());
        body.extend_from_slice(b"auxl");
        body.extend_from_slice(&child_payload);
        let refs = parse_iref(&body).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(&refs[0].reference_type, b"auxl");
        assert_eq!(refs[0].from_id, 2);
        assert_eq!(refs[0].to_ids, vec![1]);
    }

    /// `mdcv` round-trip: 6 × u16 primaries + 2 × u16 white point +
    /// 2 × u32 luminance values (no FullBox header).
    #[test]
    fn mdcv_round_trip() {
        let mut buf = Vec::new();
        // Rx=13250, Ry=34500  (BT.2020 red primary × 50000)
        buf.extend_from_slice(&13250u16.to_be_bytes());
        buf.extend_from_slice(&34500u16.to_be_bytes());
        // Gx=7500, Gy=40000  (BT.2020 green primary × 50000)
        buf.extend_from_slice(&7500u16.to_be_bytes());
        buf.extend_from_slice(&40000u16.to_be_bytes());
        // Bx=3000, By=1000   (BT.2020 blue primary × 50000)
        buf.extend_from_slice(&3000u16.to_be_bytes());
        buf.extend_from_slice(&1000u16.to_be_bytes());
        // White point: D65 = (15635, 16450)
        buf.extend_from_slice(&15635u16.to_be_bytes());
        buf.extend_from_slice(&16450u16.to_be_bytes());
        // Max luminance = 10000000 (1000 cd/m² × 10000 units)
        buf.extend_from_slice(&10_000_000u32.to_be_bytes());
        // Min luminance = 50 (0.005 cd/m² × 10000 units)
        buf.extend_from_slice(&50u32.to_be_bytes());
        let m = parse_mdcv(&buf).unwrap();
        assert_eq!(m.display_primaries_xy[0], (13250, 34500)); // R
        assert_eq!(m.display_primaries_xy[1], (7500, 40000)); // G
        assert_eq!(m.display_primaries_xy[2], (3000, 1000)); // B
        assert_eq!(m.white_point_xy, (15635, 16450));
        assert_eq!(m.max_display_mastering_luminance, 10_000_000);
        assert_eq!(m.min_display_mastering_luminance, 50);
    }

    /// `mdcv` rejects truncated input (< 24 bytes).
    #[test]
    fn mdcv_rejects_truncated() {
        let buf = vec![0u8; 23];
        assert!(parse_mdcv(&buf).is_err());
    }

    /// `clli` round-trip: MaxCLL + MaxFALL as two u16 values.
    #[test]
    fn clli_round_trip() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1000u16.to_be_bytes()); // MaxCLL = 1000 cd/m²
        buf.extend_from_slice(&400u16.to_be_bytes()); // MaxFALL = 400 cd/m²
        let c = parse_clli(&buf).unwrap();
        assert_eq!(c.max_content_light_level, 1000);
        assert_eq!(c.max_pic_average_light_level, 400);
    }

    /// `clli` rejects truncated input (< 4 bytes).
    #[test]
    fn clli_rejects_truncated() {
        let buf = vec![0u8; 3];
        assert!(parse_clli(&buf).is_err());
    }

    /// `cclv` has identical layout to `clli`.
    #[test]
    fn cclv_round_trip() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&2000u16.to_be_bytes()); // MaxCLL
        buf.extend_from_slice(&800u16.to_be_bytes()); // MaxFALL
        let c = parse_cclv(&buf).unwrap();
        assert_eq!(c.max_content_light_level, 2000);
        assert_eq!(c.max_pic_average_light_level, 800);
    }

    /// `cclv` rejects truncated input.
    #[test]
    fn cclv_rejects_truncated() {
        let buf = vec![0u8; 1];
        assert!(parse_cclv(&buf).is_err());
    }

    /// Build a synthetic v2 `infe` payload with the given item_type +
    /// optional trailing fields. The wrapper FullBox header lives in the
    /// `body` argument's prefix to mirror what the iinf parser hands to
    /// `parse_infe`.
    fn build_infe_v2(item_type: &[u8; 4], name: &str, tail: &[u8]) -> Vec<u8> {
        // FullBox header: version=2, flags=0
        let mut buf = vec![2u8, 0, 0, 0];
        buf.extend_from_slice(&1u16.to_be_bytes()); // item_ID = 1
        buf.extend_from_slice(&0u16.to_be_bytes()); // protection_index = 0
        buf.extend_from_slice(item_type); // item_type
        buf.extend_from_slice(name.as_bytes());
        buf.push(0); // NUL for item_name
        buf.extend_from_slice(tail);
        buf
    }

    /// A 'mime' v2 infe carries both content_type and an optional
    /// content_encoding after item_name. The XMP item shape that
    /// libheif / libavif emit (`application/rdf+xml`) is the canonical
    /// case the AVIF metadata path needs to recognise.
    #[test]
    fn infe_v2_mime_parses_content_type_and_encoding() {
        let mut tail = Vec::new();
        tail.extend_from_slice(b"application/rdf+xml\0");
        tail.extend_from_slice(b"\0"); // explicit empty content_encoding
        let payload = build_infe_v2(b"mime", "xmp", &tail);
        let info = parse_infe(&payload).unwrap();
        assert_eq!(info.id, 1);
        assert_eq!(&info.item_type, b"mime");
        assert_eq!(info.name, "xmp");
        assert_eq!(info.content_type.as_deref(), Some("application/rdf+xml"));
        // Spec §8.11.6.3 — empty string content_encoding means "no
        // encoding"; we collapse it to None so callers don't need to
        // special-case the empty string.
        assert!(info.content_encoding.is_none());
        assert!(info.item_uri_type.is_none());
    }

    /// 'mime' v3 infe shape (32-bit item_ID), Exif TIFF blob wrapped
    /// with content_type=application/octet-stream — the libheif Exif
    /// writer pattern.
    #[test]
    fn infe_v3_mime_octet_stream_for_exif() {
        // FullBox header: version=3, flags=0
        let mut buf = vec![3u8, 0, 0, 0];
        buf.extend_from_slice(&42u32.to_be_bytes()); // item_ID = 42
        buf.extend_from_slice(&0u16.to_be_bytes()); // protection
        buf.extend_from_slice(b"mime"); // item_type
        buf.extend_from_slice(b"\0"); // empty item_name
        buf.extend_from_slice(b"application/octet-stream\0");
        // content_encoding absent (box ends after content_type cstr)
        let info = parse_infe(&buf).unwrap();
        assert_eq!(info.id, 42);
        assert_eq!(&info.item_type, b"mime");
        assert_eq!(
            info.content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert!(info.content_encoding.is_none());
    }

    /// 'uri ' item_type carries an item_uri_type cstr instead of
    /// content_type/content_encoding.
    #[test]
    fn infe_v2_uri_parses_uri_type() {
        let mut tail = Vec::new();
        tail.extend_from_slice(b"https://example.invalid/spec\0");
        let payload = build_infe_v2(b"uri ", "uri-meta", &tail);
        let info = parse_infe(&payload).unwrap();
        assert_eq!(&info.item_type, b"uri ");
        assert!(info.content_type.is_none());
        assert_eq!(
            info.item_uri_type.as_deref(),
            Some("https://example.invalid/spec")
        );
    }

    /// Generic item types (`av01`, `Exif`, `grid`, …) stop after
    /// `item_name`; no additional fields are parsed.
    #[test]
    fn infe_v2_generic_item_type_stops_at_name() {
        let payload = build_infe_v2(b"av01", "color", &[]);
        let info = parse_infe(&payload).unwrap();
        assert_eq!(&info.item_type, b"av01");
        assert!(info.content_type.is_none());
        assert!(info.content_encoding.is_none());
        assert!(info.item_uri_type.is_none());
        let payload = build_infe_v2(b"Exif", "exif-blob", &[]);
        let info = parse_infe(&payload).unwrap();
        assert_eq!(&info.item_type, b"Exif");
        assert!(info.content_type.is_none());
    }

    /// `iref_sources_of` returns every source whose `to_ids` contains
    /// the target id. Two `thmb` irefs both pointing at the primary
    /// (small + tiny thumbnails of one master) should both surface.
    #[test]
    fn iref_sources_of_returns_all_matches() {
        let m = Meta {
            irefs: vec![
                IrefEntry {
                    reference_type: *b"thmb",
                    from_id: 10,
                    to_ids: vec![1],
                },
                IrefEntry {
                    reference_type: *b"thmb",
                    from_id: 11,
                    to_ids: vec![1],
                },
                // Irrelevant: different reference_type
                IrefEntry {
                    reference_type: *b"auxl",
                    from_id: 12,
                    to_ids: vec![1],
                },
                // Irrelevant: different target
                IrefEntry {
                    reference_type: *b"thmb",
                    from_id: 13,
                    to_ids: vec![2],
                },
            ],
            ..Meta::default()
        };
        let mut got = m.iref_sources_of(b"thmb", 1);
        got.sort_unstable();
        assert_eq!(got, vec![10, 11]);
        assert!(m.iref_sources_of(b"thmb", 99).is_empty());
    }

    /// `is_alpha_premultiplied_for` detects HEIF `prem` iref linking an
    /// alpha auxiliary to a colour image.
    #[test]
    fn is_alpha_premultiplied_for_detects_prem_iref() {
        let m = Meta {
            irefs: vec![IrefEntry {
                reference_type: *b"prem",
                from_id: 2,
                to_ids: vec![1],
            }],
            ..Meta::default()
        };
        assert!(m.is_alpha_premultiplied_for(1));
        assert!(!m.is_alpha_premultiplied_for(2));
        // Negative case: empty meta
        let empty = Meta::default();
        assert!(!empty.is_alpha_premultiplied_for(1));
    }

    /// `AuxC::kind` recognises the canonical alpha URNs (both MPEG and
    /// HEVC HEIF spellings map to `AuxKind::Alpha`).
    #[test]
    fn auxc_kind_classifies_alpha() {
        let mpeg = AuxC {
            aux_type: AUX_URN_ALPHA_MPEG.to_string(),
            aux_subtype: Vec::new(),
        };
        assert_eq!(mpeg.kind(), AuxKind::Alpha);
        assert!(mpeg.is_alpha());
        assert!(!mpeg.is_depth_map());

        let hevc = AuxC {
            aux_type: AUX_URN_ALPHA_HEVC.to_string(),
            aux_subtype: Vec::new(),
        };
        assert_eq!(hevc.kind(), AuxKind::Alpha);
        assert!(hevc.is_alpha());
    }

    /// `AuxC::kind` recognises both depth-map URN spellings.
    #[test]
    fn auxc_kind_classifies_depth_map() {
        let mpeg = AuxC {
            aux_type: AUX_URN_DEPTH_MPEG.to_string(),
            aux_subtype: Vec::new(),
        };
        assert_eq!(mpeg.kind(), AuxKind::DepthMap);
        assert!(mpeg.is_depth_map());
        assert!(!mpeg.is_alpha());

        let hevc = AuxC {
            aux_type: AUX_URN_DEPTH_HEVC.to_string(),
            aux_subtype: Vec::new(),
        };
        assert_eq!(hevc.kind(), AuxKind::DepthMap);
    }

    /// `AuxC::kind` recognises Apple's HDR gain-map URN.
    #[test]
    fn auxc_kind_classifies_hdr_gain_map() {
        let g = AuxC {
            aux_type: AUX_URN_HDR_GAINMAP.to_string(),
            aux_subtype: Vec::new(),
        };
        assert_eq!(g.kind(), AuxKind::HdrGainMap);
        assert!(g.is_hdr_gain_map());
        assert!(!g.is_alpha());
    }

    /// Unknown auxC URNs report `AuxKind::Other` rather than
    /// misclassifying. A prefix-only match doesn't slip through —
    /// classification is exact.
    #[test]
    fn auxc_kind_other_for_unknown_urn() {
        let custom = AuxC {
            aux_type: "urn:example:foo:bar".to_string(),
            aux_subtype: Vec::new(),
        };
        assert_eq!(custom.kind(), AuxKind::Other);
        assert!(!custom.is_alpha());

        // Prefix-only match must not be classified as Alpha. A writer
        // that decorates the canonical URN with a trailing identifier
        // (e.g. for sub-typing) gets `Other` and the raw URN is still
        // visible on aux_type.
        let prefix = AuxC {
            aux_type: format!("{AUX_URN_ALPHA_MPEG}:extended"),
            aux_subtype: Vec::new(),
        };
        assert_eq!(prefix.kind(), AuxKind::Other);
    }

    /// `rloc` round-trip: FullBox v=0 + two big-endian u32 offsets.
    #[test]
    fn rloc_round_trip() {
        let mut buf = vec![0u8; 4]; // FullBox v=0 f=0
        buf.extend_from_slice(&96u32.to_be_bytes()); // horizontal_offset
        buf.extend_from_slice(&128u32.to_be_bytes()); // vertical_offset
        let r = parse_rloc(&buf).unwrap();
        assert_eq!(r.horizontal_offset, 96);
        assert_eq!(r.vertical_offset, 128);
    }

    /// `rloc` rejects unrecognised versions.
    #[test]
    fn rloc_rejects_nonzero_version() {
        let mut buf = vec![1u8, 0, 0, 0]; // FullBox v=1
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        assert!(parse_rloc(&buf).is_err());
    }

    /// `rloc` rejects truncated payload.
    #[test]
    fn rloc_rejects_truncated() {
        let buf = vec![0u8; 4 + 4]; // missing vertical_offset
        assert!(parse_rloc(&buf).is_err());
    }

    /// `lsel` round-trip: ItemProperty (NO FullBox) carrying a single
    /// u16 layer_id.
    #[test]
    fn lsel_round_trip() {
        let buf = 3u16.to_be_bytes();
        let l = parse_lsel(&buf).unwrap();
        assert_eq!(l.layer_id, 3);
    }

    /// `lsel` rejects truncated payload.
    #[test]
    fn lsel_rejects_truncated() {
        let buf = vec![0u8; 1];
        assert!(parse_lsel(&buf).is_err());
    }

    /// `rloc` plus `lsel` parse through the property-store dispatch so
    /// associations land on items end-to-end. Build a minimal ipco
    /// containing both boxes.
    #[test]
    fn ipco_dispatches_rloc_and_lsel() {
        // rloc body: FullBox(v=0,f=0) + horiz=10 + vert=20
        let rloc_body = {
            let mut b = vec![0u8; 4];
            b.extend_from_slice(&10u32.to_be_bytes());
            b.extend_from_slice(&20u32.to_be_bytes());
            b
        };
        // lsel body: ItemProperty + layer_id=5
        let lsel_body = 5u16.to_be_bytes().to_vec();
        // Build an ipco containing both child boxes.
        let mut ipco = Vec::new();
        let rloc_size = 8 + rloc_body.len() as u32;
        ipco.extend_from_slice(&rloc_size.to_be_bytes());
        ipco.extend_from_slice(b"rloc");
        ipco.extend_from_slice(&rloc_body);
        let lsel_size = 8 + lsel_body.len() as u32;
        ipco.extend_from_slice(&lsel_size.to_be_bytes());
        ipco.extend_from_slice(b"lsel");
        ipco.extend_from_slice(&lsel_body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 2);
        match &props[0] {
            Property::Rloc(r) => {
                assert_eq!(r.horizontal_offset, 10);
                assert_eq!(r.vertical_offset, 20);
            }
            other => panic!("expected Rloc, got {other:?}"),
        }
        match &props[1] {
            Property::Lsel(l) => assert_eq!(l.layer_id, 5),
            other => panic!("expected Lsel, got {other:?}"),
        }
    }

    /// `a1op` is a single u8 op_index in a bare ItemProperty.
    #[test]
    fn a1op_reads_op_index() {
        let a = parse_a1op(&[7]).unwrap();
        assert_eq!(a.op_index, 7);
        // Empty body is malformed.
        assert!(parse_a1op(&[]).is_err());
    }

    /// `a1lx` with large_size = 0 → three 16-bit layer sizes.
    #[test]
    fn a1lx_16bit_field_width() {
        // byte0: reserved(7)=0, large_size(1)=0
        let mut buf = vec![0x00u8];
        buf.extend_from_slice(&100u16.to_be_bytes());
        buf.extend_from_slice(&200u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        let a = parse_a1lx(&buf).unwrap();
        assert!(!a.large_size);
        assert_eq!(a.layer_size, [100, 200, 0]);
        // Two leading non-zero sizes → image has three layers.
        assert_eq!(a.documented_layers(), 2);
    }

    /// `a1lx` with large_size = 1 → three 32-bit layer sizes. The
    /// reserved upper 7 bits of byte 0 must be ignored on read.
    #[test]
    fn a1lx_32bit_field_width_ignores_reserved() {
        // byte0: reserved bits all 1, large_size(1)=1 → 0xFF
        let mut buf = vec![0xFFu8];
        buf.extend_from_slice(&70_000u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        let a = parse_a1lx(&buf).unwrap();
        assert!(a.large_size);
        assert_eq!(a.layer_size, [70_000, 0, 0]);
        assert_eq!(a.documented_layers(), 1);
        // Truncated 32-bit body is rejected.
        let short = vec![0x01u8, 0, 0, 0, 0];
        assert!(parse_a1lx(&short).is_err());
    }

    /// `ipco` dispatch routes the two AV1-specific properties to their
    /// typed variants rather than `Property::Other`.
    #[test]
    fn ipco_dispatches_a1op_and_a1lx() {
        let a1op_body = vec![3u8]; // op_index = 3
        let a1lx_body = {
            let mut b = vec![0x00u8]; // large_size = 0
            b.extend_from_slice(&5u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b
        };
        let mut ipco = Vec::new();
        let s = 8 + a1op_body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"a1op");
        ipco.extend_from_slice(&a1op_body);
        let s = 8 + a1lx_body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"a1lx");
        ipco.extend_from_slice(&a1lx_body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 2);
        match &props[0] {
            Property::A1op(a) => assert_eq!(a.op_index, 3),
            other => panic!("expected A1op, got {other:?}"),
        }
        match &props[1] {
            Property::A1lx(a) => {
                assert!(!a.large_size);
                assert_eq!(a.layer_size, [5, 0, 0]);
            }
            other => panic!("expected A1lx, got {other:?}"),
        }
    }

    /// Essential-property enforcement (av1-avif §2.3.2.1.2 + MIAF §7.3.5):
    /// an item flagged with an essential property the crate cannot parse
    /// (lands in `Property::Other`) is reported as unprocessable; a
    /// recognised property — even one we only ignore — is not.
    #[test]
    fn unsupported_essential_property_detected() {
        let m = Meta {
            properties: vec![
                // index 0: a known property (irot) — recognised.
                Property::Irot(Irot { angle: 1 }),
                // index 1: an unknown property carried as Other.
                Property::Other(*b"zzzz", vec![0, 1, 2]),
            ],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![
                    // irot marked essential — recognised, so OK.
                    PropertyAssociation {
                        index: 0,
                        essential: true,
                    },
                    // unknown 'zzzz' marked essential — must block.
                    PropertyAssociation {
                        index: 1,
                        essential: true,
                    },
                ],
            }],
            ..Meta::default()
        };
        assert!(m.has_unsupported_essential_property(1));
        assert_eq!(m.unsupported_essential_properties(1), vec![*b"zzzz"]);
        // An item with no associations is trivially processable.
        assert!(!m.has_unsupported_essential_property(99));
    }

    /// An unknown property that is *not* marked essential may be safely
    /// ignored — the item stays processable (ISO/IEC 14496-12 §8.11.14:
    /// non-essential unrecognised properties are skipped).
    #[test]
    fn non_essential_unknown_property_does_not_block() {
        let m = Meta {
            properties: vec![Property::Other(*b"zzzz", vec![0])],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// A property association whose index points past the container is
    /// malformed; if it is flagged essential the item must be rejected
    /// (we cannot prove the essential property is supported).
    #[test]
    fn essential_property_with_dangling_index_blocks() {
        let m = Meta {
            properties: vec![],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 5,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(m.has_unsupported_essential_property(1));
    }
}
