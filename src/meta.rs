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
    read_u64, read_var_uint, type_str, BoxType,
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
/// AVIF §6.5.36 / ISO/IEC 14496-12 — Ambient Viewing Environment.
const AMVE: BoxType = b(b"amve");
/// HEIF §6.5.7 — Relative Location property.
const RLOC: BoxType = b(b"rloc");
/// HEIF §6.5.11 — Layer Selector property.
const LSEL: BoxType = b(b"lsel");
/// av1-avif §2.3.2.1 — Operating Point Selector property.
const A1OP: BoxType = b(b"a1op");
/// av1-avif §2.3.2.3 — AV1 Layered Image Indexing property.
const A1LX: BoxType = b(b"a1lx");
/// HEIF §6.5.13 — Image Scaling transformative property.
const ISCL: BoxType = b(b"iscl");
/// HEIF §6.5.17 — Required Reference Types descriptive property.
const RREF: BoxType = b(b"rref");
/// HEIF §6.5.18 — Creation Time Information descriptive property.
const CRTT: BoxType = b(b"crtt");
/// HEIF §6.5.19 — Modification Time Information descriptive property.
const MDFT: BoxType = b(b"mdft");
/// HEIF §6.5.20 — User Description descriptive property.
const UDES: BoxType = b(b"udes");
/// HEIF §6.5.21 — Accessibility Text descriptive property.
const ALTT: BoxType = b(b"altt");
/// HEIF §6.5.22 — Auto Exposure Information descriptive property.
const AEBR: BoxType = b(b"aebr");
/// HEIF §6.5.23 — White Balance Information descriptive property.
const WBBR: BoxType = b(b"wbbr");
/// HEIF §6.5.24 — Focus Information descriptive property.
const FOBR: BoxType = b(b"fobr");
/// HEIF §6.5.25 — Flash Exposure Information descriptive property.
const AFBR: BoxType = b(b"afbr");
/// HEIF §6.5.26 — Depth of Field Information descriptive property.
const DOBR: BoxType = b(b"dobr");
/// HEIF §6.5.27 — Panorama Information descriptive property.
const PANO: BoxType = b(b"pano");
/// HEIF §6.5.28 — Sub-Sample Information descriptive property.
const SUBS: BoxType = b(b"subs");
/// HEIF §6.5.29 — Target Output Layer Set descriptive property.
const TOLS: BoxType = b(b"tols");
/// HEIF §6.5.30 — Wipe Transition Effect transformative property.
const WIPE: BoxType = b(b"wipe");
/// HEIF §6.5.31 — Zoom Transition Effect transformative property.
const ZOOM: BoxType = b(b"zoom");
/// HEIF §6.5.32 — Fade Transition Effect transformative property.
const FADE: BoxType = b(b"fade");
/// HEIF §6.5.33 — Split Transition Effect transformative property.
const SPLT: BoxType = b(b"splt");
/// HEIF §6.5.34 — Suggested Transition Period descriptive property.
const STPE: BoxType = b(b"stpe");
/// HEIF §6.5.35 — Suggested Time Display Duration descriptive property.
const SSLD: BoxType = b(b"ssld");
/// HEIF §6.5.37 — Progressive Derived Image Item Information descriptive property.
const PRDI: BoxType = b(b"prdi");
/// HEIF §6.5.38 — Single Stream descriptive property.
const SSTR: BoxType = b(b"sstr");
/// HEIF §6.5.39 — Camera Extrinsic Matrix descriptive property.
const CMEX: BoxType = b(b"cmex");
/// HEIF §6.5.40 — Camera Intrinsic Matrix descriptive property.
const CMIN: BoxType = b(b"cmin");
/// HEIF §6.10.2.1 — Text Layout descriptive property.
const TXLO: BoxType = b(b"txlo");
/// HEIF §6.10.2.2 — Extended Language descriptive property.
const ELNG: BoxType = b(b"elng");
/// HEIF §6.10.4.1 — Font Characteristics descriptive property.
const FNCH: BoxType = b(b"fnch");

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
    /// The 24-bit `flags` field from the enclosing `infe` `FullBox`
    /// header. Spec: ISO/IEC 14496-12 §8.11.6.1 — bit 0 (`(flags & 1)
    /// == 1`) signals that the item is a [hidden image item][HEIF
    /// §6.4.2]: it shall not be displayed (e.g. as part of a derived
    /// image only). Higher-numbered flag bits are reserved. We retain
    /// the whole 24-bit value so callers can branch on future
    /// extensions without re-parsing.
    pub flags: u32,
}

impl ItemInfo {
    /// True when bit 0 of the `infe` `flags` is set — the HEIF
    /// hidden-image-item signal (ISO/IEC 23008-12 §6.4.2 + ISO/IEC
    /// 14496-12 §8.11.6.1). Hidden items shall not be presented
    /// directly by a reader (typical use: a base image item that only
    /// exists as an input to a `tmap` / `iden` / `iovl` / `sato`
    /// derivation, or an `auxl` auxiliary like the alpha plane).
    pub fn is_hidden(&self) -> bool {
        (self.flags & 0x01) == 0x01
    }
}

/// One `iloc` extent (offset + length pair inside the referenced data
/// blob). AVIF primary items are usually single-extent.
///
/// For `construction_method == 2` (item_offset) entries, `extent_index`
/// carries the 1-based index of the `'iloc'` item reference (declared in
/// the `iref` box, source = this item) that names the item supplying this
/// extent's data origin (ISO/IEC 14496-12 §8.11.3.3). For methods 0 / 1
/// the field is unused and left at its default of `0`; the parser also
/// records `0` when the box's `index_size` is `0`, in which case the spec
/// implies the value `1` for cm=2 resolution.
#[derive(Clone, Debug, Default)]
pub struct IlocExtent {
    pub offset: u64,
    pub length: u64,
    /// 1-based `'iloc'` iref index, used only by `construction_method == 2`.
    /// `0` means "not present in the box" (cm=2 then implies `1`).
    pub extent_index: u64,
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

/// Ambient Viewing Environment (`amve`) — the nominal viewing-environment
/// HDR metadata, carried as an `ipco` descriptive item property and
/// associated with an image item via `ipma`. AVIF §6.5.36 references it
/// as box type `amve` (no version/flags — a plain `Box`, NOT a `FullBox`)
/// with the field definitions attributed to ISO/IEC 14496-12 (post-2015
/// Ambient Viewing Environment Box).
///
/// The three fields are the same syntax elements, with the same units and
/// ranges, as the `ambient_viewing_environment` SEI message and the
/// ISO/IEC 23091-3 ambient-viewing-environment parameters — a decoder can
/// populate the bitstream AVE SEI from this box field-for-field, with no
/// scaling conversion required.
///
/// The 8-byte body layout is:
///
/// ```text
/// unsigned int(32) ambient_illuminance;  // units of 0.0001 lux
/// unsigned int(16) ambient_light_x;      // CIE 1931 x, units of 0.00002
/// unsigned int(16) ambient_light_y;      // CIE 1931 y, units of 0.00002
/// ```
///
/// Unlike `mdcv`/`clli`, which describe the *content's* mastering
/// environment, `amve` describes the *viewer's* nominal ambient
/// environment, so the two are complementary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Amve {
    /// Environmental illuminance of the ambient viewing environment, in
    /// units of 0.0001 lux (`lux = value / 10000`). `0` is permitted by
    /// the syntax but generally treated as "unknown" by consumers.
    pub ambient_illuminance: u32,
    /// Normalised CIE 1931 *x* chromaticity of the ambient light, in
    /// units of 0.00002 (`x = value * 0.00002`); range 0..=50000.
    pub ambient_light_x: u16,
    /// Normalised CIE 1931 *y* chromaticity of the ambient light, in
    /// units of 0.00002 (`y = value * 0.00002`); range 0..=50000.
    pub ambient_light_y: u16,
}

impl Amve {
    /// `ambient_illuminance` decoded into lux (`value / 10000.0`).
    #[inline]
    pub fn illuminance_lux(&self) -> f64 {
        f64::from(self.ambient_illuminance) / 10000.0
    }

    /// CIE 1931 (x, y) chromaticity of the ambient light, decoded to
    /// the normalised 0.0..=1.0 range (`value * 0.00002`).
    #[inline]
    pub fn light_xy(&self) -> (f64, f64) {
        (
            f64::from(self.ambient_light_x) * 0.00002,
            f64::from(self.ambient_light_y) * 0.00002,
        )
    }
}

/// Image Scaling transformative property (`iscl`) — HEIF §6.5.13.
///
/// Scales an input image by independent horizontal and vertical
/// rational factors. The input image is the output of the previous
/// transformative item property, if any, or the reconstructed image
/// of the associated image item. The target dimensions are computed
/// as `ceil((input_width * target_width_numerator) /
/// target_width_denominator)` and the parallel formula for height
/// (the spec is explicit that the division is floating-point and
/// the result is then ceiled — see HEIF §6.5.13.1 NOTE 1).
///
/// Spec: ISO/IEC 23008-12 §6.5.13.2 — FullBox(`iscl`, version=0,
/// flags=0) carrying four big-endian `unsigned int(16)` fields:
///
/// ```text
/// unsigned int(16) target_width_numerator;
/// unsigned int(16) target_width_denominator;
/// unsigned int(16) target_height_numerator;
/// unsigned int(16) target_height_denominator;
/// ```
///
/// Per §6.5.13.3 every numerator and denominator `shall` be non-zero;
/// the parser surfaces the raw values and a separate
/// [`Iscl::is_well_formed`] helper exposes the §6.5.13.3 check
/// without conflating "syntactically parseable" with "semantically
/// valid" — both are useful signals at distinct layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Iscl {
    /// Numerator of the horizontal scaling ratio.
    pub target_width_numerator: u16,
    /// Denominator of the horizontal scaling ratio.
    pub target_width_denominator: u16,
    /// Numerator of the vertical scaling ratio.
    pub target_height_numerator: u16,
    /// Denominator of the vertical scaling ratio.
    pub target_height_denominator: u16,
}

impl Iscl {
    /// True when every numerator and denominator is non-zero (the
    /// §6.5.13.3 `shall`). A property that parses with a zero
    /// field is malformed per spec; callers can route to a strict
    /// rejection path.
    pub fn is_well_formed(&self) -> bool {
        self.target_width_numerator != 0
            && self.target_width_denominator != 0
            && self.target_height_numerator != 0
            && self.target_height_denominator != 0
    }

    /// Compute the scaled output dimensions for an input of
    /// `(input_width, input_height)` per the HEIF §6.5.13.1
    /// formula: `ceil((input * numerator) / denominator)`. Returns
    /// `None` when either denominator is zero (avoids dividing by
    /// zero); callers wanting strict §6.5.13.3 enforcement should
    /// gate on [`Iscl::is_well_formed`] first.
    ///
    /// The arithmetic is carried out in `u64` so the intermediate
    /// `input * numerator` cannot overflow for any pair of `u32`
    /// input dimension and `u16` numerator.
    pub fn scaled_dims(&self, input_width: u32, input_height: u32) -> Option<(u32, u32)> {
        if self.target_width_denominator == 0 || self.target_height_denominator == 0 {
            return None;
        }
        let w = div_ceil_u64(
            u64::from(input_width) * u64::from(self.target_width_numerator),
            u64::from(self.target_width_denominator),
        );
        let h = div_ceil_u64(
            u64::from(input_height) * u64::from(self.target_height_numerator),
            u64::from(self.target_height_denominator),
        );
        // The scaled dimensions can legally exceed `u32::MAX` only if
        // the writer picked extreme numerators; saturate so the helper
        // never panics.
        Some((
            u32::try_from(w).unwrap_or(u32::MAX),
            u32::try_from(h).unwrap_or(u32::MAX),
        ))
    }
}

#[inline]
fn div_ceil_u64(n: u64, d: u64) -> u64 {
    // d != 0 enforced at the only call site; defensively guard anyway.
    if d == 0 {
        return 0;
    }
    n / d + u64::from(n % d != 0)
}

/// Required Reference Types descriptive property (`rref`) —
/// HEIF §6.5.17.
///
/// Lists the item reference (`iref`) types a reader `shall`
/// understand and process to decode the associated image item. Per
/// §6.5.17.1 the property is mandatory on a predictively coded
/// image item and forbidden as an essential-bit "downgrade" — the
/// associated `essential` flag in the `ipma` association `shall`
/// equal `1`, so a reader that doesn't recognise every listed
/// `iref` type must refuse to process the item.
///
/// Spec: ISO/IEC 23008-12 §6.5.17.2 — FullBox(`rref`, version=0,
/// flags=0):
///
/// ```text
/// unsigned int(8) reference_type_count;
/// for (i=0; i< reference_type_count; i++) {
///     unsigned int(32) reference_type[i];
/// }
/// ```
///
/// Each `reference_type[i]` is a four-CC carried as a big-endian
/// `u32`; the four ASCII bytes (high → low byte order) form the
/// `BoxType` of the required iref category.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rref {
    /// The list of required `iref` four-CCs in declaration order.
    /// The §6.5.17.2 `reference_type_count` is captured implicitly
    /// as `reference_types.len()`.
    pub reference_types: Vec<BoxType>,
}

impl Rref {
    /// Number of declared required reference types — equivalent to
    /// `reference_types.len()`.
    pub fn count(&self) -> usize {
        self.reference_types.len()
    }

    /// True when `four_cc` appears in the declared list.
    pub fn requires(&self, four_cc: &BoxType) -> bool {
        self.reference_types.iter().any(|t| t == four_cc)
    }
}

/// Creation Time Information descriptive property (`crtt`) —
/// HEIF §6.5.18.
///
/// Documents the creation time of the associated item or entity group.
/// The semantic field is a single `unsigned int(64)` `creation_time`
/// counted in **microseconds since midnight, Jan. 1, 1904, in UTC time**
/// (§6.5.18.3). The 1904 epoch matches the legacy QuickTime / ISOBMFF
/// movie-header epoch (ISO/IEC 14496-12 §8.2.2), but the unit here is
/// microseconds rather than the seconds used by `mvhd` / `tkhd` /
/// `mdhd` — readers that compare or convert against ISOBMFF track
/// timestamps must scale by 10^6 in the appropriate direction.
///
/// Per §6.5.18.1 the property is a descriptive item property with
/// `Quantity (per associated item_ID): At most one`, and is not
/// mandatory; absent property means the creation time is unspecified.
///
/// Spec: ISO/IEC 23008-12 §6.5.18.2 — FullBox(`crtt`, version=0,
/// flags=0):
///
/// ```text
/// unsigned int(64) creation_time;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crtt {
    /// Creation time in microseconds since midnight, Jan. 1, 1904 UTC.
    pub creation_time: u64,
}

/// Number of whole seconds between the 1904-01-01 UTC epoch used by
/// HEIF §6.5.18 / ISOBMFF §8.2.2 and the 1970-01-01 UTC Unix epoch.
///
/// `66` calendar years (1904..1970) of which `17` are leap years
/// (1904, 1908, 1912, 1916, 1920, 1924, 1928, 1932, 1936, 1940, 1944,
/// 1948, 1952, 1956, 1960, 1964, 1968 — 1900 is excluded by the
/// Gregorian century rule), giving
/// `66 * 365 + 17 = 24107` days × `86_400` s/day = `2_082_844_800` s.
const HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS: u64 = 2_082_844_800;

impl Crtt {
    /// Convert the §6.5.18.3 `creation_time` (microseconds since
    /// 1904-01-01 UTC) to whole seconds since the Unix epoch
    /// (1970-01-01 UTC), discarding the sub-second remainder.
    ///
    /// Returns `None` when the value predates the Unix epoch
    /// (i.e. less than [`HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS`] seconds
    /// after 1904-01-01) — `creation_time` is unsigned so a pre-1970
    /// HEIF timestamp would underflow on subtraction. Callers wanting
    /// the raw 1904-epoch value can read [`Self::creation_time`]
    /// directly.
    pub fn seconds_since_unix_epoch(&self) -> Option<u64> {
        let secs_since_1904 = self.creation_time / 1_000_000;
        secs_since_1904.checked_sub(HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS)
    }

    /// Sub-second component of `creation_time` in microseconds
    /// (`0..1_000_000`). Pairs with [`Self::seconds_since_unix_epoch`]
    /// when a caller needs full-resolution time reconstruction.
    pub fn subsecond_micros(&self) -> u32 {
        // `% 1_000_000` always fits in a `u32`.
        (self.creation_time % 1_000_000) as u32
    }
}

/// Modification Time Information descriptive property (`mdft`) —
/// HEIF §6.5.19.
///
/// Documents the most recent modification time of the associated item
/// or entity group. The semantic field is a single `unsigned int(64)`
/// `modification_time` counted in **microseconds since midnight,
/// Jan. 1, 1904, in UTC time** (§6.5.19.3) — the same epoch and unit
/// as the §6.5.18 [`Crtt`] creation-time field.
///
/// Per §6.5.19.1 the property is a descriptive item property with
/// `Quantity (per associated item_ID): At most one`, and is not
/// mandatory; an absent property means the modification time is
/// unspecified. A reader that sees both `mdft` and `crtt` on the same
/// item gets a creation/modification time pair; the spec does not
/// require `modification_time >= creation_time`, but a well-formed
/// writer would honour that ordering.
///
/// Spec: ISO/IEC 23008-12 §6.5.19.2 — FullBox(`mdft`, version=0,
/// flags=0):
///
/// ```text
/// unsigned int(64) modification_time;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mdft {
    /// Modification time in microseconds since midnight, Jan. 1, 1904 UTC.
    pub modification_time: u64,
}

impl Mdft {
    /// Convert the §6.5.19.3 `modification_time` (microseconds since
    /// 1904-01-01 UTC) to whole seconds since the Unix epoch
    /// (1970-01-01 UTC), discarding the sub-second remainder.
    ///
    /// Returns `None` when the value predates the Unix epoch
    /// (i.e. less than [`HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS`] seconds
    /// after 1904-01-01) — `modification_time` is unsigned so a
    /// pre-1970 HEIF timestamp would underflow on subtraction.
    /// Callers wanting the raw 1904-epoch value can read
    /// [`Self::modification_time`] directly.
    pub fn seconds_since_unix_epoch(&self) -> Option<u64> {
        let secs_since_1904 = self.modification_time / 1_000_000;
        secs_since_1904.checked_sub(HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS)
    }

    /// Sub-second component of `modification_time` in microseconds
    /// (`0..1_000_000`). Pairs with [`Self::seconds_since_unix_epoch`]
    /// when a caller needs full-resolution time reconstruction.
    pub fn subsecond_micros(&self) -> u32 {
        // `% 1_000_000` always fits in a `u32`.
        (self.modification_time % 1_000_000) as u32
    }
}

/// User Description descriptive property (`udes`) — HEIF §6.5.20.
///
/// Pairs the associated item or entity group with a human-readable
/// name, description, and a comma-separated tag list, all carried in a
/// single language. Per §6.5.20.1 the property is a descriptive item
/// property with `Quantity (per associated item_ID): Zero or more`,
/// and multiple instances associated to the same item shall carry
/// **different** language codes — they represent the same content in
/// different languages, from which a reader picks the most
/// appropriate.
///
/// The wire layout is four sequential null-terminated UTF-8 strings:
///
/// ```text
/// utf8string lang;
/// utf8string name;
/// utf8string description;
/// utf8string tags;
/// ```
///
/// Per §6.5.20.3:
///
/// * `lang` is an RFC 5646 language tag (e.g. `"en-US"`, `"fr-FR"`,
///   `"zh-CN"`); an empty string means the language is
///   unknown/undefined.
/// * `name` is a human-readable name for the associated item; an
///   empty string means no name is provided.
/// * `description` is a human-readable description; an empty string
///   means no description is provided.
/// * `tags` is a comma-separated user-defined tag list; an empty
///   string means no tags are provided.
///
/// The parser preserves every string verbatim, including the
/// `"absent"` empty-string sentinel — callers needing a strongly
/// optional shape can convert with the [`Self::name_opt`],
/// [`Self::description_opt`], [`Self::tags_opt`], and
/// [`Self::lang_opt`] helpers which return `None` for the empty
/// case.
///
/// Spec: ISO/IEC 23008-12 §6.5.20.2 — FullBox(`udes`, version=0,
/// flags=0).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Udes {
    /// RFC 5646 language tag (e.g. `"en-US"`); empty = unknown.
    pub lang: String,
    /// Human-readable name for the item or entity group; empty = absent.
    pub name: String,
    /// Human-readable description for the item or entity group;
    /// empty = absent.
    pub description: String,
    /// Comma-separated user-defined tags; empty = absent.
    pub tags: String,
}

impl Udes {
    /// `lang` typed as `Option<&str>`: `None` when the field is empty
    /// (§6.5.20.3 "unknown/undefined").
    pub fn lang_opt(&self) -> Option<&str> {
        if self.lang.is_empty() {
            None
        } else {
            Some(self.lang.as_str())
        }
    }

    /// `name` typed as `Option<&str>`: `None` when the field is empty
    /// (§6.5.20.3 "no name is provided").
    pub fn name_opt(&self) -> Option<&str> {
        if self.name.is_empty() {
            None
        } else {
            Some(self.name.as_str())
        }
    }

    /// `description` typed as `Option<&str>`: `None` when the field is
    /// empty (§6.5.20.3 "no description is provided").
    pub fn description_opt(&self) -> Option<&str> {
        if self.description.is_empty() {
            None
        } else {
            Some(self.description.as_str())
        }
    }

    /// `tags` split on `','` and trimmed, after the §6.5.20.3
    /// "comma-separated" shape. Returns an empty vector when the
    /// `tags` field is absent (empty string); blank-only segments
    /// (`",,foo,"`) are filtered out so a caller iterating the
    /// result gets a clean tag list.
    pub fn tag_list(&self) -> Vec<&str> {
        if self.tags.is_empty() {
            return Vec::new();
        }
        self.tags
            .split(',')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect()
    }

    /// `tags` typed as `Option<&str>`: `None` when the field is empty
    /// (§6.5.20.3 "no tags is provided"). Unlike [`Self::tag_list`]
    /// this preserves the raw comma-separated form including any
    /// leading / trailing whitespace.
    pub fn tags_opt(&self) -> Option<&str> {
        if self.tags.is_empty() {
            None
        } else {
            Some(self.tags.as_str())
        }
    }
}

/// Accessibility Text descriptive property (`altt`) — HEIF §6.5.21.
///
/// Carries an alternate text string suitable for use when the
/// associated image cannot be displayed — the role is analogous to the
/// HTML `alt` attribute. The language of the alternate text is
/// signalled by an RFC 5646 language tag string carried alongside.
///
/// Per §6.5.21.1 the property is a descriptive item property with
/// `Quantity (per item): Zero or more`. When multiple instances of
/// `AccessibilityTextProperty` are associated with the same item, they
/// represent alternatives possibly expressed in different languages;
/// a reader picks the most appropriate. The spec adds a "should" that
/// at most one instance with the same `alt_lang` value applies to a
/// single item — the parser preserves every instance verbatim so a
/// caller wanting to enforce that policy can do so over the property
/// list it walks.
///
/// The wire layout (§6.5.21.2) is two sequential null-terminated UTF-8
/// strings inside a FullBox(`altt`, version=0, flags=0):
///
/// ```text
/// utf8string alt_text;
/// utf8string alt_lang;
/// ```
///
/// Per §6.5.21.3:
///
/// * `alt_text` is the human-readable alternate text. The §6.5.21
///   text does not promote an empty `alt_text` to "absent"; the
///   parser nonetheless preserves an empty string verbatim and the
///   [`Self::alt_text_opt`] helper offers a strongly typed `Option`
///   shape for callers who want to skip an empty string.
/// * `alt_lang` is an RFC 5646 compliant language tag string
///   (e.g. `"en-US"`, `"fr-FR"`, `"zh-CN"`). When `alt_lang` is empty
///   the language is **unknown/undefined**; [`Self::alt_lang_opt`]
///   projects the empty form to `None`.
///
/// The wire layout matches §6.5.20 `udes` structurally (FullBox header
/// followed by null-terminated UTF-8 strings) but carries only two
/// fields rather than four, and reverses the documented field order
/// (`udes` lists `lang` first; `altt` lists `alt_text` first followed
/// by `alt_lang`). The parser surfaces the fields under their HEIF
/// names so a caller cross-referencing with the spec sees the same
/// identifiers.
///
/// Spec: ISO/IEC 23008-12 §6.5.21.2 — FullBox(`altt`, version=0,
/// flags=0).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Altt {
    /// Alternate text for the associated image (HTML-`alt`-style).
    /// An empty string is the documented "no text" shape; the
    /// [`Self::alt_text_opt`] helper projects that to `None`.
    pub alt_text: String,
    /// RFC 5646 language tag for [`Self::alt_text`]; empty = the
    /// language is unknown/undefined (§6.5.21.3).
    pub alt_lang: String,
}

impl Altt {
    /// `alt_text` typed as `Option<&str>`: `None` when the field is
    /// empty. §6.5.21.3 does not document an explicit "absent"
    /// sentinel for `alt_text` — the property is informative — but
    /// projecting the empty string to `None` lets a caller branch on
    /// "no alternate text was carried" without re-checking
    /// `is_empty()`.
    pub fn alt_text_opt(&self) -> Option<&str> {
        if self.alt_text.is_empty() {
            None
        } else {
            Some(self.alt_text.as_str())
        }
    }

    /// `alt_lang` typed as `Option<&str>`: `None` when the field is
    /// empty (§6.5.21.3 "the language is unknown/undefined").
    pub fn alt_lang_opt(&self) -> Option<&str> {
        if self.alt_lang.is_empty() {
            None
        } else {
            Some(self.alt_lang.as_str())
        }
    }
}

/// Auto Exposure Information descriptive property (`aebr`) —
/// HEIF §6.5.22.
///
/// Carries the exposure variation of the associated image item
/// relative to the camera settings (i.e. the offset, in number of
/// stops, applied by an auto-exposure-bracketing routine). The
/// property is associated with one image item out of an `aebr` entity
/// group (§6.8.6) so a reader can identify the relative position of a
/// frame inside an exposure-bracketed burst.
///
/// Per §6.5.22.1 the property is a descriptive item property with
/// `Quantity (per item): At most one` — a single item carries zero or
/// one `aebr` instance.
///
/// The wire layout (§6.5.22.2) is two signed bytes inside a
/// FullBox(`aebr`, version=0, flags=0):
///
/// ```text
/// int(8) exposure_step;
/// int(8) exposure_numerator;
/// ```
///
/// Per §6.5.22.3:
///
/// * `exposure_step` selects the bracketing increment. The spec
///   enumerates four values explicitly: `1` = full-stop increment,
///   `2` = half-stop, `3` = third-stop, `4` = quarter-stop. Other
///   values are **reserved**; the parser preserves the raw value so a
///   future-revision producer is round-tripped, and the
///   [`Aebr::is_defined_step`] helper exposes the §6.5.22.3
///   enumeration check.
/// * `exposure_numerator` specifies the numerator used to compute the
///   exposure offset, expressed as
///   `exposure_numerator / exposure_step` stops.
///
/// Note: the spec text declares both fields as `int(8)` (signed). The
/// numerator carries a signed direction (negative = darker than the
/// camera setting, positive = brighter) so the signed interpretation
/// is load-bearing for downstream consumers. The parser surfaces the
/// raw bytes as `i8` and the [`Aebr::exposure_stops`] helper exposes
/// the float-valued offset.
///
/// Spec: ISO/IEC 23008-12 §6.5.22.2 — FullBox(`aebr`, version=0,
/// flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Aebr {
    /// Bracketing increment selector. Defined values per §6.5.22.3
    /// are `1` (full stop), `2` (half), `3` (third), `4` (quarter);
    /// every other value is reserved. The parser preserves the raw
    /// byte verbatim.
    pub exposure_step: i8,
    /// Exposure numerator. The exposure offset in stops is
    /// `exposure_numerator / exposure_step` (§6.5.22.3).
    pub exposure_numerator: i8,
}

impl Aebr {
    /// Full-stop bracketing increment (§6.5.22.3 `exposure_step == 1`).
    pub const STEP_FULL: i8 = 1;
    /// Half-stop bracketing increment (§6.5.22.3 `exposure_step == 2`).
    pub const STEP_HALF: i8 = 2;
    /// Third-stop bracketing increment (§6.5.22.3 `exposure_step == 3`).
    pub const STEP_THIRD: i8 = 3;
    /// Quarter-stop bracketing increment (§6.5.22.3
    /// `exposure_step == 4`).
    pub const STEP_QUARTER: i8 = 4;

    /// True when [`Self::exposure_step`] is one of the four defined
    /// values from §6.5.22.3 (`1` / `2` / `3` / `4`). Other values
    /// are reserved and a strict reader may surface a diagnostic.
    pub fn is_defined_step(&self) -> bool {
        matches!(
            self.exposure_step,
            Self::STEP_FULL | Self::STEP_HALF | Self::STEP_THIRD | Self::STEP_QUARTER
        )
    }

    /// The exposure offset expressed in number of stops:
    /// `exposure_numerator / exposure_step` (§6.5.22.3).
    ///
    /// Returns `None` when `exposure_step == 0` — the spec lists `0`
    /// as a reserved value and dividing by it is undefined. Callers
    /// that want to gate on the §6.5.22.3 enumeration explicitly
    /// should consult [`Self::is_defined_step`] first.
    pub fn exposure_stops(&self) -> Option<f64> {
        if self.exposure_step == 0 {
            return None;
        }
        Some(f64::from(self.exposure_numerator) / f64::from(self.exposure_step))
    }
}

/// White Balance Information descriptive property (`wbbr`) —
/// HEIF §6.5.23.
///
/// Carries the white-balance compensation applied to the associated
/// image item relative to the camera settings: a blue/amber bias
/// expressed as a colour-temperature component in Kelvin, plus a
/// green/magenta bias expressed as a colour-deviation component in
/// units of 1/100 Duv (distance to the blackbody locus). The
/// property is associated with one image item out of a `wbbr` entity
/// group (§6.8.6) so a reader can identify the relative position of
/// a frame inside a white-balance bracketed burst.
///
/// Per §6.5.23.1 the property is a descriptive item property with
/// `Quantity (per item): At most one` — a single item carries zero
/// or one `wbbr` instance.
///
/// The wire layout (§6.5.23.2) is a 16-bit unsigned colour
/// temperature followed by an 8-bit signed colour deviation, inside
/// a FullBox(`wbbr`, version=0, flags=0):
///
/// ```text
/// unsigned int(16) blue_amber;
/// int(8)           green_magenta;
/// ```
///
/// Per §6.5.23.3:
///
/// * `blue_amber` is an unsigned integer indicating the colour
///   temperature component of the white balance, in Kelvin.
/// * `green_magenta` is a signed integer indicating the colour
///   deviation component of the white balance, in units of 1/100
///   Duv (distance to the blackbody locus). The §6.5.23.3 NOTE
///   states that a Duv of 0 indicates a light source that is
///   neutral, a negative Duv indicates a magenta colour shift, and
///   a positive Duv indicates a green colour shift. The
///   [`Wbbr::green_magenta_duv`] helper exposes the Duv value
///   itself (`green_magenta / 100.0`) so callers don't re-derive
///   the unit conversion.
///
/// Note: the spec text declares `green_magenta` as `int(8)` (signed).
/// A negative value carries a signed direction (magenta shift), so
/// the signed interpretation is load-bearing for downstream
/// consumers. The parser surfaces the raw byte as `i8`.
///
/// Spec: ISO/IEC 23008-12 §6.5.23.2 — FullBox(`wbbr`, version=0,
/// flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wbbr {
    /// Colour-temperature component of the white balance in Kelvin
    /// (§6.5.23.3). The wire field is `unsigned int(16)` so the
    /// representable range is 0..=65535 K — wide enough for the
    /// practical photographic span (a candle is ≈ 1850 K, midday
    /// daylight ≈ 5600 K, a clear-sky blue cast ≈ 10000 K).
    pub blue_amber: u16,
    /// Colour-deviation component of the white balance in units of
    /// 1/100 Duv (§6.5.23.3). Signed so a negative value carries
    /// the magenta direction and a positive value the green
    /// direction; the parser preserves the raw signed byte
    /// verbatim. See [`Wbbr::green_magenta_duv`] for the Duv-unit
    /// projection.
    pub green_magenta: i8,
}

impl Wbbr {
    /// The §6.5.23.3 NOTE sentinel for a neutral light source:
    /// `green_magenta == 0` carries a Duv of zero, i.e. the
    /// associated image item was captured without any green/magenta
    /// compensation relative to the camera-set white balance. The
    /// `blue_amber` field is not bound by this sentinel.
    pub const NEUTRAL_GREEN_MAGENTA: i8 = 0;

    /// The §6.5.23.3 colour-deviation expressed in Duv (the
    /// distance from the blackbody locus). The wire field is in
    /// units of 1/100 Duv, so the projection is
    /// `green_magenta / 100.0`. A negative value indicates a
    /// magenta colour shift and a positive value indicates a green
    /// colour shift, per the §6.5.23.3 NOTE.
    pub fn green_magenta_duv(&self) -> f64 {
        f64::from(self.green_magenta) / 100.0
    }

    /// True when `green_magenta == 0` — the §6.5.23.3 NOTE neutral
    /// sentinel, i.e. no green/magenta compensation relative to the
    /// camera-set white balance. The `blue_amber` (colour
    /// temperature) component is independent and is not consulted.
    pub fn is_green_magenta_neutral(&self) -> bool {
        self.green_magenta == Self::NEUTRAL_GREEN_MAGENTA
    }
}

/// Focus Information descriptive property (`fobr`) — HEIF §6.5.24.
///
/// Carries the focus variation of the associated image item relative
/// to the camera settings. The focus distance is expressed in metres
/// as the ratio of [`Self::focus_distance_numerator`] over
/// [`Self::focus_distance_denominator`]. Per the §6.5.24.3 sentinel,
/// **focus at infinity is signalled by division by zero** — i.e.
/// `focus_distance_denominator == 0` AND
/// `focus_distance_numerator should be 0`. The property identifies
/// one image item out of a `fobr` entity group (§6.8.6) so a reader
/// can place a frame inside a focus-bracketed burst.
///
/// Per §6.5.24.1 the property is a descriptive item property with
/// `Quantity (per item): At most one` — a single item carries zero
/// or one `fobr` instance.
///
/// The wire layout (§6.5.24.2) is two consecutive 16-bit unsigned
/// integers inside a FullBox(`fobr`, version=0, flags=0):
///
/// ```text
/// unsigned int(16) focus_distance_numerator;
/// unsigned int(16) focus_distance_denominator;
/// ```
///
/// Per §6.5.24.3 the focus distance in metres is the ratio
/// `focus_distance_numerator / focus_distance_denominator`. A
/// denominator of zero is the §6.5.24.3 infinity sentinel: focus at
/// infinity, with the numerator also `should` be zero. The
/// [`Fobr::focus_distance_metres`] helper returns `None` for the
/// infinity sentinel and `Some(metres)` otherwise so callers don't
/// re-derive the ratio. The [`Fobr::is_focus_at_infinity`]
/// predicate exposes the sentinel check itself.
///
/// Spec: ISO/IEC 23008-12 §6.5.24.2 — FullBox(`fobr`, version=0,
/// flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fobr {
    /// Numerator of the focus-distance ratio (§6.5.24.3). Unsigned
    /// 16-bit; combined with [`Self::focus_distance_denominator`]
    /// gives the focus distance in metres. The infinity sentinel
    /// pairs a zero denominator with a zero numerator per the
    /// §6.5.24.3 `should`.
    pub focus_distance_numerator: u16,
    /// Denominator of the focus-distance ratio (§6.5.24.3).
    /// Unsigned 16-bit. A value of zero is the §6.5.24.3
    /// "focus at infinity" sentinel; otherwise the focus distance
    /// in metres is `focus_distance_numerator /
    /// focus_distance_denominator`.
    pub focus_distance_denominator: u16,
}

impl Fobr {
    /// The §6.5.24.3 "focus at infinity" sentinel value for the
    /// denominator: `focus_distance_denominator == 0` signals
    /// focus at infinity (and the numerator `should` also be zero
    /// per the spec NOTE).
    pub const INFINITY_DENOMINATOR: u16 = 0;

    /// The focus distance in metres per §6.5.24.3
    /// (`focus_distance_numerator / focus_distance_denominator`),
    /// or `None` when the denominator is the §6.5.24.3 infinity
    /// sentinel (zero). The numerator is intentionally NOT
    /// validated against the `should be equal to 0` part of the
    /// sentinel because §6.5.24.3 expresses it as a writer
    /// recommendation; the predicate is purely on the denominator
    /// per the `i.e.` clause.
    pub fn focus_distance_metres(&self) -> Option<f64> {
        if self.focus_distance_denominator == Self::INFINITY_DENOMINATOR {
            None
        } else {
            Some(
                f64::from(self.focus_distance_numerator)
                    / f64::from(self.focus_distance_denominator),
            )
        }
    }

    /// True when the denominator is the §6.5.24.3 infinity sentinel
    /// (zero). The numerator's `should be equal to 0` is not
    /// consulted here — see [`Self::has_well_formed_infinity_sentinel`]
    /// for the stricter combined check.
    pub fn is_focus_at_infinity(&self) -> bool {
        self.focus_distance_denominator == Self::INFINITY_DENOMINATOR
    }

    /// True when the property carries the §6.5.24.3 infinity
    /// sentinel in its strict shape: BOTH numerator AND denominator
    /// are zero, matching the spec's "`focus_distance_denominator`
    /// is equal to 0 and `focus_distance_numerator` should be equal
    /// to 0" clause. Returns `false` for a denominator-only zero
    /// (which is still infinity per
    /// [`Self::is_focus_at_infinity`] but violates the writer
    /// `should`) and for any non-infinity reading.
    pub fn has_well_formed_infinity_sentinel(&self) -> bool {
        self.focus_distance_denominator == Self::INFINITY_DENOMINATOR
            && self.focus_distance_numerator == 0
    }
}

/// Flash Exposure Information descriptive property (`afbr`) —
/// HEIF §6.5.25.
///
/// Carries the flash exposure variation of the associated image item
/// relative to the camera settings, expressed in **number of f-stops**
/// as the ratio of [`Self::flash_exposure_numerator`] over
/// [`Self::flash_exposure_denominator`]. The property identifies one
/// image item out of an `afbr` entity group (§6.8.6) so a reader can
/// place a frame inside a flash-bracketed burst.
///
/// Per §6.5.25.1 the property is a descriptive item property with
/// `Quantity (per item): At most one` — a single item carries zero
/// or one `afbr` instance.
///
/// The wire layout (§6.5.25.2) is two consecutive **signed** bytes
/// inside a FullBox(`afbr`, version=0, flags=0):
///
/// ```text
/// int(8) flash_exposure_numerator;
/// int(8) flash_exposure_denominator;
/// ```
///
/// Per §6.5.25.3 the flash-exposure value of the sample is expressed
/// in number of f-stops as `flash_exposure_numerator /
/// flash_exposure_denominator`. The fields are signed so a negative
/// numerator carries an under-exposed (darker) flash setting and a
/// positive numerator an over-exposed (brighter) flash setting
/// relative to the camera-set flash exposure.
///
/// The spec does NOT carve out a dedicated infinity sentinel for
/// `afbr` (unlike the §6.5.24 `fobr` divide-by-zero infinity
/// reading). A denominator of zero is therefore mathematically
/// undefined; the [`Afbr::flash_exposure_stops`] helper returns
/// `None` in that case so callers don't trip a division-by-zero
/// panic on a malformed reading, mirroring the `aebr` /
/// `Aebr::exposure_stops` and `fobr` / `Fobr::focus_distance_metres`
/// patterns on the sibling parsers.
///
/// Note: the spec text declares both fields as `int(8)` (signed). The
/// signed interpretation is load-bearing for downstream consumers
/// because a flash-bracketed burst routinely carries both signs. The
/// parser surfaces the raw bytes as `i8`.
///
/// Spec: ISO/IEC 23008-12 §6.5.25.2 — FullBox(`afbr`, version=0,
/// flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Afbr {
    /// Numerator of the flash-exposure ratio (§6.5.25.3). Signed
    /// 8-bit; combined with [`Self::flash_exposure_denominator`]
    /// gives the flash exposure in number of f-stops. A negative
    /// value carries an under-exposed (darker) flash position
    /// relative to the camera-set flash exposure.
    pub flash_exposure_numerator: i8,
    /// Denominator of the flash-exposure ratio (§6.5.25.3). Signed
    /// 8-bit. The spec does not carve out a dedicated sentinel for
    /// a zero denominator — a zero is mathematically undefined and
    /// is surfaced as `None` by [`Self::flash_exposure_stops`].
    pub flash_exposure_denominator: i8,
}

impl Afbr {
    /// The flash exposure value in number of f-stops per §6.5.25.3
    /// (`flash_exposure_numerator / flash_exposure_denominator`),
    /// or `None` when the denominator is zero (mathematically
    /// undefined; the spec does not carve out a dedicated sentinel
    /// for this case). The signed-i8 numerator and denominator are
    /// widened to `f64` so the ratio carries the full
    /// `i8::MIN / 1` … `i8::MAX / 1` span without saturation, and
    /// the `i8::MIN / -1` case (which would overflow an
    /// integer-only divide) round-trips as `128.0`.
    pub fn flash_exposure_stops(&self) -> Option<f64> {
        if self.flash_exposure_denominator == 0 {
            return None;
        }
        Some(f64::from(self.flash_exposure_numerator) / f64::from(self.flash_exposure_denominator))
    }
}

/// Depth of Field Information descriptive property (`dobr`) —
/// HEIF §6.5.26.
///
/// Carries the depth-of-field variation of the associated image item
/// relative to the camera settings, expressed as an **aperture change**
/// in a number of stops, as the ratio of [`Self::f_stop_numerator`]
/// over [`Self::f_stop_denominator`]. The property identifies one
/// image item out of a `dobr` entity group (§6.8.6) so a reader can
/// place a frame inside a depth-of-field-bracketed burst.
///
/// Per §6.5.26.1 the property is a descriptive item property with
/// `Quantity (per item): At most one` — a single item carries zero
/// or one `dobr` instance.
///
/// The wire layout (§6.5.26.2) is two consecutive **signed** bytes
/// inside a FullBox(`dobr`, version=0, flags=0):
///
/// ```text
/// int(8) f_stop_numerator;
/// int(8) f_stop_denominator;
/// ```
///
/// Per §6.5.26.3 the depth-of-field variation is expressed as an
/// aperture change in a number of stops, computed as `f_stop_numerator
/// / f_stop_denominator`. The fields are signed so a negative
/// numerator carries an aperture change toward a smaller f-number
/// (shallower depth of field) and a positive numerator toward a larger
/// f-number (deeper depth of field) relative to the camera-set
/// aperture.
///
/// The spec does NOT carve out a dedicated infinity sentinel for
/// `dobr` (unlike the §6.5.24 `fobr` divide-by-zero infinity
/// reading). A denominator of zero is therefore mathematically
/// undefined; the [`Dobr::aperture_stops`] helper returns `None` in
/// that case so callers don't trip a division-by-zero panic on a
/// malformed reading, mirroring the `afbr` / [`Afbr::flash_exposure_stops`]
/// pattern on the structurally identical sibling parser.
///
/// Note: the spec text declares both fields as `int(8)` (signed). The
/// signed interpretation is load-bearing for downstream consumers
/// because a depth-of-field-bracketed burst routinely carries both
/// signs. The parser surfaces the raw bytes as `i8`.
///
/// Spec: ISO/IEC 23008-12 §6.5.26.2 — FullBox(`dobr`, version=0,
/// flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dobr {
    /// Numerator of the aperture-change ratio (§6.5.26.3). Signed
    /// 8-bit; combined with [`Self::f_stop_denominator`] gives the
    /// depth-of-field variation in number of stops. A negative value
    /// carries an aperture change toward a shallower depth of field
    /// relative to the camera-set aperture.
    pub f_stop_numerator: i8,
    /// Denominator of the aperture-change ratio (§6.5.26.3). Signed
    /// 8-bit. The spec does not carve out a dedicated sentinel for a
    /// zero denominator — a zero is mathematically undefined and is
    /// surfaced as `None` by [`Self::aperture_stops`].
    pub f_stop_denominator: i8,
}

impl Dobr {
    /// The depth-of-field variation expressed as an aperture change in
    /// a number of stops per §6.5.26.3 (`f_stop_numerator /
    /// f_stop_denominator`), or `None` when the denominator is zero
    /// (mathematically undefined; the spec does not carve out a
    /// dedicated sentinel for this case). The signed-i8 numerator and
    /// denominator are widened to `f64` so the ratio carries the full
    /// `i8::MIN / 1` … `i8::MAX / 1` span without saturation, and the
    /// `i8::MIN / -1` case (which would overflow an integer-only
    /// divide) round-trips as `128.0`.
    pub fn aperture_stops(&self) -> Option<f64> {
        if self.f_stop_denominator == 0 {
            return None;
        }
        Some(f64::from(self.f_stop_numerator) / f64::from(self.f_stop_denominator))
    }
}

/// Grid-shape tail of a Panorama Information property (`pano`) —
/// HEIF §6.5.27.2.
///
/// Present on the wire **only** when `panorama_direction` signals one
/// of the two grid panorama types (`4` raster scan, `5` continuous
/// order); for the four linear directions (`0..=3`) the property body
/// ends after the direction byte and this struct is absent
/// ([`Pano::grid`] is `None`).
///
/// Both fields are stored minus-one per §6.5.27.3, so the wire value
/// `0` means one row / one column. The [`Self::rows`] / [`Self::columns`]
/// projections add the one back, widening to `u16` so the `255 + 1`
/// endpoint doesn't wrap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanoGrid {
    /// Number of rows in the grid **minus one** (§6.5.27.3).
    pub rows_minus_one: u8,
    /// Number of columns in the grid **minus one** (§6.5.27.3).
    pub columns_minus_one: u8,
}

impl PanoGrid {
    /// Number of rows in the grid (§6.5.27.3 `rows_minus_one + 1`),
    /// widened to `u16` so the `255` wire endpoint reads as `256`
    /// instead of wrapping to `0`.
    pub fn rows(&self) -> u16 {
        u16::from(self.rows_minus_one) + 1
    }

    /// Number of columns in the grid (§6.5.27.3
    /// `columns_minus_one + 1`), widened to `u16` so the `255` wire
    /// endpoint reads as `256` instead of wrapping to `0`.
    pub fn columns(&self) -> u16 {
        u16::from(self.columns_minus_one) + 1
    }
}

/// Panorama Information descriptive property (`pano`) — HEIF §6.5.27.
///
/// Defines the characteristics associated with a panorama declared by
/// a `'pano'` entity group (§6.8.8.1): the type of panorama and the
/// scanning order of the input images composing it. Per §6.5.27.1 the
/// property `should` only be associated with an entity group whose
/// `grouping_type` is `'pano'` (see
/// [`EntityGroup::is_panorama`](crate::derived::EntityGroup::is_panorama)),
/// and the quantity per associated item is at most one.
///
/// The wire layout (§6.5.27.2) is a FullBox(`pano`, version=0,
/// flags=0) followed by:
///
/// ```text
/// unsigned int(8) panorama_direction;
/// if (panorama_direction >= 4 && panorama_direction <= 5) { // grid
///     unsigned int(8) rows_minus_one;
///     unsigned int(8) columns_minus_one;
/// }
/// ```
///
/// i.e. the two grid-shape bytes are **conditionally present** — they
/// exist only for the two grid directions, surfaced here as
/// [`Self::grid`] being `Some` exactly when
/// `panorama_direction ∈ {4, 5}`.
///
/// Per §6.5.27.3 the direction values are:
///
/// | value | meaning |
/// |-------|---------|
/// | 0     | left-to-right horizontal panorama |
/// | 1     | right-to-left horizontal panorama |
/// | 2     | bottom-to-top vertical panorama |
/// | 3     | top-to-bottom vertical panorama |
/// | 4     | grid panorama in raster scan order (rows left-to-right, top-to-bottom from the top-left corner) |
/// | 5     | grid panorama in continuous order (boustrophedon: first row left-to-right, second right-to-left, …) |
/// | other | undefined |
///
/// An undefined direction (`>= 6`) is **not** a parse error — the raw
/// value is preserved and [`Self::is_defined_direction`] reports
/// `false`, so a reader can skip the panorama reconstruction while
/// still walking the rest of the file.
///
/// Spec: ISO/IEC 23008-12 §6.5.27.2 — FullBox(`pano`, version=0,
/// flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pano {
    /// Type of panorama + scanning order of the input images
    /// (§6.5.27.3). See the direction table on [`Pano`]; values `>= 6`
    /// are undefined and preserved verbatim.
    pub panorama_direction: u8,
    /// Grid shape — present on the wire only when
    /// [`Self::panorama_direction`] is one of the two grid types
    /// ([`Self::DIRECTION_GRID_RASTER`] /
    /// [`Self::DIRECTION_GRID_CONTINUOUS`]); `None` for the four
    /// linear directions and for undefined direction values.
    pub grid: Option<PanoGrid>,
}

impl Pano {
    /// §6.5.27.3 value 0 — left-to-right horizontal panorama.
    pub const DIRECTION_LEFT_TO_RIGHT: u8 = 0;
    /// §6.5.27.3 value 1 — right-to-left horizontal panorama.
    pub const DIRECTION_RIGHT_TO_LEFT: u8 = 1;
    /// §6.5.27.3 value 2 — bottom-to-top vertical panorama.
    pub const DIRECTION_BOTTOM_TO_TOP: u8 = 2;
    /// §6.5.27.3 value 3 — top-to-bottom vertical panorama.
    pub const DIRECTION_TOP_TO_BOTTOM: u8 = 3;
    /// §6.5.27.3 value 4 — grid panorama in raster scan order (rows
    /// and columns organised left-to-right and top-to-bottom starting
    /// from the top-left corner).
    pub const DIRECTION_GRID_RASTER: u8 = 4;
    /// §6.5.27.3 value 5 — grid panorama in continuous order
    /// (starting from the top-left corner the first row is organised
    /// left-to-right, the second right-to-left, the third
    /// left-to-right, and so on).
    pub const DIRECTION_GRID_CONTINUOUS: u8 = 5;

    /// True when [`Self::panorama_direction`] is one of the six
    /// §6.5.27.3 defined values (`0..=5`); `false` for the
    /// spec-undefined remainder.
    pub fn is_defined_direction(&self) -> bool {
        self.panorama_direction <= Self::DIRECTION_GRID_CONTINUOUS
    }

    /// True when the direction signals one of the two grid panorama
    /// types (§6.5.27.2 syntax condition `panorama_direction >= 4 &&
    /// panorama_direction <= 5`) — exactly the case where
    /// [`Self::grid`] carries the grid shape.
    pub fn is_grid(&self) -> bool {
        self.panorama_direction >= Self::DIRECTION_GRID_RASTER
            && self.panorama_direction <= Self::DIRECTION_GRID_CONTINUOUS
    }
}

/// One sub-sample of a `subs` Sub-Sample Information descriptive property
/// (HEIF §6.5.28). Each entry mirrors the inner loop body of the
/// `SubSampleInformationBox` (ISO/IEC 14496-12 §8.7.7.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubsEntry {
    /// Size, in bytes, of this sub-sample (ISO/IEC 14496-12 §8.7.7.3).
    /// Carried on the wire as 32-bit when the box `version == 1` and
    /// 16-bit when `version == 0`; surfaced here widened to `u32` so the
    /// caller need not branch on the box version.
    pub subsample_size: u32,
    /// Degradation priority — higher values mark sub-samples with greater
    /// impact on decoded quality (§8.7.7.3).
    pub subsample_priority: u8,
    /// `0` — required to decode the sample; `1` — not required but may
    /// carry enhancement data such as SEI messages (§8.7.7.3).
    pub discardable: u8,
    /// Codec-defined parameter block; `0` when the coding format gives no
    /// definition (§8.7.7.3).
    pub codec_specific_parameters: u32,
}

/// Sub-Sample Information descriptive property (`subs`) — HEIF §6.5.28.
///
/// For coded image items, sub-sample information is given using an
/// associated item property whose body is exactly a
/// `SubSampleInformationBox` as defined in ISO/IEC 14496-12 §8.7.7 for
/// the coding format of the associated coded image item (§6.5.28).
///
/// HEIF §6.5.28 constrains the box when used as an item property: the
/// `entry_count` field shall be `1` and the single entry's `sample_delta`
/// shall be `0`. The parser enforces both and surfaces only the
/// resulting list of sub-samples — the lone entry's `subsample_count`
/// loop body — since the spec fixes the surrounding table to a single
/// degenerate row.
///
/// The wire layout (ISO/IEC 14496-12 §8.7.7.2) is a
/// `FullBox('subs', version, flags)` followed by `entry_count`, then per
/// entry a `sample_delta`, a `subsample_count`, and that many
/// `(subsample_size, subsample_priority, discardable,
/// codec_specific_parameters)` tuples. `subsample_size` is 32-bit when
/// `version == 1`, else 16-bit. Zero or more `subs` may be associated
/// with the same item; when more than one is present their `flags` shall
/// differ, so the property surfaces [`Self::flags`] verbatim to let a
/// caller key the codec-specific meaning of each box.
///
/// The property is descriptive (`Quantity (per item): Zero or more for a
/// coded image item`); a `subs` association is therefore never essential
/// in a way this crate cannot honour, and surfacing the typed entries
/// keeps it off [`Meta::unsupported_essential_properties`].
///
/// Spec: ISO/IEC 23008-12 §6.5.28; ISO/IEC 14496-12 §8.7.7.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Subs {
    /// `flags` from the `FullBox` header. The semantics, if any, are
    /// supplied by the coding format; when more than one `subs` is
    /// associated with an item their `flags` differ (§6.5.28).
    pub flags: u32,
    /// The single entry's sub-samples, in wire order (§8.7.7.2 inner
    /// loop). Empty when the entry's `subsample_count` is `0`.
    pub entries: Vec<SubsEntry>,
}

/// Target Output Layer Set descriptive property (`tols`) — HEIF
/// §6.5.29.
///
/// Provides the output layer set index to be used as input for the
/// decoding process of the associated coded image item (§6.5.29.1). For
/// a layered coding format that carries several output layer sets, this
/// property pins which set the decoder must select before reconstructing
/// the image item.
///
/// The wire layout (§6.5.29.2) is an `ItemFullProperty('tols',
/// version=0, flags=0)` followed by a single big-endian
/// `unsigned int(16) target_ols_idx`:
///
/// ```text
/// unsigned int(16) target_ols_idx;
/// ```
///
/// Per §6.5.29.1 the property is descriptive, `Quantity (per item): Zero
/// or one for a coded image item`, and — unlike the rest of the
/// descriptive §6.5.x family — `essential shall be equal to 1`: a reader
/// that cannot honour the requested output layer set must not process
/// the item. Because the parser recognises and surfaces the field, a
/// `tols` association does not trip
/// [`Meta::unsupported_essential_properties`]; a caller that lacks the
/// layered-decode path consults [`Self::target_ols_idx`] and routes
/// accordingly.
///
/// Per §6.5.29.3 the precise meaning of `target_ols_idx` is specific to
/// the coding format of the image item and is defined by each coding
/// format that requires the property, so the value is surfaced verbatim
/// without further interpretation here.
///
/// Spec: ISO/IEC 23008-12 §6.5.29.2 — ItemFullProperty(`tols`,
/// version=0, flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tols {
    /// Output layer set index to be provided to the decoding process
    /// (§6.5.29.3). Interpretation is coding-format specific.
    pub target_ols_idx: u16,
}

/// Progressive Derived Image Item Information descriptive property
/// (`prdi`) — HEIF §6.5.37.
///
/// Describes the progressive rendering steps associated with a derived
/// image item (§6.5.37.1). Each progressive rendering step specifies the
/// number of candidate input image items to use for the reconstruction
/// of the derived image item, described as a difference from the previous
/// step. Quantity per item is zero-or-one for a derived image item.
///
/// Uniquely among the §6.5.x descriptive family parsed so far, the body
/// is **gated by the box `flags`** — there is no payload byte that is
/// unconditionally present. The wire layout (§6.5.37.2) is an
/// `ItemFullProperty('prdi', version=0, flags)` followed by:
///
/// ```text
/// if ((flags & one_item_per_step) == 0 ||
///     (flags & alternative_is_candidate))
///    unsigned int(16) step_count;
/// if ((flags & one_item_per_step) == 0) {
///    for (i = 0; i < step_count; i++)
///       unsigned int(16) item_count;
/// }
/// ```
///
/// Three `flags` bits are defined (§6.5.37.1):
///
/// | bit | name | meaning when set |
/// |-----|------|------------------|
/// | `0x000001` | [`Self::FLAG_ITEM_REFERENCE_ORDER`] | candidate input image items follow the `'dimg'` reference declaration order; when not set they follow the file order of appearance of the items' data |
/// | `0x000002` | [`Self::FLAG_ONE_ITEM_PER_STEP`] | each progressive rendering step consumes exactly one candidate input image item; when not set the per-step counts are listed explicitly |
/// | `0x000004` | [`Self::FLAG_ALTERNATIVE_IS_CANDIDATE`] | an alternative (`'prgr'`/`'altr'` group member or thumbnail) to an input image item is also a candidate input image item |
///
/// The conditional shape follows directly:
///
/// * `one_item_per_step` clear → every step consumes a per-step
///   `item_count`, so `step_count` is present and the `item_count[]`
///   array follows.
/// * `one_item_per_step` set + `alternative_is_candidate` set →
///   `step_count` is present (the step count is no longer derivable from
///   `'dimg'` because alternatives extend the candidate list) but the
///   per-step `item_count` array is **not** (each step consumes one item).
/// * `one_item_per_step` set + `alternative_is_candidate` clear →
///   neither field is present; per §6.5.37.3 `step_count` is then
///   inferred to be the number of input image items listed in `'dimg'`
///   and each `item_count` is inferred to be `1`.
///
/// [`Self::step_count`] / [`Self::item_counts`] are therefore `Option`,
/// carrying `Some` exactly when the field was present on the wire. A
/// reader resolves the inferred values against the derived item's
/// `'dimg'` reference; [`Self::step_count_or_inferred`] applies the
/// §6.5.37.3 inference rule given that input count.
///
/// `step_count` semantics (§6.5.37.3): `> 1` is the number of
/// progressive rendering steps; `== 1` indicates progressive rendering is
/// not desired; `== 0` is reserved for future use by ISO/IEC.
///
/// Spec: ISO/IEC 23008-12 §6.5.37.2 — ItemFullProperty(`prdi`,
/// version=0, flags).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prdi {
    /// The full 24-bit `flags` field of the `ItemFullProperty` header.
    /// The three defined bits (`item_reference_order`,
    /// `one_item_per_step`, `alternative_is_candidate`) are surfaced via
    /// the `is_*` projections; the field is kept whole so an unknown
    /// future flag bit round-trips.
    pub flags: u32,
    /// `step_count` (§6.5.37.3) when present on the wire — `Some` exactly
    /// when `one_item_per_step` is clear **or**
    /// `alternative_is_candidate` is set. `None` means the count is
    /// inferred to be the number of `'dimg'` input image items (see
    /// [`Self::step_count_or_inferred`]).
    pub step_count: Option<u16>,
    /// Per-step `item_count` array (§6.5.37.3) when present — `Some`
    /// exactly when `one_item_per_step` is clear, in which case it has
    /// exactly `step_count` entries. `None` means each step consumes one
    /// candidate input image item (the inferred `item_count == 1`).
    pub item_counts: Option<Vec<u16>>,
}

impl Prdi {
    /// §6.5.37.1 flag `0x000001` — candidate input image items follow
    /// the `'dimg'` item-reference declaration order.
    pub const FLAG_ITEM_REFERENCE_ORDER: u32 = 0x0000_0001;
    /// §6.5.37.1 flag `0x000002` — each progressive rendering step
    /// consumes exactly one candidate input image item.
    pub const FLAG_ONE_ITEM_PER_STEP: u32 = 0x0000_0002;
    /// §6.5.37.1 flag `0x000004` — an alternative to an input image item
    /// is also a candidate input image item.
    pub const FLAG_ALTERNATIVE_IS_CANDIDATE: u32 = 0x0000_0004;

    /// True when the `item_reference_order` flag (`0x000001`) is set.
    pub fn is_item_reference_order(&self) -> bool {
        self.flags & Self::FLAG_ITEM_REFERENCE_ORDER != 0
    }

    /// True when the `one_item_per_step` flag (`0x000002`) is set.
    pub fn is_one_item_per_step(&self) -> bool {
        self.flags & Self::FLAG_ONE_ITEM_PER_STEP != 0
    }

    /// True when the `alternative_is_candidate` flag (`0x000004`) is set.
    pub fn is_alternative_is_candidate(&self) -> bool {
        self.flags & Self::FLAG_ALTERNATIVE_IS_CANDIDATE != 0
    }

    /// The effective number of progressive rendering steps, applying the
    /// §6.5.37.3 inference rule: when [`Self::step_count`] was present on
    /// the wire it is returned verbatim; otherwise it is inferred to be
    /// `dimg_input_count`, the number of input image items listed in the
    /// derived item's `'dimg'` reference.
    pub fn step_count_or_inferred(&self, dimg_input_count: u16) -> u16 {
        self.step_count.unwrap_or(dimg_input_count)
    }

    /// The effective `item_count` for the `i`-th progressive rendering
    /// step, applying the §6.5.37.3 inference rule: when
    /// [`Self::item_counts`] is present it is indexed directly (returning
    /// `None` for an out-of-range step); otherwise each step is inferred
    /// to consume `1` candidate input image item.
    pub fn item_count_for_step(&self, i: usize) -> Option<u16> {
        match &self.item_counts {
            Some(counts) => counts.get(i).copied(),
            None => Some(1),
        }
    }
}

/// Single Stream descriptive property (`sstr`) — HEIF §6.5.38.
///
/// When associated with a derived image item, the `SingleStreamProperty`
/// indicates that the item data of the input image items collectively
/// form a single bitstream that is conformant to the coding format of the
/// input image items and is decodable with a single decoder (§6.5.38.1).
/// The input image items are concatenated in the order they are listed in
/// the derived item's `'dimg'` item reference to form the *derived
/// bitstream*, which conforms to the item type and decoder-configuration
/// item property of the **last** input image item.
///
/// The box carries no payload — it is the bare `ItemProperty('sstr')`
/// marker (§6.5.38.2), so its presence is the entire semantic. Quantity
/// per item is zero-or-one for a derived image item. This is a
/// *descriptive* property (it does not transform the reconstructed
/// representation).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sstr;

/// Text Layout descriptive property (`txlo`) — HEIF §6.10.2.1.
///
/// Documents parameters to render the payload of an associated text item
/// into a reference space, following the §6.10.2.1.1 layout algorithm: a
/// 2D coordinate system with origin `(0, 0)` at the top-left and a maximum
/// size of `reference_width` × `reference_height`, within which the text
/// is laid out starting at `(x, y)` using `direction` / `writing_mode` /
/// `font_size`. When this property is associated with a text item it shall
/// precede any transformative property.
///
/// The wire layout (§6.10.2.1.2) is an `ItemFullProperty('txlo',
/// version = 0, flags)` with a `(flags & 1)`-selected field size:
///
/// ```text
/// field_size = ((flags & 1) + 1) * 16;   // 16 or 32 bits
/// unsigned int(field_size) reference_width;
/// unsigned int(field_size) reference_height;
/// signed   int(field_size) x;
/// signed   int(field_size) y;
/// unsigned int(field_size) width;
/// unsigned int(field_size) height;
/// unsigned int(16)         font_size;     // 8.8 fixed-point percentage
/// utf8string               direction;
/// utf8string               writing_mode;
/// ```
///
/// `(flags & 1) == 0` selects 16-bit `reference_*` / `x` / `y` / `width` /
/// `height` fields; `(flags & 1) == 1` selects 32-bit. Values of `flags`
/// greater than `1` are reserved. `font_size` is an 8.8 representation of
/// the desired font size as a percentage of `reference_height`.
/// `direction` / `writing_mode` are valid W3C TTML2 `tts:direction` /
/// `tts:writingMode` attribute values (the spec defers their grammar to
/// TTML2, so they are surfaced verbatim).
///
/// Spec: ISO/IEC 23008-12 §6.10.2.1.2 — ItemFullProperty(`txlo`,
/// version=0, flags).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Txlo {
    /// The full 24-bit `flags` field. Only bit 0 (the field-size
    /// selector) is defined; the whole value is retained so a future
    /// reserved bit round-trips. [`Self::is_large_field_size`] projects
    /// bit 0.
    pub flags: u32,
    /// Width, in pixels, of the reference space (§6.10.2.1.3).
    pub reference_width: u32,
    /// Height, in pixels, of the reference space (§6.10.2.1.3).
    pub reference_height: u32,
    /// Start x position to render the text (§6.10.2.1.3); may be negative
    /// to place a top-left corner outside the image.
    pub x: i32,
    /// Start y position to render the text (§6.10.2.1.3); may be negative.
    pub y: i32,
    /// Width, in pixels, of the text content resulting from the text item.
    pub width: u32,
    /// Height, in pixels, of the text content resulting from the text item.
    pub height: u32,
    /// 8.8 fixed-point font size as a percentage of `reference_height`.
    /// Use [`Self::font_size_percent`] for the decoded floating value.
    pub font_size: u16,
    /// W3C TTML2 `tts:direction` value (e.g. `"ltr"`, `"rtl"`), verbatim.
    pub direction: String,
    /// W3C TTML2 `tts:writingMode` value (e.g. `"lrtb"`), verbatim.
    pub writing_mode: String,
}

impl Txlo {
    /// §6.10.2.1.2 `flags` bit 0 — when set the `reference_*` / `x` / `y` /
    /// `width` / `height` fields are 32-bit (else 16-bit).
    pub const FLAG_LARGE_FIELD_SIZE: u32 = 0x0000_0001;

    /// True when the 32-bit field-size selector (`flags & 1`) is set.
    pub fn is_large_field_size(&self) -> bool {
        self.flags & Self::FLAG_LARGE_FIELD_SIZE != 0
    }

    /// The decoded `font_size` as a percentage of `reference_height`
    /// (8.8 fixed-point → floating, §6.10.2.1.3).
    pub fn font_size_percent(&self) -> f64 {
        f64::from(self.font_size) / 256.0
    }
}

/// Extended Language descriptive property (`elng`) — HEIF §6.10.2.2.
///
/// Has the same syntax and semantics as the `ExtendedLanguageBox` defined
/// in ISO/IEC 14496-12 but applies to items: it carries the language
/// information of the associated item (§6.10.2.2.1). The wire layout is an
/// `ItemFullProperty('elng', version=0, flags=0)` followed by a single
/// `utf8string extended_language` (an RFC 5646 / BCP 47 language tag).
///
/// Spec: ISO/IEC 23008-12 §6.10.2.2 (carrying ISO/IEC 14496-12 §8.4.6
/// ExtendedLanguageBox semantics).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Elng {
    /// RFC 5646 / BCP 47 language tag for the associated item. An empty
    /// string is the documented "unspecified" shape.
    pub extended_language: String,
}

/// Font Characteristics descriptive property (`fnch`) — HEIF §6.10.4.1.
///
/// Documents parameters for the font-selection algorithm used when
/// rendering a text item (§6.10.4.1.1). It is mandatory (exactly one) for
/// a font item and absent otherwise. The wire layout (§6.10.4.1.2) is an
/// `ItemFullProperty('fnch', version=0, flags=0)` followed by three
/// `utf8string` fields:
///
/// ```text
/// utf8string font_family;   // e.g. "Arial", "Helvetica"
/// utf8string font_style;    // W3C TTML2 tts:fontStyle  (e.g. "normal", "italic")
/// utf8string font_weight;   // W3C TTML2 tts:fontWeight (e.g. "normal", "bold")
/// ```
///
/// `font_style` / `font_weight` are valid W3C TTML2 `tts:fontStyle` /
/// `tts:fontWeight` attribute values; their grammar is deferred to TTML2
/// so they are surfaced verbatim.
///
/// Spec: ISO/IEC 23008-12 §6.10.4.1.2 — ItemFullProperty(`fnch`,
/// version=0, flags=0).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fnch {
    /// Single font-family name (e.g. `"Arial"`, `"Helvetica"`).
    pub font_family: String,
    /// W3C TTML2 `tts:fontStyle` value (e.g. `"normal"`, `"italic"`).
    pub font_style: String,
    /// W3C TTML2 `tts:fontWeight` value (e.g. `"normal"`, `"bold"`).
    pub font_weight: String,
}

/// Camera Extrinsic Matrix descriptive property (`cmex`) — HEIF
/// §6.5.39.
///
/// Describes the spatial setup of the camera(s): a position in the
/// cartesian representation and an orientation of an orthogonal
/// right-handed camera coordinate system within an orthogonal
/// right-handed cartesian 3D world coordinate system (§6.5.39.1). The
/// z-axis of the camera coordinate system faces outward from the lens.
///
/// The `flags` field selects which sub-set of the extrinsic fields is
/// signalled (§6.5.39.1):
///
/// ```text
/// 0x000001  pos_x_present        — position signalled along the x axis
/// 0x000002  pos_y_present        — position signalled along the y axis
/// 0x000004  pos_z_present        — position signalled along the z axis
/// 0x000008  orientation_present  — orientation is signalled
/// 0x000010  rot_large_field_size — quaternion elements are 32-bit
///                                  (else 16-bit)
/// 0x000020  id_present           — world coordinate-system id signalled
/// ```
///
/// The wire layout (§6.5.39.2) is an ItemFullProperty(`cmex`, version,
/// flags) followed by the present fields in order: `pos_x`, `pos_y`,
/// `pos_z` (each `signed int(32)`, µm), then — when
/// `orientation_present` — for `version == 0` a quaternion triplet
/// `quat_x` / `quat_y` / `quat_z` (each a `signed int` of 32 or 16 bits
/// per `rot_large_field_size`), and finally — when `id_present` — an
/// `unsigned int(32) id`.
///
/// A field that is not present is inferred to be `0` (§6.5.39.3) — this
/// type stores each as an [`Option`] so a caller can distinguish
/// "explicitly 0" from "absent / inferred 0".
///
/// For `version == 0` the quaternion is decoded per §6.5.39.1:
///
/// ```text
/// orientationPrecision = (flags & rot_large_field_size) ? 16 : 0
/// qX = quat_x / 2^(14 + orientationPrecision)
/// qY = quat_y / 2^(14 + orientationPrecision)
/// qZ = quat_z / 2^(14 + orientationPrecision)
/// qW = abs(sqrt(1 - (qX^2 + qY^2 + qZ^2)))      // always positive
/// ```
///
/// surfaced via [`Self::orientation_precision`], [`Self::quaternion`]
/// (the four `(qX, qY, qZ, qW)` floats), and [`Self::rotation_matrix`]
/// (the §6.5.39.1 `RC` 3×3 row-major rotation matrix). The µm positions
/// are surfaced verbatim via [`Self::position`].
///
/// `version == 1` signals orientation via a
/// `ViewpointGlobalCoordinateSysRotationStruct` defined in
/// ISO/IEC 23090-7, whose byte layout is **not** in this crate's
/// clean-room allow-list. For `version == 1` with `orientation_present`,
/// the orientation tail (and any subsequent `id`) cannot be parsed; the
/// parser surfaces the positions and rejects the unparseable
/// orientation rather than guessing. See [`parse_cmex`] for the exact
/// handling.
///
/// Per §6.5.39.1 quantity per item is zero or more; instances sharing an
/// `id` indicate items captured in the same world coordinate system, and
/// when more than one `cmex` is associated with an item the `id` shall be
/// present.
///
/// Spec: ISO/IEC 23008-12 §6.5.39 — ItemFullProperty(`cmex`, version,
/// flags).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cmex {
    /// The full 24-bit `flags` field of the FullBox header, preserved
    /// whole so the presence bits and an unknown future flag round-trip.
    /// See the `FLAG_*` constants and the presence accessors.
    pub flags: u32,
    /// FullBox `version` (§6.5.39.2). `0` selects the quaternion
    /// orientation form; `1` selects the ISO/IEC 23090-7
    /// `ViewpointGlobalCoordinateSysRotationStruct` form.
    pub version: u8,
    /// `pos_x` — the x-coordinate of the camera location in µm
    /// (§6.5.39.3). `Some` exactly when `pos_x_present`; absent values
    /// are inferred to be `0`.
    pub pos_x: Option<i32>,
    /// `pos_y` — the y-coordinate of the camera location in µm
    /// (§6.5.39.3). `Some` exactly when `pos_y_present`.
    pub pos_y: Option<i32>,
    /// `pos_z` — the z-coordinate of the camera location in µm
    /// (§6.5.39.3). `Some` exactly when `pos_z_present`.
    pub pos_z: Option<i32>,
    /// `quat_x` — the raw x component of the orientation quaternion
    /// (§6.5.39.3). `Some` exactly when `orientation_present` **and**
    /// `version == 0`; normalise via [`Self::quaternion`].
    pub quat_x: Option<i32>,
    /// `quat_y` — the raw y component of the orientation quaternion
    /// (§6.5.39.3). Present under the same conditions as [`Self::quat_x`].
    pub quat_y: Option<i32>,
    /// `quat_z` — the raw z component of the orientation quaternion
    /// (§6.5.39.3). Present under the same conditions as [`Self::quat_x`].
    pub quat_z: Option<i32>,
    /// `id` — the world coordinate-system identifier (§6.5.39.3). `Some`
    /// exactly when `id_present`; absent values are inferred to be `0`.
    pub id: Option<u32>,
}

impl Cmex {
    /// §6.5.39.1 `pos_x_present` — position signalled along the x axis.
    pub const FLAG_POS_X_PRESENT: u32 = 0x00_0001;
    /// §6.5.39.1 `pos_y_present` — position signalled along the y axis.
    pub const FLAG_POS_Y_PRESENT: u32 = 0x00_0002;
    /// §6.5.39.1 `pos_z_present` — position signalled along the z axis.
    pub const FLAG_POS_Z_PRESENT: u32 = 0x00_0004;
    /// §6.5.39.1 `orientation_present` — orientation is signalled.
    pub const FLAG_ORIENTATION_PRESENT: u32 = 0x00_0008;
    /// §6.5.39.1 `rot_large_field_size` — quaternion elements are 32-bit
    /// integers (otherwise 16-bit integers).
    pub const FLAG_ROT_LARGE_FIELD_SIZE: u32 = 0x00_0010;
    /// §6.5.39.1 `id_present` — the world coordinate-system id is
    /// signalled.
    pub const FLAG_ID_PRESENT: u32 = 0x00_0020;

    /// True when `pos_x_present` (§6.5.39.1).
    pub fn pos_x_present(&self) -> bool {
        self.flags & Self::FLAG_POS_X_PRESENT != 0
    }

    /// True when `pos_y_present` (§6.5.39.1).
    pub fn pos_y_present(&self) -> bool {
        self.flags & Self::FLAG_POS_Y_PRESENT != 0
    }

    /// True when `pos_z_present` (§6.5.39.1).
    pub fn pos_z_present(&self) -> bool {
        self.flags & Self::FLAG_POS_Z_PRESENT != 0
    }

    /// True when `orientation_present` (§6.5.39.1).
    pub fn orientation_present(&self) -> bool {
        self.flags & Self::FLAG_ORIENTATION_PRESENT != 0
    }

    /// True when `rot_large_field_size` (§6.5.39.1) — quaternion elements
    /// are 32-bit rather than 16-bit.
    pub fn rot_large_field_size(&self) -> bool {
        self.flags & Self::FLAG_ROT_LARGE_FIELD_SIZE != 0
    }

    /// True when `id_present` (§6.5.39.1).
    pub fn id_present(&self) -> bool {
        self.flags & Self::FLAG_ID_PRESENT != 0
    }

    /// `orientationPrecision = (flags & rot_large_field_size) ? 16 : 0`
    /// per §6.5.39.1 — the extra bits of fractional quaternion precision
    /// signalled by `rot_large_field_size`.
    pub fn orientation_precision(&self) -> u32 {
        if self.rot_large_field_size() {
            16
        } else {
            0
        }
    }

    /// The camera position vector `(pos_x, pos_y, pos_z)` in µm
    /// (§6.5.39.1 / §6.5.39.3). Each axis not present in `flags` is
    /// inferred to be `0`.
    pub fn position(&self) -> [i32; 3] {
        [
            self.pos_x.unwrap_or(0),
            self.pos_y.unwrap_or(0),
            self.pos_z.unwrap_or(0),
        ]
    }

    /// The orientation unit quaternion `(qX, qY, qZ, qW)` per §6.5.39.1,
    /// computed only for `version == 0`. The three signalled components
    /// are divided by `2^(14 + orientationPrecision)` (a floating-point
    /// division, NOTE 2), and the fourth component is recovered as
    /// `qW = abs(sqrt(1 - (qX^2 + qY^2 + qZ^2)))` (always positive,
    /// NOTE 3). Returns `None` when orientation is absent or
    /// `version != 0` (the `version == 1` form is an
    /// ISO/IEC 23090-7 struct that this crate does not decode).
    pub fn quaternion(&self) -> Option<[f64; 4]> {
        if self.version != 0 {
            return None;
        }
        let (qx, qy, qz) = (self.quat_x?, self.quat_y?, self.quat_z?);
        let scale = f64::from(1u32 << (14 + self.orientation_precision()));
        let qx = f64::from(qx) / scale;
        let qy = f64::from(qy) / scale;
        let qz = f64::from(qz) / scale;
        let qw = (1.0 - (qx * qx + qy * qy + qz * qz)).max(0.0).sqrt().abs();
        Some([qx, qy, qz, qw])
    }

    /// The §6.5.39.1 rotation matrix `RC` (row-major 3×3) built from the
    /// unit quaternion, describing the transformation of the camera
    /// coordinate system relative to the world coordinate system. Returns
    /// `None` under the same conditions as [`Self::quaternion`].
    pub fn rotation_matrix(&self) -> Option<[[f64; 3]; 3]> {
        let [qx, qy, qz, qw] = self.quaternion()?;
        Some([
            [
                1.0 - 2.0 * (qy * qy + qz * qz),
                2.0 * (qx * qy - qz * qw),
                2.0 * (qx * qz + qy * qw),
            ],
            [
                2.0 * (qx * qy + qz * qw),
                1.0 - 2.0 * (qx * qx + qz * qz),
                2.0 * (qy * qz - qx * qw),
            ],
            [
                2.0 * (qx * qz - qy * qw),
                2.0 * (qy * qz + qx * qw),
                1.0 - 2.0 * (qx * qx + qy * qy),
            ],
        ])
    }

    /// The world coordinate-system identifier per §6.5.39.3, inferred to
    /// be `0` when `id_present` is clear.
    pub fn coordinate_system_id(&self) -> u32 {
        self.id.unwrap_or(0)
    }
}

/// Camera Intrinsic Matrix descriptive property (`cmin`) — HEIF
/// §6.5.40.
///
/// Describes the characteristics of the camera that captured the
/// associated image item via a pinhole-camera intrinsic matrix
///
/// ```text
/// | fx  s  cx |
/// |  0 fy  cy |
/// |  0  0   1 |
/// ```
///
/// where `fx` / `fy` are the horizontal / vertical focal lengths, `s`
/// is the skew factor, and `(cx, cy)` is the principal point
/// (§6.5.40.1). Per §6.5.40.1 quantity per item is at most one.
///
/// The wire layout (§6.5.40.2) is an ItemFullProperty(`cmin`,
/// version=0, flags) followed by:
///
/// ```text
/// signed int(32) focal_length_x;
/// signed int(32) principal_point_x;
/// signed int(32) principal_point_y;
/// if (flags & 1) {            // full intrinsics
///     signed int(32) focal_length_y;
///     signed int(32) skew_factor;
/// }
/// ```
///
/// Per §6.5.40.3 `(flags & 1) == 0` signals **simplified** intrinsics
/// (no skew, square pixels: `focal_length_y` and `skew_factor` are
/// absent, `fy` is inferred equal to `fx` and `s` is inferred to be
/// `0`); `(flags & 1) == 1` signals **full** intrinsics with both
/// extra fields present. [`Self::skew`] is `Some` exactly when the
/// full-intrinsics tail is present.
///
/// The on-wire values are integers normalised by the image dimensions
/// and a pair of flag-derived power-of-two denominators (§6.5.40.1):
///
/// ```text
/// denominator     = 1 << ((flags & 0x001F00) >> 8)
/// skewDenominator = 1 << ((flags & 0x1F0000) >> 16)
/// ```
///
/// surfaced verbatim via [`Self::denominator_shift`] /
/// [`Self::skew_denominator_shift`] (the raw shift operands) and
/// [`Self::denominator`] / [`Self::skew_denominator`] (the
/// `1 << shift` values). The §6.5.40.1 focal-length / principal-point
/// formulas (which fold in the `image_width` / `image_height` from the
/// associated [`Ispe`]) are applied by
/// [`Self::focal_length_x_value`] / [`Self::focal_length_y_value`] /
/// [`Self::principal_point_x_value`] / [`Self::principal_point_y_value`]
/// / [`Self::skew_value`], each taking the spatial extents and
/// returning the floating-point matrix entry (a floating-point, not an
/// integer, division per §6.5.40.1 NOTE 3).
///
/// Spec: ISO/IEC 23008-12 §6.5.40.2 — ItemFullProperty(`cmin`,
/// version=0, flags).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cmin {
    /// The full 24-bit `flags` field of the FullBox header. The
    /// low bit selects simplified vs. full intrinsics (§6.5.40.3) and
    /// the two 5-bit shift operands embedded in it select the
    /// `denominator` / `skewDenominator` (§6.5.40.1); preserved whole
    /// so an unknown future flag bit round-trips. See
    /// [`Self::denominator_shift`] / [`Self::skew_denominator_shift`]
    /// for the decoded shift operands and [`Self::has_skew`] for the
    /// low-bit test.
    pub flags: u32,
    /// `focal_length_x` (§6.5.40.3) — the horizontal focal length in
    /// image widths, signed. Normalise via [`Self::focal_length_x_value`].
    pub focal_length_x: i32,
    /// `principal_point_x` (§6.5.40.3) — the principal-point
    /// x-coordinate in image widths, signed. Normalise via
    /// [`Self::principal_point_x_value`].
    pub principal_point_x: i32,
    /// `principal_point_y` (§6.5.40.3) — the principal-point
    /// y-coordinate in image heights, signed. Normalise via
    /// [`Self::principal_point_y_value`].
    pub principal_point_y: i32,
    /// `focal_length_y` (§6.5.40.3) — the vertical focal length in
    /// image heights, signed. Present only in the full-intrinsics form
    /// (`flags & 1`); `None` for the simplified form, in which case
    /// `fy` is inferred equal to `fx` (§6.5.40.3).
    pub focal_length_y: Option<i32>,
    /// `skew_factor` (§6.5.40.3) — the camera-system skew factor,
    /// signed. Present only in the full-intrinsics form (`flags & 1`);
    /// `None` for the simplified form, in which case `s` is inferred to
    /// be `0` (§6.5.40.3).
    pub skew_factor: Option<i32>,
}

impl Cmin {
    /// §6.5.40.3 low-flag bit selecting full (skew + separate vertical
    /// focal length) intrinsics over the simplified form.
    pub const FLAG_FULL_INTRINSICS: u32 = 0x1;

    /// True when the full-intrinsics tail (`focal_length_y` +
    /// `skew_factor`) is present per §6.5.40.3 (`flags & 1 == 1`).
    pub fn has_skew(&self) -> bool {
        self.flags & Self::FLAG_FULL_INTRINSICS != 0
    }

    /// `denominatorShiftOperand = (flags & 0x001F00) >> 8` per
    /// §6.5.40.1 — the power-of-two shift applied to the focal-length /
    /// principal-point fields. Returned as the raw operand; see
    /// [`Self::denominator`] for `1 << operand`.
    pub fn denominator_shift(&self) -> u32 {
        (self.flags & 0x0000_1F00) >> 8
    }

    /// `skewDenominatorShiftOperand = (flags & 0x1F0000) >> 16` per
    /// §6.5.40.1 — the power-of-two shift applied to the skew factor.
    /// Returned as the raw operand; see [`Self::skew_denominator`] for
    /// `1 << operand`.
    pub fn skew_denominator_shift(&self) -> u32 {
        (self.flags & 0x001F_0000) >> 16
    }

    /// `denominator = 1 << denominatorShiftOperand` per §6.5.40.1 —
    /// the common denominator of the focal-length and principal-point
    /// formulas. The operand is a 5-bit field (`0..=31`), so the shift
    /// always fits a `u64`.
    pub fn denominator(&self) -> u64 {
        1u64 << self.denominator_shift()
    }

    /// `skewDenominator = 1 << skewDenominatorShiftOperand` per
    /// §6.5.40.1 — the denominator of the skew formula. The operand is
    /// a 5-bit field (`0..=31`), so the shift always fits a `u64`.
    pub fn skew_denominator(&self) -> u64 {
        1u64 << self.skew_denominator_shift()
    }

    /// `fx = focal_length_x × image_width / denominator` per
    /// §6.5.40.1 (floating-point division, NOTE 3).
    pub fn focal_length_x_value(&self, image_width: u32) -> f64 {
        f64::from(self.focal_length_x) * f64::from(image_width) / self.denominator() as f64
    }

    /// `fy = focal_length_y × image_height / denominator` per
    /// §6.5.40.1. When [`Self::focal_length_y`] is absent (simplified
    /// intrinsics) the value of `fy` is inferred equal to `fx`
    /// (§6.5.40.3), so this falls back to
    /// [`Self::focal_length_x_value`] computed against `image_width`.
    pub fn focal_length_y_value(&self, image_width: u32, image_height: u32) -> f64 {
        match self.focal_length_y {
            Some(fl_y) => f64::from(fl_y) * f64::from(image_height) / self.denominator() as f64,
            None => self.focal_length_x_value(image_width),
        }
    }

    /// `cx = principal_point_x × image_width / denominator` per
    /// §6.5.40.1 (floating-point division, NOTE 3).
    pub fn principal_point_x_value(&self, image_width: u32) -> f64 {
        f64::from(self.principal_point_x) * f64::from(image_width) / self.denominator() as f64
    }

    /// `cy = principal_point_y × image_height / denominator` per
    /// §6.5.40.1 (floating-point division, NOTE 3).
    pub fn principal_point_y_value(&self, image_height: u32) -> f64 {
        f64::from(self.principal_point_y) * f64::from(image_height) / self.denominator() as f64
    }

    /// `s = skew_factor / skewDenominator` per §6.5.40.1. When
    /// [`Self::skew_factor`] is absent (simplified intrinsics) the value
    /// of `s` is inferred to be `0` (§6.5.40.3).
    pub fn skew_value(&self) -> f64 {
        match self.skew_factor {
            Some(skew) => f64::from(skew) / self.skew_denominator() as f64,
            None => 0.0,
        }
    }
}

/// Wipe Transition Effect transformative property (`wipe`) — HEIF
/// §6.5.30.
///
/// Documents a suggested wipe transition effect to apply between the
/// display of two consecutive items of a slideshow entity group: an
/// image that gradually replaces another from one side to another
/// following a moving boundary. The property is associated with the
/// **first** of the two consecutive items involved in the transition
/// (§6.5.30.1), and the precise rendering process is deliberately left
/// to the player.
///
/// Although §6.5.30.1 lists the property type as *transformative*, it
/// does not transform the pixels of the associated image item itself;
/// it is a slideshow-presentation hint. A reader that does not perform
/// slideshow transitions safely ignores it, so a recognised `wipe`
/// never trips [`Meta::unsupported_essential_properties`].
///
/// Spec: ISO/IEC 23008-12 §6.5.30.2 — ItemFullProperty(`wipe`,
/// version=0, flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wipe {
    /// `transition_direction` (§6.5.30.3) — see the direction constants
    /// on [`Wipe`]. Values `>= 8` are reserved; surfaced verbatim.
    pub transition_direction: u8,
}

impl Wipe {
    /// §6.5.30.3 — from the left, following a vertical moving boundary.
    pub const FROM_LEFT: u8 = 0;
    /// §6.5.30.3 — from the right, following a vertical moving boundary.
    pub const FROM_RIGHT: u8 = 1;
    /// §6.5.30.3 — from the top, following a horizontal moving boundary.
    pub const FROM_TOP: u8 = 2;
    /// §6.5.30.3 — from the bottom, following a horizontal moving boundary.
    pub const FROM_BOTTOM: u8 = 3;
    /// §6.5.30.3 — from the left-top corner, following a 45° boundary.
    pub const FROM_LEFT_TOP: u8 = 4;
    /// §6.5.30.3 — from the right-top corner, following a -45° boundary.
    pub const FROM_RIGHT_TOP: u8 = 5;
    /// §6.5.30.3 — from the left-bottom corner, following a -45° boundary.
    pub const FROM_LEFT_BOTTOM: u8 = 6;
    /// §6.5.30.3 — from the right-bottom corner, following a 45° boundary.
    pub const FROM_RIGHT_BOTTOM: u8 = 7;

    /// True when `transition_direction` is one of the eight defined
    /// values (`0..=7`); values `>= 8` are reserved (§6.5.30.3).
    pub fn is_known_direction(&self) -> bool {
        self.transition_direction <= Self::FROM_RIGHT_BOTTOM
    }
}

/// Zoom Transition Effect transformative property (`zoom`) — HEIF
/// §6.5.31.
///
/// Documents a suggested zoom transition effect between two consecutive
/// slideshow items: a new image replaces another by zoom-in or
/// zoom-out from the middle of the preceding image, using a shape
/// (`transition_shape`). Associated with the **first** of the two
/// items (§6.5.31.1); rendering is left to the player.
///
/// The §6.5.31.2 body packs both fields into a single octet:
/// `unsigned int(1) transition_direction` followed by
/// `unsigned int(7) transition_shape`. The direction is the most
/// significant bit; the shape occupies the low 7 bits.
///
/// Like the other transition effects, a reader that does not perform
/// slideshow transitions safely ignores this property, so a recognised
/// `zoom` never trips [`Meta::unsupported_essential_properties`].
///
/// Spec: ISO/IEC 23008-12 §6.5.31.2 — ItemFullProperty(`zoom`,
/// version=0, flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Zoom {
    /// `transition_direction` (§6.5.31.3) — `0` = zoom-in, `1` =
    /// zoom-out. Decoded from the top bit of the §6.5.31.2 octet.
    pub transition_direction: u8,
    /// `transition_shape` (§6.5.31.3) — see the shape constants on
    /// [`Zoom`]. Decoded from the low 7 bits; values not enumerated are
    /// reserved and surfaced verbatim.
    pub transition_shape: u8,
}

impl Zoom {
    /// §6.5.31.3 — zoom-in using the shape defined by `transition_shape`.
    pub const DIRECTION_IN: u8 = 0;
    /// §6.5.31.3 — zoom-out using the shape defined by `transition_shape`.
    pub const DIRECTION_OUT: u8 = 1;
    /// §6.5.31.3 — rectangular transition shape.
    pub const SHAPE_RECTANGULAR: u8 = 0;
    /// §6.5.31.3 — circle transition shape.
    pub const SHAPE_CIRCLE: u8 = 1;
    /// §6.5.31.3 — star transition shape.
    pub const SHAPE_STAR: u8 = 2;

    /// True when `transition_shape` is one of the three defined values
    /// (`0..=2`); other values are reserved (§6.5.31.3).
    pub fn is_known_shape(&self) -> bool {
        self.transition_shape <= Self::SHAPE_STAR
    }
}

/// Fade Transition Effect transformative property (`fade`) — HEIF
/// §6.5.32.
///
/// Documents a suggested fade transition effect between two consecutive
/// slideshow items: an image replaces another either by transitioning
/// through a white or black image, or by mutually fading in and out.
/// Associated with the **first** of the two items (§6.5.32.1);
/// rendering is left to the player.
///
/// A reader that does not perform slideshow transitions safely ignores
/// this property, so a recognised `fade` never trips
/// [`Meta::unsupported_essential_properties`].
///
/// Spec: ISO/IEC 23008-12 §6.5.32.2 — ItemFullProperty(`fade`,
/// version=0, flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fade {
    /// `transition_direction` (§6.5.32.3) — see the constants on
    /// [`Fade`]. Values `>= 3` are reserved; surfaced verbatim.
    pub transition_direction: u8,
}

impl Fade {
    /// §6.5.32.3 — the transition is through a white image.
    pub const THROUGH_WHITE: u8 = 0;
    /// §6.5.32.3 — the transition is through a black image.
    pub const THROUGH_BLACK: u8 = 1;
    /// §6.5.32.3 — the transition is a mutual fading in and out.
    pub const DISSOLVE: u8 = 2;

    /// True when `transition_direction` is one of the three defined
    /// values (`0..=2`); other values are reserved (§6.5.32.3).
    pub fn is_known_direction(&self) -> bool {
        self.transition_direction <= Self::DISSOLVE
    }
}

/// Split Transition Effect transformative property (`splt`) — HEIF
/// §6.5.33.
///
/// Documents a suggested split transition effect between two
/// consecutive slideshow items: one of the two images is split into
/// two equal parts (vertically or horizontally) and the parts move to
/// reveal or cover, depending on `transition_direction`. Associated
/// with the **first** of the two items (§6.5.33.1); rendering is left
/// to the player.
///
/// A reader that does not perform slideshow transitions safely ignores
/// this property, so a recognised `splt` never trips
/// [`Meta::unsupported_essential_properties`].
///
/// Spec: ISO/IEC 23008-12 §6.5.33.2 — ItemFullProperty(`splt`,
/// version=0, flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Splt {
    /// `transition_direction` (§6.5.33.3) — see the constants on
    /// [`Splt`]. Values `>= 4` are reserved; surfaced verbatim.
    pub transition_direction: u8,
}

impl Splt {
    /// §6.5.33.3 — vertical-in: the second image is split vertically
    /// and its parts move inward over the first image.
    pub const VERTICAL_IN: u8 = 0;
    /// §6.5.33.3 — vertical-out: the first image is split vertically
    /// and its parts move outward revealing the second image.
    pub const VERTICAL_OUT: u8 = 1;
    /// §6.5.33.3 — horizontal-in: the second image is split
    /// horizontally and its parts move inward over the first image.
    pub const HORIZONTAL_IN: u8 = 2;
    /// §6.5.33.3 — horizontal-out: the first image is split
    /// horizontally and its parts move outward revealing the second.
    pub const HORIZONTAL_OUT: u8 = 3;

    /// True when `transition_direction` is one of the four defined
    /// values (`0..=3`); other values are reserved (§6.5.33.3).
    pub fn is_known_direction(&self) -> bool {
        self.transition_direction <= Self::HORIZONTAL_OUT
    }
}

/// Suggested Transition Period descriptive property (`stpe`) — HEIF
/// §6.5.34.
///
/// Documents the suggested transition effect duration (from the start
/// to the end of the transition) to apply between the display of two
/// consecutive slideshow items. Associated with the **first** of the
/// two items (§6.5.34.1).
///
/// Descriptive, so a recognised `stpe` never trips
/// [`Meta::unsupported_essential_properties`].
///
/// Spec: ISO/IEC 23008-12 §6.5.34.2 — ItemFullProperty(`stpe`,
/// version=0, flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stpe {
    /// `transition_period` (§6.5.34.3) — the recommended transition
    /// period in units of 1/16 second. Surfaced verbatim; see
    /// [`Self::seconds`] for the converted value.
    pub transition_period: u8,
}

impl Stpe {
    /// The recommended transition period in seconds, applying the
    /// §6.5.34.3 unit (`transition_period / 16`).
    pub fn seconds(&self) -> f64 {
        f64::from(self.transition_period) / 16.0
    }
}

/// Suggested Time Display Duration descriptive property (`ssld`) —
/// HEIF §6.5.35.
///
/// Documents the suggested display duration to apply to an image item
/// (or, when associated with a slideshow entity group, the default for
/// items in that group) of a slideshow (§6.5.35.1). When a transition
/// effect applies at the start or end of an item's display, the
/// duration is the time between the end of the incoming transition and
/// the start of the outgoing one; otherwise it is the full display
/// time (§6.5.35.1).
///
/// Descriptive, so a recognised `ssld` never trips
/// [`Meta::unsupported_essential_properties`].
///
/// Spec: ISO/IEC 23008-12 §6.5.35.2 — ItemFullProperty(`ssld`,
/// version=0, flags=0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ssld {
    /// `duration` (§6.5.35.3) — the recommended display duration in
    /// units of 1/16 second. The value `0` is reserved; surfaced
    /// verbatim. See [`Self::seconds`] for the converted value and
    /// [`Self::is_reserved`] for the reserved-sentinel check.
    pub duration: u16,
}

impl Ssld {
    /// The recommended display duration in seconds, applying the
    /// §6.5.35.3 unit (`duration / 16`).
    pub fn seconds(&self) -> f64 {
        f64::from(self.duration) / 16.0
    }

    /// True when `duration == 0`, the §6.5.35.3 reserved sentinel.
    pub fn is_reserved(&self) -> bool {
        self.duration == 0
    }
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
    /// Ambient viewing environment HDR metadata (AVIF §6.5.36).
    Amve(Amve),
    /// Relative-location property (HEIF §6.5.7).
    Rloc(Rloc),
    /// Layer-selector property (HEIF §6.5.11).
    Lsel(Lsel),
    /// Operating-point selector property (av1-avif §2.3.2.1).
    A1op(A1op),
    /// AV1 layered-image indexing property (av1-avif §2.3.2.3).
    A1lx(A1lx),
    /// Image-scaling transformative property (HEIF §6.5.13).
    Iscl(Iscl),
    /// Required-reference-types descriptive property (HEIF §6.5.17).
    Rref(Rref),
    /// Creation-time descriptive property (HEIF §6.5.18).
    Crtt(Crtt),
    /// Modification-time descriptive property (HEIF §6.5.19).
    Mdft(Mdft),
    /// User-description descriptive property (HEIF §6.5.20).
    Udes(Udes),
    /// Accessibility-text descriptive property (HEIF §6.5.21).
    Altt(Altt),
    /// Auto-exposure-information descriptive property (HEIF §6.5.22).
    Aebr(Aebr),
    /// White-balance-information descriptive property (HEIF §6.5.23).
    Wbbr(Wbbr),
    /// Focus-information descriptive property (HEIF §6.5.24).
    Fobr(Fobr),
    /// Flash-exposure-information descriptive property (HEIF §6.5.25).
    Afbr(Afbr),
    /// Depth-of-field-information descriptive property (HEIF §6.5.26).
    Dobr(Dobr),
    /// Panorama-information descriptive property (HEIF §6.5.27).
    Pano(Pano),
    /// Sub-sample-information descriptive property (HEIF §6.5.28).
    Subs(Subs),
    /// Target-output-layer-set descriptive property (HEIF §6.5.29).
    Tols(Tols),
    /// Wipe-transition-effect transformative property (HEIF §6.5.30).
    Wipe(Wipe),
    /// Zoom-transition-effect transformative property (HEIF §6.5.31).
    Zoom(Zoom),
    /// Fade-transition-effect transformative property (HEIF §6.5.32).
    Fade(Fade),
    /// Split-transition-effect transformative property (HEIF §6.5.33).
    Splt(Splt),
    /// Suggested-transition-period descriptive property (HEIF §6.5.34).
    Stpe(Stpe),
    /// Suggested-time-display-duration descriptive property (HEIF §6.5.35).
    Ssld(Ssld),
    /// Progressive-derived-image-item-information descriptive property
    /// (HEIF §6.5.37).
    Prdi(Prdi),
    /// `sstr` — Single Stream descriptive property (HEIF §6.5.38).
    Sstr(Sstr),
    /// `txlo` — Text Layout descriptive property (HEIF §6.10.2.1).
    Txlo(Txlo),
    /// `elng` — Extended Language descriptive property (HEIF §6.10.2.2).
    Elng(Elng),
    /// `fnch` — Font Characteristics descriptive property (HEIF §6.10.4.1).
    Fnch(Fnch),
    /// Camera-extrinsic-matrix descriptive property (HEIF §6.5.39).
    Cmex(Cmex),
    /// Camera-intrinsic-matrix descriptive property (HEIF §6.5.40).
    Cmin(Cmin),
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
            Property::Amve(_) => AMVE,
            Property::Rloc(_) => RLOC,
            Property::Lsel(_) => LSEL,
            Property::A1op(_) => A1OP,
            Property::A1lx(_) => A1LX,
            Property::Iscl(_) => ISCL,
            Property::Rref(_) => RREF,
            Property::Crtt(_) => CRTT,
            Property::Mdft(_) => MDFT,
            Property::Udes(_) => UDES,
            Property::Altt(_) => ALTT,
            Property::Aebr(_) => AEBR,
            Property::Wbbr(_) => WBBR,
            Property::Fobr(_) => FOBR,
            Property::Afbr(_) => AFBR,
            Property::Dobr(_) => DOBR,
            Property::Pano(_) => PANO,
            Property::Subs(_) => SUBS,
            Property::Tols(_) => TOLS,
            Property::Wipe(_) => WIPE,
            Property::Zoom(_) => ZOOM,
            Property::Fade(_) => FADE,
            Property::Splt(_) => SPLT,
            Property::Stpe(_) => STPE,
            Property::Ssld(_) => SSLD,
            Property::Prdi(_) => PRDI,
            Property::Sstr(_) => SSTR,
            Property::Txlo(_) => TXLO,
            Property::Elng(_) => ELNG,
            Property::Fnch(_) => FNCH,
            Property::Cmex(_) => CMEX,
            Property::Cmin(_) => CMIN,
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
    /// `irot`, `imir`, `lsel`, `a1op`, `iscl`, `rref`, `crtt`, `mdft`,
    /// `udes`, `altt`, `aebr`, `wbbr`, `fobr`, `afbr`, `dobr`,
    /// `pano`, `subs`, `tols`, etc.
    /// that we parse counts as recognised regardless of the essential
    /// bit. `tols` (§6.5.29) is the one descriptive property the spec
    /// *requires* to be essential (`essential shall be equal to 1`), so
    /// surfacing its typed value is what keeps a `tols`-carrying item
    /// off this list.
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
    let (version, flags, body) = parse_full_box(payload)?;
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
        flags,
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
            // v1/v2: optional extent_index before offset/length. The spec
            // (§8.11.3.3) only assigns it meaning for construction_method
            // 2 (item_offset), where it is the 1-based index of the
            // 'iloc' item reference naming the data-origin item; we
            // capture it unconditionally so the cm=2 resolver can use it.
            let extent_index = if (version == 1 || version == 2) && index_size > 0 {
                let v = read_var_uint(body, cursor, index_size)?;
                cursor += index_size;
                v
            } else {
                0
            };
            let offset = read_var_uint(body, cursor, offset_size)?;
            cursor += offset_size;
            let length = read_var_uint(body, cursor, length_size)?;
            cursor += length_size;
            extents.push(IlocExtent {
                offset,
                length,
                extent_index,
            });
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
            x if x == &AMVE => Property::Amve(parse_amve(body)?),
            x if x == &RLOC => Property::Rloc(parse_rloc(body)?),
            x if x == &LSEL => Property::Lsel(parse_lsel(body)?),
            x if x == &A1OP => Property::A1op(parse_a1op(body)?),
            x if x == &A1LX => Property::A1lx(parse_a1lx(body)?),
            x if x == &ISCL => Property::Iscl(parse_iscl(body)?),
            x if x == &RREF => Property::Rref(parse_rref(body)?),
            x if x == &CRTT => Property::Crtt(parse_crtt(body)?),
            x if x == &MDFT => Property::Mdft(parse_mdft(body)?),
            x if x == &UDES => Property::Udes(parse_udes(body)?),
            x if x == &ALTT => Property::Altt(parse_altt(body)?),
            x if x == &AEBR => Property::Aebr(parse_aebr(body)?),
            x if x == &WBBR => Property::Wbbr(parse_wbbr(body)?),
            x if x == &FOBR => Property::Fobr(parse_fobr(body)?),
            x if x == &AFBR => Property::Afbr(parse_afbr(body)?),
            x if x == &DOBR => Property::Dobr(parse_dobr(body)?),
            x if x == &PANO => Property::Pano(parse_pano(body)?),
            x if x == &SUBS => Property::Subs(parse_subs(body)?),
            x if x == &TOLS => Property::Tols(parse_tols(body)?),
            x if x == &WIPE => Property::Wipe(parse_wipe(body)?),
            x if x == &ZOOM => Property::Zoom(parse_zoom(body)?),
            x if x == &FADE => Property::Fade(parse_fade(body)?),
            x if x == &SPLT => Property::Splt(parse_splt(body)?),
            x if x == &STPE => Property::Stpe(parse_stpe(body)?),
            x if x == &SSLD => Property::Ssld(parse_ssld(body)?),
            x if x == &PRDI => Property::Prdi(parse_prdi(body)?),
            x if x == &SSTR => Property::Sstr(parse_sstr(body)?),
            x if x == &TXLO => Property::Txlo(parse_txlo(body)?),
            x if x == &ELNG => Property::Elng(parse_elng(body)?),
            x if x == &FNCH => Property::Fnch(parse_fnch(body)?),
            x if x == &CMEX => Property::Cmex(parse_cmex(body)?),
            x if x == &CMIN => Property::Cmin(parse_cmin(body)?),
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

/// Parse `amve` (AmbientViewingEnvironmentBox — AVIF §6.5.36). A plain
/// `Box` (no version/flags) with a fixed 8-byte body: a big-endian
/// `unsigned int(32)` illuminance followed by two big-endian
/// `unsigned int(16)` CIE 1931 chromaticity values.
fn parse_amve(body: &[u8]) -> Result<Amve> {
    if body.len() < 8 {
        return Err(Error::invalid(format!(
            "avif: amve too short ({} < 8)",
            body.len()
        )));
    }
    Ok(Amve {
        ambient_illuminance: read_u32(body, 0)?,
        ambient_light_x: read_u16(body, 4)?,
        ambient_light_y: read_u16(body, 6)?,
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

/// Parse `iscl` (ImageScaling — HEIF §6.5.13). FullBox(`iscl`,
/// version=0, flags=0) followed by four big-endian
/// `unsigned int(16)` fields totalling 8 bytes:
///
/// ```text
/// unsigned int(16) target_width_numerator;
/// unsigned int(16) target_width_denominator;
/// unsigned int(16) target_height_numerator;
/// unsigned int(16) target_height_denominator;
/// ```
///
/// The §6.5.13.3 `shall` that every numerator and denominator be
/// non-zero is not enforced at parse time — the parser surfaces
/// the bytes as written and the caller routes to
/// [`Iscl::is_well_formed`] for the §6.5.13.3 check. This keeps the
/// "did the bytes decode" and "did they satisfy the normative
/// constraint" signals separate, matching the pattern used by the
/// other HEIF property parsers in this module.
///
/// An unknown `version` is rejected so a future v1 layout never
/// gets misread as v0.
fn parse_iscl(body: &[u8]) -> Result<Iscl> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: iscl version {version} != 0")));
    }
    if rest.len() < 8 {
        return Err(Error::invalid(format!(
            "avif: iscl too short ({} < 8)",
            rest.len()
        )));
    }
    Ok(Iscl {
        target_width_numerator: read_u16(rest, 0)?,
        target_width_denominator: read_u16(rest, 2)?,
        target_height_numerator: read_u16(rest, 4)?,
        target_height_denominator: read_u16(rest, 6)?,
    })
}

/// Parse `rref` (RequiredReferenceTypesProperty — HEIF §6.5.17).
/// FullBox(`rref`, version=0, flags=0) followed by:
///
/// ```text
/// unsigned int(8) reference_type_count;
/// for (i=0; i< reference_type_count; i++) {
///     unsigned int(32) reference_type[i];
/// }
/// ```
///
/// A declared `reference_type_count` that exceeds the available
/// body bytes returns an error rather than silently truncating —
/// per §6.5.17 a reader that fails to honour every listed type
/// `shall` refuse to process the associated item, so a partial
/// read would defeat the property's purpose.
///
/// An unknown `version` is rejected so a future-version layout
/// can't be misread.
fn parse_rref(body: &[u8]) -> Result<Rref> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: rref version {version} != 0")));
    }
    if rest.is_empty() {
        return Err(Error::invalid("avif: rref too short (0 < 1)"));
    }
    let count = rest[0] as usize;
    let need = 1 + count * 4;
    if rest.len() < need {
        return Err(Error::invalid(format!(
            "avif: rref reference_type table truncated ({} < {need})",
            rest.len()
        )));
    }
    let mut reference_types = Vec::with_capacity(count);
    for i in 0..count {
        let at = 1 + i * 4;
        let mut t = [0u8; 4];
        t.copy_from_slice(&rest[at..at + 4]);
        reference_types.push(t);
    }
    Ok(Rref { reference_types })
}

/// Parse `crtt` (CreationTimeProperty — HEIF §6.5.18). FullBox(`crtt`,
/// version=0, flags=0) followed by a single big-endian
/// `unsigned int(64)` field totalling 8 bytes:
///
/// ```text
/// unsigned int(64) creation_time;
/// ```
///
/// `creation_time` is in microseconds since midnight, Jan. 1, 1904 UTC
/// per §6.5.18.3 — the parser surfaces the value as written; the
/// [`Crtt::seconds_since_unix_epoch`] / [`Crtt::subsecond_micros`]
/// helpers convert to the Unix epoch when a caller wants a directly
/// comparable timestamp.
///
/// An unknown `version` is rejected so a future-version layout cannot
/// be misread as v0.
fn parse_crtt(body: &[u8]) -> Result<Crtt> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: crtt version {version} != 0")));
    }
    if rest.len() < 8 {
        return Err(Error::invalid(format!(
            "avif: crtt too short ({} < 8)",
            rest.len()
        )));
    }
    Ok(Crtt {
        creation_time: read_u64(rest, 0)?,
    })
}

/// Parse `mdft` (ModificationTimeProperty — HEIF §6.5.19).
/// FullBox(`mdft`, version=0, flags=0) followed by a single
/// big-endian `unsigned int(64)` field totalling 8 bytes:
///
/// ```text
/// unsigned int(64) modification_time;
/// ```
///
/// `modification_time` is in microseconds since midnight, Jan. 1, 1904
/// UTC per §6.5.19.3 — the parser surfaces the value as written; the
/// [`Mdft::seconds_since_unix_epoch`] / [`Mdft::subsecond_micros`]
/// helpers convert to the Unix epoch when a caller wants a directly
/// comparable timestamp.
///
/// The wire layout mirrors §6.5.18 `crtt` exactly (same FullBox header,
/// same u64 field width, same 1904-epoch microsecond unit), so the
/// parser is structurally identical — only the box four-CC and the
/// surfaced struct differ.
///
/// An unknown `version` is rejected so a future-version layout cannot
/// be misread as v0.
fn parse_mdft(body: &[u8]) -> Result<Mdft> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: mdft version {version} != 0")));
    }
    if rest.len() < 8 {
        return Err(Error::invalid(format!(
            "avif: mdft too short ({} < 8)",
            rest.len()
        )));
    }
    Ok(Mdft {
        modification_time: read_u64(rest, 0)?,
    })
}

/// Parse `udes` (UserDescriptionProperty — HEIF §6.5.20).
/// FullBox(`udes`, version=0, flags=0) followed by four
/// sequential null-terminated UTF-8 strings:
///
/// ```text
/// utf8string lang;
/// utf8string name;
/// utf8string description;
/// utf8string tags;
/// ```
///
/// Per §6.5.20.3 each field's empty-string form (a single nul byte)
/// is the documented "absent" sentinel; the parser preserves the raw
/// string and leaves the `Option` projection to the
/// [`Udes::lang_opt`] / [`Udes::name_opt`] / [`Udes::description_opt`]
/// / [`Udes::tags_opt`] / [`Udes::tag_list`] helpers.
///
/// An unknown `version` is rejected so a future-version layout (which
/// might re-shape the field order or widths) cannot be misread as v0.
/// A body that runs out before all four strings have been read is
/// rejected by [`read_cstr`]; trailing bytes past the fourth
/// terminator are ignored, mirroring the §8.11.6 `infe` tail-field
/// behaviour for forward compatibility with future spec revisions
/// that append new fields under the same `version=0` slot.
fn parse_udes(body: &[u8]) -> Result<Udes> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: udes version {version} != 0")));
    }
    let (lang, after_lang) = read_cstr(rest, 0)?;
    let (name, after_name) = read_cstr(rest, after_lang)?;
    let (description, after_desc) = read_cstr(rest, after_name)?;
    let (tags, _after_tags) = read_cstr(rest, after_desc)?;
    Ok(Udes {
        lang,
        name,
        description,
        tags,
    })
}

/// Parse `altt` (AccessibilityTextProperty — HEIF §6.5.21).
/// FullBox(`altt`, version=0, flags=0) followed by two sequential
/// null-terminated UTF-8 strings:
///
/// ```text
/// utf8string alt_text;
/// utf8string alt_lang;
/// ```
///
/// Per §6.5.21.3 an empty `alt_lang` flags the language as
/// unknown/undefined; the parser preserves the raw empty string and
/// the [`Altt::alt_lang_opt`] / [`Altt::alt_text_opt`] helpers project
/// the empty form to `None`. The parsed field order is
/// `alt_text`-first to mirror the §6.5.21.2 syntax verbatim — this
/// reverses the field ordering relative to `udes`, where the language
/// tag comes first.
///
/// An unknown `version` is rejected so a future-version layout cannot
/// be misread as v0. A body that runs out before both strings have
/// been read is rejected by [`read_cstr`]. Trailing bytes past the
/// second terminator are ignored, mirroring the §8.11.6 `infe`
/// tail-field behaviour for forward compatibility with future spec
/// revisions that append new fields under the same `version=0` slot.
fn parse_altt(body: &[u8]) -> Result<Altt> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: altt version {version} != 0")));
    }
    let (alt_text, after_text) = read_cstr(rest, 0)?;
    let (alt_lang, _after_lang) = read_cstr(rest, after_text)?;
    Ok(Altt { alt_text, alt_lang })
}

/// Parse `aebr` (AutoExposureProperty — HEIF §6.5.22). FullBox(`aebr`,
/// version=0, flags=0) followed by two `int(8)` fields:
///
/// ```text
/// int(8) exposure_step;
/// int(8) exposure_numerator;
/// ```
///
/// The §6.5.22.3 enumeration for `exposure_step` (`1`/`2`/`3`/`4`)
/// is not enforced at parse time — the parser surfaces the raw value
/// and the [`Aebr::is_defined_step`] / [`Aebr::exposure_stops`]
/// helpers expose the semantic checks separately. This keeps "did the
/// bytes decode" and "did they satisfy the spec's enumeration" as
/// distinct signals, matching the pattern used by the other HEIF
/// property parsers in this module (notably `iscl` which factors out
/// §6.5.13.3 `is_well_formed`).
///
/// An unknown `version` is rejected so a future-version layout cannot
/// be misread as v0. Trailing bytes past the two fields are ignored,
/// mirroring the forward-compatibility behaviour of the other
/// FullBox-headed property parsers — a v0 producer that pads the box
/// with reserved bytes is read cleanly.
fn parse_aebr(body: &[u8]) -> Result<Aebr> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: aebr version {version} != 0")));
    }
    if rest.len() < 2 {
        return Err(Error::invalid(format!(
            "avif: aebr too short ({} < 2)",
            rest.len()
        )));
    }
    Ok(Aebr {
        exposure_step: rest[0] as i8,
        exposure_numerator: rest[1] as i8,
    })
}

/// Parse `wbbr` (WhiteBalanceProperty — HEIF §6.5.23).
/// FullBox(`wbbr`, version=0, flags=0) followed by:
///
/// ```text
/// unsigned int(16) blue_amber;
/// int(8)           green_magenta;
/// ```
///
/// per §6.5.23.2. `blue_amber` is the colour-temperature component
/// in Kelvin (so a 16-bit unsigned range is comfortable for every
/// practical photographic temperature). `green_magenta` is the
/// colour-deviation component in 1/100 Duv (signed: negative =
/// magenta shift, positive = green shift per the §6.5.23.3 NOTE).
///
/// An unknown `version` is rejected so a future-version layout
/// cannot be misread as v0. Trailing bytes past the three fields
/// are ignored, mirroring the forward-compatibility behaviour of
/// the other FullBox-headed property parsers in this module — a v0
/// producer that pads the box with reserved bytes is read cleanly.
///
/// The §6.5.23.3 NOTE sentinel (`green_magenta == 0` = neutral
/// light source) is not enforced at parse time — the
/// [`Wbbr::is_green_magenta_neutral`] predicate exposes the check
/// separately, mirroring how `aebr`'s §6.5.22.3 enumeration is
/// surfaced via [`Aebr::is_defined_step`].
fn parse_wbbr(body: &[u8]) -> Result<Wbbr> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: wbbr version {version} != 0")));
    }
    if rest.len() < 3 {
        return Err(Error::invalid(format!(
            "avif: wbbr too short ({} < 3)",
            rest.len()
        )));
    }
    Ok(Wbbr {
        blue_amber: read_u16(rest, 0)?,
        green_magenta: rest[2] as i8,
    })
}

/// Parse `fobr` (FocusProperty — HEIF §6.5.24).
/// FullBox(`fobr`, version=0, flags=0) followed by:
///
/// ```text
/// unsigned int(16) focus_distance_numerator;
/// unsigned int(16) focus_distance_denominator;
/// ```
///
/// per §6.5.24.2. The focus distance is expressed in metres as
/// `focus_distance_numerator / focus_distance_denominator`
/// (§6.5.24.3). Both fields are big-endian unsigned per ISO/IEC
/// 14496-12 §4.2. A denominator of zero is the §6.5.24.3 infinity
/// sentinel and the numerator `should` also be zero in that case;
/// neither field is validated against that sentinel here so a
/// well-formed but unusual reading (denominator-only zero) survives
/// to the typed value where [`Fobr::has_well_formed_infinity_sentinel`]
/// can distinguish it.
///
/// An unknown `version` is rejected so a future-version layout
/// cannot be misread as v0. Trailing bytes past the four fixed
/// bytes are ignored, mirroring the forward-compatibility behaviour
/// of the other FullBox-headed property parsers in this module — a
/// v0 producer that pads the box with reserved bytes is read cleanly.
fn parse_fobr(body: &[u8]) -> Result<Fobr> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: fobr version {version} != 0")));
    }
    if rest.len() < 4 {
        return Err(Error::invalid(format!(
            "avif: fobr too short ({} < 4)",
            rest.len()
        )));
    }
    Ok(Fobr {
        focus_distance_numerator: read_u16(rest, 0)?,
        focus_distance_denominator: read_u16(rest, 2)?,
    })
}

/// Parse `afbr` (FlashExposureProperty — HEIF §6.5.25).
/// FullBox(`afbr`, version=0, flags=0) followed by:
///
/// ```text
/// int(8) flash_exposure_numerator;
/// int(8) flash_exposure_denominator;
/// ```
///
/// per §6.5.25.2. The flash exposure value of the sample is expressed
/// in number of f-stops as `flash_exposure_numerator /
/// flash_exposure_denominator` per §6.5.25.3. Both fields are signed
/// per the spec text; the bytes are reinterpreted as `i8` so a writer
/// that produces `-1` (`0xFF`) for the smallest dark direction
/// round-trips to `-1`, not `255`.
///
/// An unknown `version` is rejected so a future-version layout cannot
/// be misread as v0. Trailing bytes past the two fixed bytes are
/// ignored, mirroring the forward-compatibility behaviour of the
/// other FullBox-headed property parsers in this module — a v0
/// producer that pads the box with reserved bytes is read cleanly.
fn parse_afbr(body: &[u8]) -> Result<Afbr> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: afbr version {version} != 0")));
    }
    if rest.len() < 2 {
        return Err(Error::invalid(format!(
            "avif: afbr too short ({} < 2)",
            rest.len()
        )));
    }
    Ok(Afbr {
        flash_exposure_numerator: rest[0] as i8,
        flash_exposure_denominator: rest[1] as i8,
    })
}

/// Parse `dobr` (DepthOfFieldProperty — HEIF §6.5.26).
/// FullBox(`dobr`, version=0, flags=0) followed by:
///
/// ```text
/// int(8) f_stop_numerator;
/// int(8) f_stop_denominator;
/// ```
///
/// per §6.5.26.2. The depth-of-field variation is expressed as an
/// aperture change in a number of stops, computed as `f_stop_numerator
/// / f_stop_denominator` per §6.5.26.3. Both fields are signed per the
/// spec text; the bytes are reinterpreted as `i8` so a writer that
/// produces `-1` (`0xFF`) for the shallow direction round-trips to
/// `-1`, not `255`.
///
/// An unknown `version` is rejected so a future-version layout cannot
/// be misread as v0. Trailing bytes past the two fixed bytes are
/// ignored, mirroring the forward-compatibility behaviour of the
/// other FullBox-headed property parsers in this module — a v0
/// producer that pads the box with reserved bytes is read cleanly.
fn parse_dobr(body: &[u8]) -> Result<Dobr> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: dobr version {version} != 0")));
    }
    if rest.len() < 2 {
        return Err(Error::invalid(format!(
            "avif: dobr too short ({} < 2)",
            rest.len()
        )));
    }
    Ok(Dobr {
        f_stop_numerator: rest[0] as i8,
        f_stop_denominator: rest[1] as i8,
    })
}

/// Parse `pano` (PanoramaProperty — HEIF §6.5.27).
/// FullBox(`pano`, version=0, flags=0) followed by:
///
/// ```text
/// unsigned int(8) panorama_direction;
/// if (panorama_direction >= 4 && panorama_direction <= 5) { // grid
///     unsigned int(8) rows_minus_one;
///     unsigned int(8) columns_minus_one;
/// }
/// ```
///
/// per §6.5.27.2. The two grid-shape bytes are conditionally present —
/// the syntax guards them behind the two grid direction values, so a
/// linear-direction body is one byte long and a grid-direction body is
/// three bytes long. A grid direction whose body is missing the shape
/// bytes is rejected (truncated); a linear or undefined direction
/// ignores any trailing bytes, mirroring the forward-compatibility
/// behaviour of the other FullBox-headed property parsers in this
/// module.
///
/// An undefined `panorama_direction` (`>= 6`, §6.5.27.3 "other values
/// are undefined") is NOT a parse error — the raw value is preserved
/// so a reader can skip the panorama reconstruction without losing the
/// rest of the file. An unknown `version` is rejected so a
/// future-version layout cannot be misread as v0.
fn parse_pano(body: &[u8]) -> Result<Pano> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: pano version {version} != 0")));
    }
    if rest.is_empty() {
        return Err(Error::invalid("avif: pano too short (0 < 1)"));
    }
    let panorama_direction = rest[0];
    let grid = if (Pano::DIRECTION_GRID_RASTER..=Pano::DIRECTION_GRID_CONTINUOUS)
        .contains(&panorama_direction)
    {
        if rest.len() < 3 {
            return Err(Error::invalid(format!(
                "avif: pano grid direction {panorama_direction} but body too short ({} < 3)",
                rest.len()
            )));
        }
        Some(PanoGrid {
            rows_minus_one: rest[1],
            columns_minus_one: rest[2],
        })
    } else {
        None
    };
    Ok(Pano {
        panorama_direction,
        grid,
    })
}

/// Parse `subs` (SubSampleInformationBox as item property — HEIF §6.5.28,
/// ISO/IEC 14496-12 §8.7.7.2).
///
/// `FullBox('subs', version, flags)` followed by `entry_count` and, per
/// entry, `sample_delta`, `subsample_count`, and that many sub-sample
/// tuples. When used as an item property HEIF §6.5.28 fixes
/// `entry_count == 1` and the single entry's `sample_delta == 0`; both
/// are enforced. `version` selects the `subsample_size` width: 32-bit for
/// v1, 16-bit for v0; any other version is rejected so a future layout is
/// not misread. Trailing bytes past the declared sub-samples are ignored
/// for forward compatibility.
fn parse_subs(body: &[u8]) -> Result<Subs> {
    let (version, flags, rest) = parse_full_box(body)?;
    if version != 0 && version != 1 {
        return Err(Error::invalid(format!(
            "avif: subs version {version} unsupported (expected 0 or 1)"
        )));
    }
    let entry_count = read_u32(rest, 0)?;
    if entry_count != 1 {
        return Err(Error::invalid(format!(
            "avif: subs entry_count {entry_count} != 1 (HEIF §6.5.28)"
        )));
    }
    let sample_delta = read_u32(rest, 4)?;
    if sample_delta != 0 {
        return Err(Error::invalid(format!(
            "avif: subs sample_delta {sample_delta} != 0 (HEIF §6.5.28)"
        )));
    }
    let subsample_count = read_u16(rest, 8)?;
    let mut entries = Vec::with_capacity(subsample_count as usize);
    let mut off = 10usize;
    for _ in 0..subsample_count {
        let subsample_size = if version == 1 {
            let v = read_u32(rest, off)?;
            off += 4;
            v
        } else {
            let v = read_u16(rest, off)? as u32;
            off += 2;
            v
        };
        let priority_off = off
            .checked_add(2)
            .ok_or_else(|| Error::invalid("avif: subs entry offset overflow"))?;
        if priority_off > rest.len() {
            return Err(Error::invalid("avif: subs truncated sub-sample entry"));
        }
        let subsample_priority = rest[off];
        let discardable = rest[off + 1];
        off += 2;
        let codec_specific_parameters = read_u32(rest, off)?;
        off += 4;
        entries.push(SubsEntry {
            subsample_size,
            subsample_priority,
            discardable,
            codec_specific_parameters,
        });
    }
    Ok(Subs { flags, entries })
}

/// Parse `tols` (TargetOlsProperty — HEIF §6.5.29).
/// ItemFullProperty(`tols`, version=0, flags=0) followed by a single
/// big-endian `unsigned int(16) target_ols_idx` (§6.5.29.2).
///
/// An unknown `version` is rejected so a future-version layout (which
/// might re-shape the field width or add fields) cannot be misread as
/// v0; a body shorter than the two-byte field is rejected; trailing
/// bytes past the field are ignored for forward compatibility, matching
/// every other FullBox-headed property parser in this module.
fn parse_tols(body: &[u8]) -> Result<Tols> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: tols version {version} != 0")));
    }
    if rest.len() < 2 {
        return Err(Error::invalid(format!(
            "avif: tols too short ({} < 2)",
            rest.len()
        )));
    }
    Ok(Tols {
        target_ols_idx: read_u16(rest, 0)?,
    })
}

/// Parse `wipe` (WipeTransitionEffectProperty — HEIF §6.5.30).
/// FullBox(`wipe`, version=0, flags=0) followed by a single
/// `unsigned int(8) transition_direction` (§6.5.30.2). An unknown
/// `version` is rejected so a future-version layout cannot be misread
/// as v0; trailing bytes are ignored for forward compatibility.
fn parse_wipe(body: &[u8]) -> Result<Wipe> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: wipe version {version} != 0")));
    }
    if rest.is_empty() {
        return Err(Error::invalid("avif: wipe too short (0 < 1)"));
    }
    Ok(Wipe {
        transition_direction: rest[0],
    })
}

/// Parse `zoom` (ZoomTransitionEffectProperty — HEIF §6.5.31).
/// FullBox(`zoom`, version=0, flags=0) followed by a single octet that
/// packs `unsigned int(1) transition_direction` (top bit) and
/// `unsigned int(7) transition_shape` (low 7 bits) per §6.5.31.2. An
/// unknown `version` is rejected; trailing bytes are ignored for
/// forward compatibility.
fn parse_zoom(body: &[u8]) -> Result<Zoom> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: zoom version {version} != 0")));
    }
    if rest.is_empty() {
        return Err(Error::invalid("avif: zoom too short (0 < 1)"));
    }
    let packed = rest[0];
    Ok(Zoom {
        transition_direction: packed >> 7,
        transition_shape: packed & 0x7f,
    })
}

/// Parse `fade` (FadeTransitionEffectProperty — HEIF §6.5.32).
/// FullBox(`fade`, version=0, flags=0) followed by a single
/// `unsigned int(8) transition_direction` (§6.5.32.2). An unknown
/// `version` is rejected; trailing bytes are ignored for forward
/// compatibility.
fn parse_fade(body: &[u8]) -> Result<Fade> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: fade version {version} != 0")));
    }
    if rest.is_empty() {
        return Err(Error::invalid("avif: fade too short (0 < 1)"));
    }
    Ok(Fade {
        transition_direction: rest[0],
    })
}

/// Parse `splt` (SplitTransitionEffectProperty — HEIF §6.5.33).
/// FullBox(`splt`, version=0, flags=0) followed by a single
/// `unsigned int(8) transition_direction` (§6.5.33.2). An unknown
/// `version` is rejected; trailing bytes are ignored for forward
/// compatibility.
fn parse_splt(body: &[u8]) -> Result<Splt> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: splt version {version} != 0")));
    }
    if rest.is_empty() {
        return Err(Error::invalid("avif: splt too short (0 < 1)"));
    }
    Ok(Splt {
        transition_direction: rest[0],
    })
}

/// Parse `stpe` (SuggestedTransitionPeriodProperty — HEIF §6.5.34).
/// FullBox(`stpe`, version=0, flags=0) followed by a single
/// `unsigned int(8) transition_period` in units of 1/16 s (§6.5.34.2).
/// An unknown `version` is rejected; trailing bytes are ignored for
/// forward compatibility.
fn parse_stpe(body: &[u8]) -> Result<Stpe> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: stpe version {version} != 0")));
    }
    if rest.is_empty() {
        return Err(Error::invalid("avif: stpe too short (0 < 1)"));
    }
    Ok(Stpe {
        transition_period: rest[0],
    })
}

/// Parse `ssld` (SuggestedTimeDisplayDurationProperty — HEIF §6.5.35).
/// FullBox(`ssld`, version=0, flags=0) followed by a single
/// `unsigned int(16) duration` in units of 1/16 s (§6.5.35.2). An
/// unknown `version` is rejected; a truncated `duration` is rejected;
/// trailing bytes are ignored for forward compatibility.
fn parse_ssld(body: &[u8]) -> Result<Ssld> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: ssld version {version} != 0")));
    }
    if rest.len() < 2 {
        return Err(Error::invalid(format!(
            "avif: ssld too short ({} < 2)",
            rest.len()
        )));
    }
    Ok(Ssld {
        duration: read_u16(rest, 0)?,
    })
}

/// Parse `prdi` (ProgressiveDerivedImageItemInformationProperty — HEIF
/// §6.5.37).
///
/// `ItemFullProperty('prdi', version=0, flags)` whose body is entirely
/// `flags`-gated (§6.5.37.2):
///
/// ```text
/// if ((flags & one_item_per_step) == 0 ||
///     (flags & alternative_is_candidate))
///    unsigned int(16) step_count;
/// if ((flags & one_item_per_step) == 0) {
///    for (i = 0; i < step_count; i++)
///       unsigned int(16) item_count;
/// }
/// ```
///
/// `step_count` is present iff `one_item_per_step` is clear or
/// `alternative_is_candidate` is set; the `item_count[]` array is present
/// iff `one_item_per_step` is clear (in which case `step_count` is also
/// present, so the array length is well-defined). When neither field is
/// present (`one_item_per_step` set, `alternative_is_candidate` clear) the
/// body is empty and both values are inferred per §6.5.37.3.
///
/// An unknown `version` is rejected so a future-version layout cannot be
/// misread as v0; a truncated `step_count` or `item_count` array is
/// rejected; trailing bytes past the declared array are ignored for
/// forward compatibility, matching every other FullBox-headed property
/// parser in this module.
fn parse_prdi(body: &[u8]) -> Result<Prdi> {
    let (version, flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: prdi version {version} != 0")));
    }
    let one_item_per_step = flags & Prdi::FLAG_ONE_ITEM_PER_STEP != 0;
    let alternative_is_candidate = flags & Prdi::FLAG_ALTERNATIVE_IS_CANDIDATE != 0;

    let mut off = 0usize;
    let step_count = if !one_item_per_step || alternative_is_candidate {
        let v = read_u16(rest, off)?;
        off += 2;
        Some(v)
    } else {
        None
    };

    let item_counts = if !one_item_per_step {
        // `step_count` is guaranteed present here (the condition that
        // gates `step_count` is a superset of `!one_item_per_step`).
        let n = step_count.expect("prdi: step_count present when one_item_per_step is clear");
        let mut counts = Vec::with_capacity(n as usize);
        for _ in 0..n {
            counts.push(read_u16(rest, off)?);
            off += 2;
        }
        Some(counts)
    } else {
        None
    };

    Ok(Prdi {
        flags,
        step_count,
        item_counts,
    })
}

/// Parse `sstr` (SingleStreamProperty — HEIF §6.5.38). The box is the
/// bare `ItemProperty('sstr')` with no payload (§6.5.38.2), so its mere
/// presence is the whole semantic. Any trailing bytes are tolerated for
/// forward compatibility (a future revision could add fields).
fn parse_sstr(_body: &[u8]) -> Result<Sstr> {
    Ok(Sstr)
}

/// Parse `txlo` (TextLayoutProperty — HEIF §6.10.2.1).
/// `ItemFullProperty('txlo', version=0, flags)` with a `(flags & 1)`
/// field-size selector (§6.10.2.1.2):
///
/// ```text
/// field_size = ((flags & 1) + 1) * 16;   // 16 or 32 bits
/// unsigned int(field_size) reference_width;
/// unsigned int(field_size) reference_height;
/// signed   int(field_size) x;
/// signed   int(field_size) y;
/// unsigned int(field_size) width;
/// unsigned int(field_size) height;
/// unsigned int(16)         font_size;
/// utf8string               direction;
/// utf8string               writing_mode;
/// ```
fn parse_txlo(body: &[u8]) -> Result<Txlo> {
    let (version, flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: txlo version {version} != 0")));
    }
    let large = flags & Txlo::FLAG_LARGE_FIELD_SIZE != 0;
    let mut off = 0usize;
    let reference_width = read_txlo_u(rest, &mut off, large)?;
    let reference_height = read_txlo_u(rest, &mut off, large)?;
    let x = read_txlo_i(rest, &mut off, large)?;
    let y = read_txlo_i(rest, &mut off, large)?;
    let width = read_txlo_u(rest, &mut off, large)?;
    let height = read_txlo_u(rest, &mut off, large)?;
    let font_size = read_u16(rest, off)?;
    off += 2;
    let (direction, after_dir) = read_cstr(rest, off)?;
    let (writing_mode, _after_wm) = read_cstr(rest, after_dir)?;
    Ok(Txlo {
        flags,
        reference_width,
        reference_height,
        x,
        y,
        width,
        height,
        font_size,
        direction,
        writing_mode,
    })
}

/// Read a `txlo` variable-width **unsigned** field (16- or 32-bit per the
/// `large` selector), advancing `*off` past it.
fn read_txlo_u(buf: &[u8], off: &mut usize, large: bool) -> Result<u32> {
    if large {
        let v = read_u32(buf, *off)?;
        *off += 4;
        Ok(v)
    } else {
        let v = read_u16(buf, *off)?;
        *off += 2;
        Ok(u32::from(v))
    }
}

/// Read a `txlo` variable-width **signed** field (16- or 32-bit per the
/// `large` selector), sign-extending to `i32` and advancing `*off`.
fn read_txlo_i(buf: &[u8], off: &mut usize, large: bool) -> Result<i32> {
    if large {
        let v = read_u32(buf, *off)? as i32;
        *off += 4;
        Ok(v)
    } else {
        let v = read_u16(buf, *off)? as i16 as i32;
        *off += 2;
        Ok(v)
    }
}

/// Parse `elng` (ExtendedLanguageProperty — HEIF §6.10.2.2).
/// `ItemFullProperty('elng', version=0, flags=0)` followed by a single
/// `utf8string extended_language` (carrying ISO/IEC 14496-12 §8.4.6
/// ExtendedLanguageBox semantics applied to an item).
fn parse_elng(body: &[u8]) -> Result<Elng> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: elng version {version} != 0")));
    }
    let (extended_language, _after) = read_cstr(rest, 0)?;
    Ok(Elng { extended_language })
}

/// Parse `fnch` (FontCharacteristicsProperty — HEIF §6.10.4.1).
/// `ItemFullProperty('fnch', version=0, flags=0)` followed by three
/// `utf8string` fields: `font_family`, `font_style`, `font_weight`
/// (§6.10.4.1.2).
fn parse_fnch(body: &[u8]) -> Result<Fnch> {
    let (version, _flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: fnch version {version} != 0")));
    }
    let (font_family, after_family) = read_cstr(rest, 0)?;
    let (font_style, after_style) = read_cstr(rest, after_family)?;
    let (font_weight, _after_weight) = read_cstr(rest, after_style)?;
    Ok(Fnch {
        font_family,
        font_style,
        font_weight,
    })
}

/// Parse `cmex` (CameraExtrinsicMatrixProperty — HEIF §6.5.39).
/// ItemFullProperty(`cmex`, version, flags) followed by the present
/// fields in `flags`-selected order (§6.5.39.2):
///
/// ```text
/// if (flags & pos_x_present)       signed int(32) pos_x;
/// if (flags & pos_y_present)       signed int(32) pos_y;
/// if (flags & pos_z_present)       signed int(32) pos_z;
/// if (flags & orientation_present) {
///     if (version == 0) {
///         signed int((flags & rot_large_field_size) ? 32 : 16) quat_x;
///         signed int((flags & rot_large_field_size) ? 32 : 16) quat_y;
///         signed int((flags & rot_large_field_size) ? 32 : 16) quat_z;
///     } else if (version == 1) {
///         ViewpointGlobalCoordinateSysRotationStruct rot;
///     }
/// }
/// if (flags & id_present)          unsigned int(32) id;
/// ```
///
/// The three position fields are µm coordinates (§6.5.39.3). For
/// `version == 0` the orientation is a quaternion triplet whose element
/// width is 16 or 32 bits per `rot_large_field_size`; this parser reads
/// the signalled width and stores the raw signed values (the
/// floating-point quaternion / rotation matrix is derived by
/// [`Cmex::quaternion`] / [`Cmex::rotation_matrix`]).
///
/// For `version == 1` the orientation is a
/// `ViewpointGlobalCoordinateSysRotationStruct` from ISO/IEC 23090-7,
/// which is outside this crate's clean-room allow-list. When
/// `version == 1` has `orientation_present`, the variable-length
/// orientation struct (and therefore the byte position of any subsequent
/// `id`) cannot be parsed; the parser returns an error rather than
/// guessing the struct's length. A `version == 1` `cmex` **without**
/// orientation (positions and/or `id` only) parses fine.
///
/// Any field whose presence flag is clear stays `None` (inferred to be
/// `0` per §6.5.39.3). An unknown `version > 1` is rejected. Trailing
/// bytes past the parsed fields are ignored, mirroring the other
/// FullBox-headed property parsers in this module.
fn parse_cmex(body: &[u8]) -> Result<Cmex> {
    let (version, flags, rest) = parse_full_box(body)?;
    if version > 1 {
        return Err(Error::invalid(format!("avif: cmex version {version} > 1")));
    }
    let mut off = 0usize;
    let read_i32_at = |rest: &[u8], off: &mut usize| -> Result<i32> {
        let v = read_u32(rest, *off)? as i32;
        *off += 4;
        Ok(v)
    };

    let mut cmex = Cmex {
        flags,
        version,
        ..Cmex::default()
    };

    if flags & Cmex::FLAG_POS_X_PRESENT != 0 {
        cmex.pos_x = Some(read_i32_at(rest, &mut off)?);
    }
    if flags & Cmex::FLAG_POS_Y_PRESENT != 0 {
        cmex.pos_y = Some(read_i32_at(rest, &mut off)?);
    }
    if flags & Cmex::FLAG_POS_Z_PRESENT != 0 {
        cmex.pos_z = Some(read_i32_at(rest, &mut off)?);
    }
    if flags & Cmex::FLAG_ORIENTATION_PRESENT != 0 {
        match version {
            0 => {
                if flags & Cmex::FLAG_ROT_LARGE_FIELD_SIZE != 0 {
                    cmex.quat_x = Some(read_i32_at(rest, &mut off)?);
                    cmex.quat_y = Some(read_i32_at(rest, &mut off)?);
                    cmex.quat_z = Some(read_i32_at(rest, &mut off)?);
                } else {
                    cmex.quat_x = Some(i32::from(read_u16(rest, off)? as i16));
                    off += 2;
                    cmex.quat_y = Some(i32::from(read_u16(rest, off)? as i16));
                    off += 2;
                    cmex.quat_z = Some(i32::from(read_u16(rest, off)? as i16));
                    off += 2;
                }
            }
            // version == 1: the orientation is a
            // ViewpointGlobalCoordinateSysRotationStruct (ISO/IEC
            // 23090-7), whose byte layout is not in this crate's
            // allow-list. Refuse rather than guess its length.
            _ => {
                return Err(Error::unsupported(
                    "avif: cmex version 1 orientation \
                     (ViewpointGlobalCoordinateSysRotationStruct, \
                     ISO/IEC 23090-7) is not supported",
                ));
            }
        }
    }
    if flags & Cmex::FLAG_ID_PRESENT != 0 {
        cmex.id = Some(read_u32(rest, off)?);
    }
    Ok(cmex)
}

/// Parse `cmin` (CameraIntrinsicMatrixProperty — HEIF §6.5.40).
/// ItemFullProperty(`cmin`, version=0, flags) followed by:
///
/// ```text
/// signed int(32) focal_length_x;
/// signed int(32) principal_point_x;
/// signed int(32) principal_point_y;
/// if (flags & 1) {
///     signed int(32) focal_length_y;
///     signed int(32) skew_factor;
/// }
/// ```
///
/// per §6.5.40.2. The low `flags` bit selects the simplified (no skew,
/// square pixels) vs. full intrinsics form: when clear, `focal_length_y`
/// and `skew_factor` are absent and inferred per §6.5.40.3
/// (`fy = fx`, `s = 0`). The two 5-bit shift operands embedded in
/// `flags` (`0x001F00` for the focal-length / principal-point
/// denominator, `0x1F0000` for the skew denominator) are preserved
/// whole in [`Cmin::flags`] and decoded by the projection helpers.
///
/// An unknown `version` is rejected (§6.5.40.2 declares `version = 0`)
/// so a future layout cannot be misread as v0. The three mandatory
/// fields are always required; the two-field tail is required only when
/// `flags & 1` is set (a truncated tail is rejected). Trailing bytes
/// past the parsed fields are ignored, mirroring the
/// forward-compatibility behaviour of the other FullBox-headed property
/// parsers in this module.
fn parse_cmin(body: &[u8]) -> Result<Cmin> {
    let (version, flags, rest) = parse_full_box(body)?;
    if version != 0 {
        return Err(Error::invalid(format!("avif: cmin version {version} != 0")));
    }
    let full = flags & Cmin::FLAG_FULL_INTRINSICS != 0;
    let need = if full { 20 } else { 12 };
    if rest.len() < need {
        return Err(Error::invalid(format!(
            "avif: cmin too short ({} < {need})",
            rest.len()
        )));
    }
    let focal_length_x = read_u32(rest, 0)? as i32;
    let principal_point_x = read_u32(rest, 4)? as i32;
    let principal_point_y = read_u32(rest, 8)? as i32;
    let (focal_length_y, skew_factor) = if full {
        (
            Some(read_u32(rest, 12)? as i32),
            Some(read_u32(rest, 16)? as i32),
        )
    } else {
        (None, None)
    };
    Ok(Cmin {
        flags,
        focal_length_x,
        principal_point_x,
        principal_point_y,
        focal_length_y,
        skew_factor,
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

    /// Build an `iloc` payload (FullBox body) for a single v1 entry whose
    /// extents carry the supplied `(extent_index, offset, length)` tuples.
    /// `cm` is the construction_method nibble; `index_size` selects the
    /// per-extent `extent_index` field width (0 / 4 / 8 bytes). offsets /
    /// lengths are emitted as 4-byte fields.
    fn build_iloc_v1(cm: u8, index_size: u8, extents: &[(u64, u32, u32)]) -> Vec<u8> {
        let mut p = Vec::new();
        p.push(1); // version
        p.extend_from_slice(&[0, 0, 0]); // flags
                                         // offset_size=4, length_size=4 in byte 0; base_offset_size=0,
                                         // index_size in byte 1.
        p.push(0x44);
        // base_offset_size = 0 (high nibble), index_size in the low nibble.
        p.push(index_size & 0x0f);
        p.extend_from_slice(&1u16.to_be_bytes()); // item_count = 1
        p.extend_from_slice(&7u16.to_be_bytes()); // item_ID = 7
        p.extend_from_slice(&[0x00, cm & 0x0f]); // reserved(12)+construction_method(4)
        p.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
                                                  // base_offset_size == 0 → no base_offset bytes.
        p.extend_from_slice(&(extents.len() as u16).to_be_bytes());
        for (idx, off, len) in extents {
            match index_size {
                0 => {}
                4 => p.extend_from_slice(&(*idx as u32).to_be_bytes()),
                8 => p.extend_from_slice(&idx.to_be_bytes()),
                _ => unreachable!(),
            }
            p.extend_from_slice(&off.to_be_bytes());
            p.extend_from_slice(&len.to_be_bytes());
        }
        p
    }

    #[test]
    fn iloc_v1_cm2_captures_extent_index() {
        // construction_method=2 (item_offset), index_size=4. The §8.11.3.3
        // extent_index must be parsed and retained on each extent.
        let payload = build_iloc_v1(2, 4, &[(3, 100, 16), (5, 200, 0)]);
        let locs = parse_iloc(&payload).unwrap();
        assert_eq!(locs.len(), 1);
        let l = &locs[0];
        assert_eq!(l.id, 7);
        assert_eq!(l.construction_method, 2);
        assert_eq!(l.extents.len(), 2);
        assert_eq!(l.extents[0].extent_index, 3);
        assert_eq!(l.extents[0].offset, 100);
        assert_eq!(l.extents[0].length, 16);
        assert_eq!(l.extents[1].extent_index, 5);
        assert_eq!(l.extents[1].offset, 200);
        assert_eq!(l.extents[1].length, 0);
    }

    #[test]
    fn iloc_v1_index_size_zero_yields_zero_extent_index() {
        // index_size=0 → no extent_index field is present; the parser
        // records 0 (the cm=2 resolver then implies the value 1).
        let payload = build_iloc_v1(0, 0, &[(0, 42, 8)]);
        let locs = parse_iloc(&payload).unwrap();
        assert_eq!(locs[0].extents[0].extent_index, 0);
        assert_eq!(locs[0].extents[0].offset, 42);
        assert_eq!(locs[0].construction_method, 0);
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

    /// `amve` round-trip against the spec's BT.2035 / D65 worked example
    /// (10 lux, x=0.3127, y=0.3290). Wire body: 00 01 86 A0 3D 13 40 42.
    #[test]
    fn amve_round_trip() {
        let buf = [0x00, 0x01, 0x86, 0xA0, 0x3D, 0x13, 0x40, 0x42];
        let a = parse_amve(&buf).unwrap();
        assert_eq!(a.ambient_illuminance, 100_000);
        assert_eq!(a.ambient_light_x, 15635);
        assert_eq!(a.ambient_light_y, 16450);
        // Decoded helpers.
        assert!((a.illuminance_lux() - 10.0).abs() < 1e-9);
        let (x, y) = a.light_xy();
        assert!((x - 0.3127).abs() < 1e-9);
        assert!((y - 0.3290).abs() < 1e-9);
    }

    /// `amve` dispatches through `parse_ipco` and reports its kind.
    #[test]
    fn amve_property_kind_round_trip() {
        let body = [0x00, 0x01, 0x86, 0xA0, 0x3D, 0x13, 0x40, 0x42];
        let mut buf = Vec::new();
        buf.extend_from_slice(&(8u32 + body.len() as u32).to_be_bytes());
        buf.extend_from_slice(b"amve");
        buf.extend_from_slice(&body);
        let props = parse_ipco(&buf).unwrap();
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].kind(), AMVE);
        match &props[0] {
            Property::Amve(a) => assert_eq!(a.ambient_illuminance, 100_000),
            other => panic!("expected Amve, got {other:?}"),
        }
    }

    /// `amve` rejects a body shorter than the fixed 8 bytes.
    #[test]
    fn amve_rejects_truncated() {
        let buf = vec![0u8; 7];
        assert!(parse_amve(&buf).is_err());
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
    /// content_encoding after item_name. The canonical XMP item shape
    /// (HEIF §A.2 + RFC 3023 `application/rdf+xml`) is what AVIF
    /// readers most commonly see, so the metadata path needs to
    /// recognise it.
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
    /// with content_type=application/octet-stream — one of the
    /// real-world Exif carriers seen in HEIF / AVIF files (alongside
    /// the native `item_type == 'Exif'` form and the `image/tiff` MIME
    /// variant).
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

    // -----------------------------------------------------------------
    // HEIF §6.5.13 ImageScaling (`iscl`) — parse + helpers
    // -----------------------------------------------------------------

    fn iscl_body(wn: u16, wd: u16, hn: u16, hd: u16) -> Vec<u8> {
        // FullBox(v=0, f=0) + four u16 fields.
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&wn.to_be_bytes());
        buf.extend_from_slice(&wd.to_be_bytes());
        buf.extend_from_slice(&hn.to_be_bytes());
        buf.extend_from_slice(&hd.to_be_bytes());
        buf
    }

    #[test]
    fn iscl_round_trip_reads_all_four_fields() {
        let buf = iscl_body(3, 2, 5, 4);
        let s = parse_iscl(&buf).unwrap();
        assert_eq!(s.target_width_numerator, 3);
        assert_eq!(s.target_width_denominator, 2);
        assert_eq!(s.target_height_numerator, 5);
        assert_eq!(s.target_height_denominator, 4);
        assert!(s.is_well_formed());
    }

    #[test]
    fn iscl_rejects_truncated_body() {
        // FullBox header present but only 7 of the 8 body bytes.
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&[0, 1, 0, 1, 0, 1, 0]);
        assert!(parse_iscl(&buf).is_err());
    }

    #[test]
    fn iscl_rejects_unknown_version() {
        let mut buf = vec![1u8, 0, 0, 0]; // version=1, flags=0
        buf.extend_from_slice(&iscl_body(1, 1, 1, 1)[4..]);
        assert!(parse_iscl(&buf).is_err());
    }

    #[test]
    fn iscl_is_well_formed_rejects_zero_field() {
        // Parser surfaces the bytes; the semantic check sits on the type.
        let s = parse_iscl(&iscl_body(0, 2, 5, 4)).unwrap();
        assert!(!s.is_well_formed());
        let s = parse_iscl(&iscl_body(3, 0, 5, 4)).unwrap();
        assert!(!s.is_well_formed());
        let s = parse_iscl(&iscl_body(3, 2, 0, 4)).unwrap();
        assert!(!s.is_well_formed());
        let s = parse_iscl(&iscl_body(3, 2, 5, 0)).unwrap();
        assert!(!s.is_well_formed());
    }

    #[test]
    fn iscl_scaled_dims_applies_ceil_division() {
        // 100 × 3 / 2 = 150 (exact)
        // 100 × 5 / 4 = 125 (exact)
        let s = parse_iscl(&iscl_body(3, 2, 5, 4)).unwrap();
        assert_eq!(s.scaled_dims(100, 100), Some((150, 125)));

        // ceil((100 * 1) / 3) = ceil(33.33...) = 34
        // ceil((100 * 1) / 7) = ceil(14.28...) = 15
        let s = parse_iscl(&iscl_body(1, 3, 1, 7)).unwrap();
        assert_eq!(s.scaled_dims(100, 100), Some((34, 15)));

        // 1:1 → identity
        let s = parse_iscl(&iscl_body(1, 1, 1, 1)).unwrap();
        assert_eq!(s.scaled_dims(640, 480), Some((640, 480)));
    }

    #[test]
    fn iscl_scaled_dims_short_circuits_zero_denominator() {
        // Parser allows zero; the helper guards.
        let s = parse_iscl(&iscl_body(1, 0, 1, 1)).unwrap();
        assert_eq!(s.scaled_dims(100, 100), None);
        let s = parse_iscl(&iscl_body(1, 1, 1, 0)).unwrap();
        assert_eq!(s.scaled_dims(100, 100), None);
    }

    #[test]
    fn iscl_scaled_dims_saturates_on_u32_overflow() {
        // Numerator = max u16, denominator = 1, input = max u32 → product
        // overflows u32 but is well within u64; the saturating cast on
        // the back end keeps the helper total.
        let s = parse_iscl(&iscl_body(u16::MAX, 1, u16::MAX, 1)).unwrap();
        let dims = s.scaled_dims(u32::MAX, u32::MAX).unwrap();
        assert_eq!(dims, (u32::MAX, u32::MAX));
    }

    #[test]
    fn iscl_dispatched_through_parse_ipco() {
        // Wrap an iscl property in a single-property ipco container and
        // confirm the dispatch produces `Property::Iscl`.
        let body = iscl_body(7, 5, 11, 3);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"iscl");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Iscl(s) => {
                assert_eq!(s.target_width_numerator, 7);
                assert_eq!(s.target_height_denominator, 3);
            }
            other => panic!("expected Iscl, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"iscl");
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.17 RequiredReferenceTypesProperty (`rref`) — parse + helpers
    // -----------------------------------------------------------------

    fn rref_body(types: &[&[u8; 4]]) -> Vec<u8> {
        // FullBox(v=0, f=0) + u8 count + N u32 four-CCs.
        let mut buf = vec![0u8; 4];
        buf.push(types.len() as u8);
        for t in types {
            buf.extend_from_slice(&t[..]);
        }
        buf
    }

    #[test]
    fn rref_round_trip_reads_typed_four_ccs() {
        let buf = rref_body(&[b"dimg", b"auxl", b"thmb"]);
        let r = parse_rref(&buf).unwrap();
        assert_eq!(r.count(), 3);
        assert_eq!(r.reference_types[0], *b"dimg");
        assert_eq!(r.reference_types[1], *b"auxl");
        assert_eq!(r.reference_types[2], *b"thmb");
        assert!(r.requires(&b(b"dimg")));
        assert!(r.requires(&b(b"thmb")));
        assert!(!r.requires(&b(b"pred")));
    }

    #[test]
    fn rref_empty_list_parses() {
        // reference_type_count = 0 → no four-CC follows. Empty list is
        // syntactically valid even if §6.5.17 expects at least one type
        // on a predictively coded image item — the parser accepts and
        // the count helper reports zero.
        let buf = rref_body(&[]);
        let r = parse_rref(&buf).unwrap();
        assert_eq!(r.count(), 0);
        assert!(!r.requires(&b(b"dimg")));
    }

    #[test]
    fn rref_rejects_truncated_table() {
        // Declares 2 types but only ships one full four-CC + 2 bytes.
        let mut buf = vec![0u8; 4];
        buf.push(2);
        buf.extend_from_slice(b"dimg");
        buf.extend_from_slice(&[0x00, 0x00]); // 2 of the 4 bytes of the next type
        assert!(parse_rref(&buf).is_err());
    }

    #[test]
    fn rref_rejects_unknown_version() {
        let mut buf = vec![1u8, 0, 0, 0]; // version=1, flags=0
        buf.push(0);
        assert!(parse_rref(&buf).is_err());
    }

    #[test]
    fn rref_rejects_missing_count() {
        // FullBox header only — no `reference_type_count` byte.
        let buf = vec![0u8; 4];
        assert!(parse_rref(&buf).is_err());
    }

    #[test]
    fn rref_dispatched_through_parse_ipco() {
        // Wrap an rref property in a single-property ipco container.
        let body = rref_body(&[b"dimg"]);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"rref");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Rref(r) => {
                assert_eq!(r.count(), 1);
                assert_eq!(r.reference_types[0], *b"dimg");
            }
            other => panic!("expected Rref, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"rref");
    }

    /// Recognised `iscl` and `rref` properties — even when flagged
    /// essential — do **not** trip
    /// [`Meta::unsupported_essential_properties`]; only
    /// [`Property::Other`] essential associations should be reported.
    #[test]
    fn iscl_and_rref_essential_associations_are_recognised() {
        let m = Meta {
            properties: vec![
                Property::Iscl(Iscl {
                    target_width_numerator: 1,
                    target_width_denominator: 1,
                    target_height_numerator: 1,
                    target_height_denominator: 1,
                }),
                Property::Rref(Rref {
                    reference_types: vec![*b"dimg"],
                }),
            ],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![
                    PropertyAssociation {
                        index: 0,
                        essential: true,
                    },
                    PropertyAssociation {
                        index: 1,
                        essential: true,
                    },
                ],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.18 CreationTimeProperty (`crtt`) — parse + helpers
    // -----------------------------------------------------------------

    fn crtt_body(creation_time: u64) -> Vec<u8> {
        // FullBox(v=0, f=0) + one u64.
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&creation_time.to_be_bytes());
        buf
    }

    #[test]
    fn crtt_round_trip_reads_creation_time() {
        // Pick a recognisable big-endian pattern so a byte swap in the
        // reader would surface immediately.
        let raw = 0x0102_0304_0506_0708u64;
        let c = parse_crtt(&crtt_body(raw)).unwrap();
        assert_eq!(c.creation_time, raw);
    }

    #[test]
    fn crtt_rejects_truncated_body() {
        // FullBox header present but only 7 of the 8 body bytes follow.
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
        assert!(parse_crtt(&buf).is_err());
    }

    #[test]
    fn crtt_rejects_unknown_version() {
        // version=1, flags=0; body bytes are otherwise well-formed.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&0u64.to_be_bytes());
        assert!(parse_crtt(&buf).is_err());
    }

    #[test]
    fn crtt_rejects_missing_payload() {
        // FullBox header only — the u64 timestamp is absent entirely.
        let buf = vec![0u8; 4];
        assert!(parse_crtt(&buf).is_err());
    }

    #[test]
    fn crtt_dispatched_through_parse_ipco() {
        // Wrap a `crtt` property in a single-property `ipco` container
        // and confirm the dispatch produces `Property::Crtt`.
        let body = crtt_body(0xDEAD_BEEF_CAFE_F00Du64);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"crtt");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Crtt(c) => assert_eq!(c.creation_time, 0xDEAD_BEEF_CAFE_F00Du64),
            other => panic!("expected Crtt, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"crtt");
    }

    #[test]
    fn crtt_seconds_since_unix_epoch_matches_documented_offset() {
        // `creation_time == 0` is exactly the 1904-01-01 UTC epoch —
        // precedes the Unix epoch, so the helper underflows to None.
        let c = Crtt { creation_time: 0 };
        assert_eq!(c.seconds_since_unix_epoch(), None);

        // Exactly the Unix epoch — 1970-01-01 00:00:00 UTC — sits
        // HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS seconds after the HEIF
        // epoch, so it maps to 0 Unix seconds. Express in microseconds.
        let c = Crtt {
            creation_time: HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS * 1_000_000,
        };
        assert_eq!(c.seconds_since_unix_epoch(), Some(0));

        // 1970-01-01 00:00:01 UTC → 1 Unix second.
        let c = Crtt {
            creation_time: (HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS + 1) * 1_000_000,
        };
        assert_eq!(c.seconds_since_unix_epoch(), Some(1));
    }

    #[test]
    fn crtt_subsecond_micros_isolates_remainder() {
        // 1.5 seconds past the Unix epoch in 1904-epoch microseconds.
        let secs = HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS;
        let c = Crtt {
            creation_time: secs * 1_000_000 + 500_000,
        };
        assert_eq!(c.seconds_since_unix_epoch(), Some(0));
        assert_eq!(c.subsecond_micros(), 500_000);

        // No sub-second component → returns 0.
        let c = Crtt {
            creation_time: secs * 1_000_000,
        };
        assert_eq!(c.subsecond_micros(), 0);

        // Highest legal sub-second value (999_999 µs).
        let c = Crtt {
            creation_time: secs * 1_000_000 + 999_999,
        };
        assert_eq!(c.subsecond_micros(), 999_999);
    }

    /// A recognised `crtt` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser doesn't
    /// reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn crtt_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Crtt(Crtt { creation_time: 0 })],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.19 ModificationTimeProperty (`mdft`) — parse + helpers
    // -----------------------------------------------------------------

    fn mdft_body(modification_time: u64) -> Vec<u8> {
        // FullBox(v=0, f=0) + one u64.
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&modification_time.to_be_bytes());
        buf
    }

    #[test]
    fn mdft_round_trip_reads_modification_time() {
        // Distinct big-endian pattern (avoid the crtt sentinel) so a
        // byte swap or a field cross-wire would surface immediately.
        let raw = 0x1122_3344_5566_7788u64;
        let m = parse_mdft(&mdft_body(raw)).unwrap();
        assert_eq!(m.modification_time, raw);
    }

    #[test]
    fn mdft_rejects_truncated_body() {
        // FullBox header present but only 7 of the 8 body bytes follow.
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
        assert!(parse_mdft(&buf).is_err());
    }

    #[test]
    fn mdft_rejects_unknown_version() {
        // version=1, flags=0; body bytes are otherwise well-formed.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&0u64.to_be_bytes());
        assert!(parse_mdft(&buf).is_err());
    }

    #[test]
    fn mdft_rejects_missing_payload() {
        // FullBox header only — the u64 timestamp is absent entirely.
        let buf = vec![0u8; 4];
        assert!(parse_mdft(&buf).is_err());
    }

    #[test]
    fn mdft_dispatched_through_parse_ipco() {
        // Wrap an `mdft` property in a single-property `ipco` container
        // and confirm the dispatch produces `Property::Mdft`.
        let body = mdft_body(0xFEED_FACE_DEAD_BEEFu64);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"mdft");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Mdft(m) => assert_eq!(m.modification_time, 0xFEED_FACE_DEAD_BEEFu64),
            other => panic!("expected Mdft, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"mdft");
    }

    #[test]
    fn mdft_seconds_since_unix_epoch_matches_documented_offset() {
        // `modification_time == 0` is exactly the 1904-01-01 UTC epoch —
        // precedes the Unix epoch, so the helper underflows to None.
        let m = Mdft {
            modification_time: 0,
        };
        assert_eq!(m.seconds_since_unix_epoch(), None);

        // Exactly the Unix epoch — 1970-01-01 00:00:00 UTC — sits
        // HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS seconds after the HEIF
        // epoch, so it maps to 0 Unix seconds. Express in microseconds.
        let m = Mdft {
            modification_time: HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS * 1_000_000,
        };
        assert_eq!(m.seconds_since_unix_epoch(), Some(0));

        // 1970-01-01 00:00:01 UTC → 1 Unix second.
        let m = Mdft {
            modification_time: (HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS + 1) * 1_000_000,
        };
        assert_eq!(m.seconds_since_unix_epoch(), Some(1));
    }

    #[test]
    fn mdft_subsecond_micros_isolates_remainder() {
        // 0.5 seconds past the Unix epoch in 1904-epoch microseconds.
        let secs = HEIF_EPOCH_TO_UNIX_EPOCH_SECONDS;
        let m = Mdft {
            modification_time: secs * 1_000_000 + 500_000,
        };
        assert_eq!(m.seconds_since_unix_epoch(), Some(0));
        assert_eq!(m.subsecond_micros(), 500_000);

        // No sub-second component → returns 0.
        let m = Mdft {
            modification_time: secs * 1_000_000,
        };
        assert_eq!(m.subsecond_micros(), 0);

        // Highest legal sub-second value (999_999 µs).
        let m = Mdft {
            modification_time: secs * 1_000_000 + 999_999,
        };
        assert_eq!(m.subsecond_micros(), 999_999);
    }

    /// A recognised `mdft` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser doesn't
    /// reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn mdft_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Mdft(Mdft {
                modification_time: 0,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// `mdft` + `crtt` may legally co-occur on the same item; the
    /// dispatch returns both properties in insertion order, each
    /// associable by its index, with `property_for` resolving each
    /// kind independently.
    #[test]
    fn mdft_and_crtt_coexist_on_same_item() {
        let crtt_raw = 0x0102_0304_0506_0708u64;
        let mdft_raw = 0xAABB_CCDD_EEFF_0011u64;
        let crtt_b = crtt_body(crtt_raw);
        let mdft_b = mdft_body(mdft_raw);
        let mut ipco = Vec::new();
        // crtt entry
        let s1 = 8 + crtt_b.len() as u32;
        ipco.extend_from_slice(&s1.to_be_bytes());
        ipco.extend_from_slice(b"crtt");
        ipco.extend_from_slice(&crtt_b);
        // mdft entry
        let s2 = 8 + mdft_b.len() as u32;
        ipco.extend_from_slice(&s2.to_be_bytes());
        ipco.extend_from_slice(b"mdft");
        ipco.extend_from_slice(&mdft_b);

        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 2);
        let m = Meta {
            properties: props,
            associations: vec![ItemPropertyAssociation {
                item_id: 7,
                entries: vec![
                    PropertyAssociation {
                        index: 0,
                        essential: false,
                    },
                    PropertyAssociation {
                        index: 1,
                        essential: false,
                    },
                ],
            }],
            ..Meta::default()
        };
        match m.property_for(7, &CRTT) {
            Some(Property::Crtt(c)) => assert_eq!(c.creation_time, crtt_raw),
            other => panic!("expected Crtt, got {other:?}"),
        }
        match m.property_for(7, &MDFT) {
            Some(Property::Mdft(d)) => assert_eq!(d.modification_time, mdft_raw),
            other => panic!("expected Mdft, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.20 UserDescriptionProperty (`udes`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build a `udes` body — FullBox(v=0, f=0) followed by four
    /// nul-terminated UTF-8 strings in the §6.5.20.2 declaration
    /// order: `lang`, `name`, `description`, `tags`.
    fn udes_body(lang: &str, name: &str, description: &str, tags: &str) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        for s in [lang, name, description, tags] {
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn udes_round_trip_reads_all_four_fields() {
        // Distinct values per field so a cross-wire between lang /
        // name / description / tags would surface immediately.
        let u = parse_udes(&udes_body(
            "en-US",
            "Front porch",
            "Photo of the cabin's front porch at dusk",
            "outdoor,porch,dusk",
        ))
        .unwrap();
        assert_eq!(u.lang, "en-US");
        assert_eq!(u.name, "Front porch");
        assert_eq!(u.description, "Photo of the cabin's front porch at dusk");
        assert_eq!(u.tags, "outdoor,porch,dusk");
    }

    /// §6.5.20.3 documents an empty string for any of the four fields
    /// as the "absent" sentinel. The parser surfaces the raw empty
    /// string verbatim; the `*_opt` helpers project to `None`.
    #[test]
    fn udes_empty_strings_are_preserved_and_projectable_to_none() {
        // All four fields empty (just nul terminators). This is the
        // minimal-length §6.5.20.2 body — a FullBox header and four
        // bytes of nul.
        let u = parse_udes(&udes_body("", "", "", "")).unwrap();
        assert_eq!(u.lang, "");
        assert_eq!(u.name, "");
        assert_eq!(u.description, "");
        assert_eq!(u.tags, "");
        assert_eq!(u.lang_opt(), None);
        assert_eq!(u.name_opt(), None);
        assert_eq!(u.description_opt(), None);
        assert_eq!(u.tags_opt(), None);
        assert!(u.tag_list().is_empty());
    }

    #[test]
    fn udes_opt_helpers_round_trip_non_empty() {
        let u = parse_udes(&udes_body("fr-FR", "Mer", "Vue de la mer", "mer,été")).unwrap();
        assert_eq!(u.lang_opt(), Some("fr-FR"));
        assert_eq!(u.name_opt(), Some("Mer"));
        assert_eq!(u.description_opt(), Some("Vue de la mer"));
        assert_eq!(u.tags_opt(), Some("mer,été"));
    }

    /// `tag_list` splits on `','`, trims whitespace per segment, and
    /// drops empty / whitespace-only segments. The raw `tags` field
    /// is preserved verbatim — `tag_list` is a derived view.
    #[test]
    fn udes_tag_list_splits_and_trims() {
        let u = parse_udes(&udes_body(
            "en",
            "n",
            "d",
            "outdoor, sunset ,, beach,,, mountain",
        ))
        .unwrap();
        assert_eq!(u.tag_list(), vec!["outdoor", "sunset", "beach", "mountain"]);
        // raw field is untouched.
        assert_eq!(u.tags, "outdoor, sunset ,, beach,,, mountain");
    }

    /// UTF-8 multi-byte payloads (CJK + accented Latin) survive the
    /// parser intact — the underlying cstring reader uses
    /// `from_utf8_lossy`, so the round-trip is byte-equal for any
    /// already-valid UTF-8 input.
    #[test]
    fn udes_preserves_utf8_multibyte() {
        let u = parse_udes(&udes_body(
            "zh-CN",
            "海岸",
            "夕暮れの海岸",
            "海, 夕暮れ, 風景",
        ))
        .unwrap();
        assert_eq!(u.lang, "zh-CN");
        assert_eq!(u.name, "海岸");
        assert_eq!(u.description, "夕暮れの海岸");
        assert_eq!(u.tag_list(), vec!["海", "夕暮れ", "風景"]);
    }

    #[test]
    fn udes_rejects_unknown_version() {
        // version=1, flags=0; four empty strings would otherwise be
        // a well-formed v0 body.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&[0, 0, 0, 0]);
        assert!(parse_udes(&buf).is_err());
    }

    /// A body that runs out before the fourth nul is written must be
    /// rejected — the `read_cstr` helper bails on an unterminated
    /// string, which is exactly what we want here so a truncated
    /// `udes` cannot be partially read.
    #[test]
    fn udes_rejects_truncated_body() {
        // FullBox header + three empty strings + start of the fourth
        // without a terminator.
        let mut buf = vec![0u8; 4];
        // lang = ""
        buf.push(0);
        // name = ""
        buf.push(0);
        // description = ""
        buf.push(0);
        // tags = "abc" with no trailing nul.
        buf.extend_from_slice(b"abc");
        assert!(parse_udes(&buf).is_err());
    }

    /// Trailing bytes past the fourth terminator are forward-compat
    /// space — the parser ignores them (mirrors the §8.11.6 `infe`
    /// tail behaviour). A v0 producer that pads the box with extra
    /// reserved bytes is read cleanly.
    #[test]
    fn udes_tolerates_trailing_bytes() {
        let mut body = udes_body("en", "n", "d", "t");
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let u = parse_udes(&body).unwrap();
        assert_eq!(u.lang, "en");
        assert_eq!(u.tags, "t");
    }

    #[test]
    fn udes_dispatched_through_parse_ipco() {
        let body = udes_body("en-US", "Cabin", "Front porch", "outdoor,porch");
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"udes");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Udes(u) => {
                assert_eq!(u.lang, "en-US");
                assert_eq!(u.name, "Cabin");
                assert_eq!(u.description, "Front porch");
                assert_eq!(u.tags, "outdoor,porch");
            }
            other => panic!("expected Udes, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"udes");
    }

    /// A recognised `udes` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser doesn't
    /// reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn udes_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Udes(Udes::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.20.1 allows zero-or-more `udes` instances per item, with
    /// each instance carrying a different `lang` — the dispatch
    /// returns every `udes` in insertion order so the caller can pick
    /// the most appropriate language.
    #[test]
    fn udes_multiple_languages_coexist_on_same_item() {
        let en = udes_body("en-US", "Beach", "Sunset over the bay", "beach,sunset");
        let fr = udes_body(
            "fr-FR",
            "Plage",
            "Coucher de soleil sur la baie",
            "plage,coucher",
        );
        let mut ipco = Vec::new();
        let se = 8 + en.len() as u32;
        ipco.extend_from_slice(&se.to_be_bytes());
        ipco.extend_from_slice(b"udes");
        ipco.extend_from_slice(&en);
        let sf = 8 + fr.len() as u32;
        ipco.extend_from_slice(&sf.to_be_bytes());
        ipco.extend_from_slice(b"udes");
        ipco.extend_from_slice(&fr);

        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 2);
        let langs: Vec<&str> = props
            .iter()
            .filter_map(|p| match p {
                Property::Udes(u) => Some(u.lang.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(langs, vec!["en-US", "fr-FR"]);
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.21 AccessibilityTextProperty (`altt`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build an `altt` body — FullBox(v=0, f=0) followed by two
    /// nul-terminated UTF-8 strings in the §6.5.21.2 declaration
    /// order: `alt_text`, then `alt_lang`. The reversed pairing
    /// relative to `udes` is intentional — `udes` lists `lang` first
    /// and `altt` lists the text first.
    fn altt_body(alt_text: &str, alt_lang: &str) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        for s in [alt_text, alt_lang] {
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn altt_round_trip_reads_text_then_lang() {
        // Distinct values per field so a cross-wire between
        // alt_text and alt_lang would surface immediately.
        let a = parse_altt(&altt_body(
            "Photo of the cabin's front porch at dusk",
            "en-US",
        ))
        .unwrap();
        assert_eq!(a.alt_text, "Photo of the cabin's front porch at dusk");
        assert_eq!(a.alt_lang, "en-US");
    }

    /// §6.5.21.3 documents an empty `alt_lang` as the
    /// "unknown/undefined" sentinel. The parser surfaces the raw
    /// empty string verbatim; the `*_opt` helpers project to `None`.
    /// `alt_text` is also tolerated as empty here even though the
    /// spec text does not promote it explicitly — the parse still
    /// preserves the raw shape and `alt_text_opt` is `None` for the
    /// empty case.
    #[test]
    fn altt_empty_strings_are_preserved_and_projectable_to_none() {
        let a = parse_altt(&altt_body("", "")).unwrap();
        assert_eq!(a.alt_text, "");
        assert_eq!(a.alt_lang, "");
        assert_eq!(a.alt_text_opt(), None);
        assert_eq!(a.alt_lang_opt(), None);
    }

    #[test]
    fn altt_opt_helpers_round_trip_non_empty() {
        let a = parse_altt(&altt_body("Vue de la mer", "fr-FR")).unwrap();
        assert_eq!(a.alt_text_opt(), Some("Vue de la mer"));
        assert_eq!(a.alt_lang_opt(), Some("fr-FR"));
    }

    /// UTF-8 multi-byte payloads (CJK + accented Latin) survive the
    /// parser intact — `read_cstr` uses `from_utf8_lossy`, so the
    /// round-trip is byte-equal for already-valid UTF-8 input.
    #[test]
    fn altt_preserves_utf8_multibyte() {
        let a = parse_altt(&altt_body("夕暮れの海岸", "zh-CN")).unwrap();
        assert_eq!(a.alt_text, "夕暮れの海岸");
        assert_eq!(a.alt_lang, "zh-CN");
    }

    #[test]
    fn altt_rejects_unknown_version() {
        // version=1, flags=0; two empty strings would otherwise be a
        // well-formed v0 body.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&[0, 0]);
        assert!(parse_altt(&buf).is_err());
    }

    /// A body that runs out before the second nul is written must be
    /// rejected — `read_cstr` bails on an unterminated string, which
    /// is exactly the behaviour we want so a truncated `altt` cannot
    /// be partially read.
    #[test]
    fn altt_rejects_truncated_body() {
        // FullBox header + one empty string + start of the second
        // without a terminator.
        let mut buf = vec![0u8; 4];
        // alt_text = ""
        buf.push(0);
        // alt_lang = "en" with no trailing nul.
        buf.extend_from_slice(b"en");
        assert!(parse_altt(&buf).is_err());
    }

    /// Trailing bytes past the second terminator are forward-compat
    /// space — the parser ignores them (mirrors the §8.11.6 `infe`
    /// tail behaviour). A v0 producer that pads the box with extra
    /// reserved bytes is read cleanly.
    #[test]
    fn altt_tolerates_trailing_bytes() {
        let mut body = altt_body("hi", "en");
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let a = parse_altt(&body).unwrap();
        assert_eq!(a.alt_text, "hi");
        assert_eq!(a.alt_lang, "en");
    }

    #[test]
    fn altt_dispatched_through_parse_ipco() {
        let body = altt_body("Cabin at dusk", "en-US");
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"altt");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Altt(a) => {
                assert_eq!(a.alt_text, "Cabin at dusk");
                assert_eq!(a.alt_lang, "en-US");
            }
            other => panic!("expected Altt, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"altt");
    }

    /// A recognised `altt` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser does not
    /// reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn altt_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Altt(Altt::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.21.1 allows zero-or-more `altt` instances per item with
    /// each instance carrying a different `alt_lang` — the dispatch
    /// returns every `altt` in insertion order so the caller can pick
    /// the most appropriate language.
    #[test]
    fn altt_multiple_languages_coexist_on_same_item() {
        let en = altt_body("Sunset over the bay", "en-US");
        let fr = altt_body("Coucher de soleil sur la baie", "fr-FR");
        let mut ipco = Vec::new();
        let se = 8 + en.len() as u32;
        ipco.extend_from_slice(&se.to_be_bytes());
        ipco.extend_from_slice(b"altt");
        ipco.extend_from_slice(&en);
        let sf = 8 + fr.len() as u32;
        ipco.extend_from_slice(&sf.to_be_bytes());
        ipco.extend_from_slice(b"altt");
        ipco.extend_from_slice(&fr);

        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 2);
        let langs: Vec<&str> = props
            .iter()
            .filter_map(|p| match p {
                Property::Altt(a) => Some(a.alt_lang.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(langs, vec!["en-US", "fr-FR"]);
    }

    /// `altt` reverses the §6.5.20 `udes` field ordering — `altt`
    /// puts the text first and the language second, while `udes`
    /// puts the language first. A bit-rotted parser that copy-pasted
    /// `udes`'s field order would surface immediately: an `altt`
    /// whose `alt_text == "en-US"` and `alt_lang == "Sunset"` is a
    /// red flag, not a valid payload. This test pins the documented
    /// order against that regression.
    #[test]
    fn altt_field_order_is_text_then_lang_not_reversed() {
        // The wire bytes are unambiguous about which string is
        // `alt_text` and which is `alt_lang`. If the parser swapped
        // the assignment, the assertions below would flip.
        let a = parse_altt(&altt_body("Sunset", "en-US")).unwrap();
        assert_eq!(a.alt_text, "Sunset");
        assert_eq!(a.alt_lang, "en-US");
        // And the inverse — if a writer accidentally put the
        // language first, the parser MUST surface that wire-level
        // mistake by carrying the language tag as `alt_text` and the
        // English description as `alt_lang`, rather than silently
        // re-ordering for the caller.
        let mis = parse_altt(&altt_body("en-US", "Sunset")).unwrap();
        assert_eq!(mis.alt_text, "en-US");
        assert_eq!(mis.alt_lang, "Sunset");
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.22 AutoExposureProperty (`aebr`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build an `aebr` body — FullBox(v=0, f=0) followed by two
    /// `int(8)` fields in the §6.5.22.2 declaration order:
    /// `exposure_step`, then `exposure_numerator`. The helper accepts
    /// signed `i8` inputs and writes them as their two's-complement
    /// byte form so a negative `exposure_numerator` (darker than the
    /// camera setting) is exercised faithfully.
    fn aebr_body(exposure_step: i8, exposure_numerator: i8) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        buf.push(exposure_step as u8);
        buf.push(exposure_numerator as u8);
        buf
    }

    #[test]
    fn aebr_round_trip_reads_step_then_numerator() {
        // Distinct values per field so a cross-wire between
        // exposure_step and exposure_numerator would surface
        // immediately.
        let a = parse_aebr(&aebr_body(3, 5)).unwrap();
        assert_eq!(a.exposure_step, 3);
        assert_eq!(a.exposure_numerator, 5);
    }

    /// The §6.5.22.3 enumeration for `exposure_step` documents four
    /// defined values; the `STEP_*` constants pin each one and the
    /// `is_defined_step` helper rejects everything else (including
    /// `0` and the negative range).
    #[test]
    fn aebr_defined_step_enumeration() {
        for s in [
            Aebr::STEP_FULL,
            Aebr::STEP_HALF,
            Aebr::STEP_THIRD,
            Aebr::STEP_QUARTER,
        ] {
            let a = parse_aebr(&aebr_body(s, 1)).unwrap();
            assert!(a.is_defined_step(), "step {s} should be defined");
        }
        for s in [-1, 0, 5, 7, i8::MIN, i8::MAX] {
            let a = parse_aebr(&aebr_body(s, 1)).unwrap();
            assert!(!a.is_defined_step(), "step {s} must NOT be defined");
        }
    }

    /// The §6.5.22.3 stops formula is `exposure_numerator /
    /// exposure_step`. The helper returns `Some(f)` for every
    /// non-zero `exposure_step` (including reserved values, so a
    /// strict reader can route through `is_defined_step` first to
    /// gate the enumeration) and `None` for a zero step (the
    /// reserved sentinel that would divide by zero).
    #[test]
    fn aebr_exposure_stops_matches_spec_ratio() {
        // Full-stop: -2 numerator → -2.0 stops (two stops darker).
        let a = parse_aebr(&aebr_body(1, -2)).unwrap();
        assert_eq!(a.exposure_stops(), Some(-2.0));
        // Half-stop: 3 numerator → +1.5 stops.
        let a = parse_aebr(&aebr_body(2, 3)).unwrap();
        assert_eq!(a.exposure_stops(), Some(1.5));
        // Third-stop: 4 numerator → +4/3 stops.
        let a = parse_aebr(&aebr_body(3, 4)).unwrap();
        let v = a.exposure_stops().unwrap();
        assert!((v - 4.0 / 3.0).abs() < 1e-12, "got {v}");
        // Quarter-stop: -3 numerator → -0.75 stops.
        let a = parse_aebr(&aebr_body(4, -3)).unwrap();
        assert_eq!(a.exposure_stops(), Some(-0.75));
        // Zero step (reserved): no float interpretation.
        let a = parse_aebr(&aebr_body(0, 7)).unwrap();
        assert_eq!(a.exposure_stops(), None);
    }

    /// Both fields are signed: the parser must read a negative
    /// `exposure_numerator` as the two's-complement `i8` value, not
    /// as an unsigned byte. A writer that produces `-1` (0xFF) for
    /// "one stop darker" must round-trip to `-1`, not `255`.
    #[test]
    fn aebr_signed_byte_reinterpretation() {
        let a = parse_aebr(&aebr_body(1, -1)).unwrap();
        assert_eq!(a.exposure_numerator, -1);
        assert_eq!(a.exposure_stops(), Some(-1.0));

        let a = parse_aebr(&aebr_body(-1, 5)).unwrap();
        assert_eq!(a.exposure_step, -1);
        // Reserved-step value; the helper still computes a ratio so
        // the caller can route through `is_defined_step` to gate the
        // enumeration explicitly.
        assert!(!a.is_defined_step());
        assert_eq!(a.exposure_stops(), Some(-5.0));

        let a = parse_aebr(&aebr_body(i8::MIN, i8::MAX)).unwrap();
        assert_eq!(a.exposure_step, i8::MIN);
        assert_eq!(a.exposure_numerator, i8::MAX);
    }

    #[test]
    fn aebr_rejects_unknown_version() {
        // version=1, flags=0; two zero bytes would otherwise be a
        // well-formed v0 body.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&[0, 0]);
        assert!(parse_aebr(&buf).is_err());
    }

    /// A body shorter than the two-byte fixed tail must be rejected
    /// so a truncated `aebr` cannot be partially read.
    #[test]
    fn aebr_rejects_truncated_body() {
        // FullBox header alone — no fields at all.
        let buf = vec![0u8; 4];
        assert!(parse_aebr(&buf).is_err());
        // FullBox header + one field only.
        let mut buf = vec![0u8; 4];
        buf.push(2);
        assert!(parse_aebr(&buf).is_err());
    }

    /// Trailing bytes past the two-byte tail are forward-compat
    /// space — the parser ignores them, mirroring the behaviour of
    /// every other FullBox-headed property parser in this module.
    #[test]
    fn aebr_tolerates_trailing_bytes() {
        let mut body = aebr_body(2, 1);
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let a = parse_aebr(&body).unwrap();
        assert_eq!(a.exposure_step, 2);
        assert_eq!(a.exposure_numerator, 1);
    }

    #[test]
    fn aebr_dispatched_through_parse_ipco() {
        let body = aebr_body(3, -2);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"aebr");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Aebr(a) => {
                assert_eq!(a.exposure_step, 3);
                assert_eq!(a.exposure_numerator, -2);
            }
            other => panic!("expected Aebr, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"aebr");
    }

    /// A recognised `aebr` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser does not
    /// reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn aebr_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Aebr(Aebr::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.22.1 caps `aebr` at one per item (`At most one`). A
    /// `Meta::property_for(item_id, &AEBR)` lookup finds the
    /// associated single instance via the standard `ipma` walk; the
    /// dispatch returns the `Aebr` variant which the caller pattern
    /// matches on. This test exercises the typical lookup shape end
    /// to end.
    #[test]
    fn aebr_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Aebr(Aebr {
                exposure_step: 2,
                exposure_numerator: -1,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 9,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(9, b"aebr") {
            Some(Property::Aebr(a)) => {
                assert_eq!(a.exposure_step, 2);
                assert_eq!(a.exposure_numerator, -1);
                assert_eq!(a.exposure_stops(), Some(-0.5));
            }
            other => panic!("expected Aebr, got {other:?}"),
        }
        // No `aebr` for an item that doesn't carry the association.
        assert!(m.property_for(99, b"aebr").is_none());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.23 WhiteBalanceProperty (`wbbr`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build a `wbbr` body — FullBox(v=0, f=0) followed by an
    /// `unsigned int(16)` `blue_amber` (big-endian per ISO/IEC
    /// 14496-12 §4.2) and a signed `int(8)` `green_magenta`, in the
    /// §6.5.23.2 declaration order. The helper accepts an `i8` for
    /// the second field and writes its two's-complement byte form
    /// so a negative `green_magenta` (magenta colour shift) is
    /// exercised faithfully.
    fn wbbr_body(blue_amber: u16, green_magenta: i8) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&blue_amber.to_be_bytes());
        buf.push(green_magenta as u8);
        buf
    }

    #[test]
    fn wbbr_round_trip_reads_blue_amber_then_green_magenta() {
        // Distinct, asymmetric values per field so a cross-wire
        // between `blue_amber` and `green_magenta` would surface
        // immediately. 5600 K is a midday-daylight Kelvin value
        // wide enough to require both bytes of the 16-bit field.
        let w = parse_wbbr(&wbbr_body(5600, 7)).unwrap();
        assert_eq!(w.blue_amber, 5600);
        assert_eq!(w.green_magenta, 7);
    }

    /// `blue_amber` is `unsigned int(16)` and is read big-endian
    /// per ISO/IEC 14496-12 §4.2. A value that requires the high
    /// byte to be non-zero (`0x15B0` = 5552) pins the byte order
    /// against a little-endian regression that would surface as
    /// `0xB015` = 45077 K — well outside the photographic span and
    /// easy to catch.
    #[test]
    fn wbbr_blue_amber_is_big_endian() {
        let w = parse_wbbr(&wbbr_body(0x15B0, 0)).unwrap();
        assert_eq!(w.blue_amber, 0x15B0);
        // Endpoint coverage: u16::MAX exercises the high bit.
        let w = parse_wbbr(&wbbr_body(u16::MAX, 0)).unwrap();
        assert_eq!(w.blue_amber, u16::MAX);
        // 0 K is the spec's lower bound — the wire field is
        // unsigned so no sign extension is in play.
        let w = parse_wbbr(&wbbr_body(0, 0)).unwrap();
        assert_eq!(w.blue_amber, 0);
    }

    /// The §6.5.23.3 NOTE describes `green_magenta` as a signed
    /// 1/100 Duv value: negative = magenta shift, positive = green
    /// shift, zero = neutral. The parser must read the byte as
    /// two's-complement `i8`, not as an unsigned byte. A writer
    /// that produces `-1` (`0xFF`) for "0.01 Duv magenta shift"
    /// must round-trip to `-1`, not `255`.
    #[test]
    fn wbbr_signed_green_magenta_reinterpretation() {
        let w = parse_wbbr(&wbbr_body(5600, -1)).unwrap();
        assert_eq!(w.green_magenta, -1);
        // Endpoint coverage: i8 min/max round-trip.
        let w = parse_wbbr(&wbbr_body(5600, i8::MIN)).unwrap();
        assert_eq!(w.green_magenta, i8::MIN);
        let w = parse_wbbr(&wbbr_body(5600, i8::MAX)).unwrap();
        assert_eq!(w.green_magenta, i8::MAX);
    }

    /// The §6.5.23.3 NOTE projection: the wire field is in 1/100
    /// Duv, so `green_magenta_duv()` returns `green_magenta /
    /// 100.0` — `-50` is `-0.5` Duv (magenta), `+50` is `+0.5` Duv
    /// (green), `0` is the neutral sentinel.
    #[test]
    fn wbbr_green_magenta_duv_projection() {
        let w = parse_wbbr(&wbbr_body(5600, -50)).unwrap();
        assert_eq!(w.green_magenta_duv(), -0.5);
        let w = parse_wbbr(&wbbr_body(5600, 50)).unwrap();
        assert_eq!(w.green_magenta_duv(), 0.5);
        let w = parse_wbbr(&wbbr_body(5600, 0)).unwrap();
        assert_eq!(w.green_magenta_duv(), 0.0);
        // Endpoint: i8::MIN as 1/100 Duv is -1.28 Duv (well past
        // any realistic camera adjustment, so this exercises the
        // projection arithmetic at the wire-format extreme).
        let w = parse_wbbr(&wbbr_body(5600, i8::MIN)).unwrap();
        assert!((w.green_magenta_duv() - (-1.28)).abs() < 1e-12);
    }

    /// §6.5.23.3 NOTE: `green_magenta == 0` is the documented
    /// neutral sentinel. The predicate flips on exactly that value
    /// regardless of the `blue_amber` colour-temperature reading
    /// (the two components are independent per §6.5.23.3).
    #[test]
    fn wbbr_green_magenta_neutral_predicate() {
        let w = parse_wbbr(&wbbr_body(5600, 0)).unwrap();
        assert!(w.is_green_magenta_neutral());
        // `blue_amber` is independent: a non-default colour
        // temperature with a zero deviation is still neutral on
        // the green/magenta axis.
        let w = parse_wbbr(&wbbr_body(2700, 0)).unwrap();
        assert!(w.is_green_magenta_neutral());
        let w = parse_wbbr(&wbbr_body(0, 0)).unwrap();
        assert!(w.is_green_magenta_neutral());
        // Any non-zero deviation flips off the predicate.
        for gm in [-1, 1, -50, 50, i8::MIN, i8::MAX] {
            let w = parse_wbbr(&wbbr_body(5600, gm)).unwrap();
            assert!(
                !w.is_green_magenta_neutral(),
                "green_magenta {gm} must NOT be neutral"
            );
        }
        assert_eq!(Wbbr::NEUTRAL_GREEN_MAGENTA, 0);
    }

    #[test]
    fn wbbr_rejects_unknown_version() {
        // version=1, flags=0; three zero bytes would otherwise be
        // a well-formed v0 body.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&[0, 0, 0]);
        assert!(parse_wbbr(&buf).is_err());
    }

    /// A body shorter than the three-byte fixed tail must be
    /// rejected so a truncated `wbbr` cannot be partially read.
    #[test]
    fn wbbr_rejects_truncated_body() {
        // FullBox header alone — no fields at all.
        let buf = vec![0u8; 4];
        assert!(parse_wbbr(&buf).is_err());
        // FullBox header + `blue_amber` only (missing
        // `green_magenta`).
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&[0x15, 0xB0]);
        assert!(parse_wbbr(&buf).is_err());
        // FullBox header + one byte of `blue_amber` only
        // (missing the second byte plus `green_magenta`).
        let mut buf = vec![0u8; 4];
        buf.push(0x15);
        assert!(parse_wbbr(&buf).is_err());
    }

    /// Trailing bytes past the three-byte tail are forward-compat
    /// space — the parser ignores them, mirroring the behaviour of
    /// every other FullBox-headed property parser in this module.
    #[test]
    fn wbbr_tolerates_trailing_bytes() {
        let mut body = wbbr_body(5600, -3);
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let w = parse_wbbr(&body).unwrap();
        assert_eq!(w.blue_amber, 5600);
        assert_eq!(w.green_magenta, -3);
    }

    #[test]
    fn wbbr_dispatched_through_parse_ipco() {
        let body = wbbr_body(6500, -7);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"wbbr");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Wbbr(w) => {
                assert_eq!(w.blue_amber, 6500);
                assert_eq!(w.green_magenta, -7);
            }
            other => panic!("expected Wbbr, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"wbbr");
    }

    /// A recognised `wbbr` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser does
    /// not reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn wbbr_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Wbbr(Wbbr::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.23.1 caps `wbbr` at one per item (`At most one`). A
    /// `Meta::property_for(item_id, &WBBR)` lookup finds the
    /// associated single instance via the standard `ipma` walk;
    /// the dispatch returns the `Wbbr` variant which the caller
    /// pattern matches on. This test exercises the typical lookup
    /// shape end to end.
    #[test]
    fn wbbr_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Wbbr(Wbbr {
                blue_amber: 4800,
                green_magenta: 25,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 9,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(9, b"wbbr") {
            Some(Property::Wbbr(w)) => {
                assert_eq!(w.blue_amber, 4800);
                assert_eq!(w.green_magenta, 25);
                assert_eq!(w.green_magenta_duv(), 0.25);
                assert!(!w.is_green_magenta_neutral());
            }
            other => panic!("expected Wbbr, got {other:?}"),
        }
        // No `wbbr` for an item that doesn't carry the association.
        assert!(m.property_for(99, b"wbbr").is_none());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.24 FocusProperty (`fobr`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build a `fobr` body — FullBox(v=0, f=0) followed by two
    /// `unsigned int(16)` fields (big-endian per ISO/IEC 14496-12
    /// §4.2) in the §6.5.24.2 declaration order:
    /// `focus_distance_numerator` then `focus_distance_denominator`.
    fn fobr_body(num: u16, den: u16) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&num.to_be_bytes());
        buf.extend_from_slice(&den.to_be_bytes());
        buf
    }

    #[test]
    fn fobr_round_trip_reads_numerator_then_denominator() {
        // Distinct, asymmetric values per field so a cross-wire
        // between numerator and denominator would surface
        // immediately. 17 / 10 expresses 1.7 m, a realistic
        // mid-range portrait focus distance.
        let f = parse_fobr(&fobr_body(17, 10)).unwrap();
        assert_eq!(f.focus_distance_numerator, 17);
        assert_eq!(f.focus_distance_denominator, 10);
    }

    /// Both fields are `unsigned int(16)` and are read big-endian
    /// per ISO/IEC 14496-12 §4.2. A value that requires the high
    /// byte to be non-zero (`0x0125` = 293) pins the byte order
    /// against a little-endian regression that would surface as
    /// `0x2501` = 9473 — easily distinguished from the intended
    /// reading.
    #[test]
    fn fobr_fields_are_big_endian() {
        let f = parse_fobr(&fobr_body(0x0125, 0x0008)).unwrap();
        assert_eq!(f.focus_distance_numerator, 0x0125);
        assert_eq!(f.focus_distance_denominator, 0x0008);
        // Endpoint coverage: u16::MAX exercises both high bits.
        let f = parse_fobr(&fobr_body(u16::MAX, u16::MAX)).unwrap();
        assert_eq!(f.focus_distance_numerator, u16::MAX);
        assert_eq!(f.focus_distance_denominator, u16::MAX);
        // Strict-infinity sentinel: both fields zero.
        let f = parse_fobr(&fobr_body(0, 0)).unwrap();
        assert_eq!(f.focus_distance_numerator, 0);
        assert_eq!(f.focus_distance_denominator, 0);
    }

    /// §6.5.24.3 projection: focus distance in metres is
    /// `focus_distance_numerator / focus_distance_denominator`.
    /// The helper returns `Some(metres)` for a well-formed
    /// denominator and `None` for the infinity sentinel
    /// (denominator zero).
    #[test]
    fn fobr_focus_distance_metres_projection() {
        // 17 / 10 = 1.7 m (portrait range).
        let f = parse_fobr(&fobr_body(17, 10)).unwrap();
        assert_eq!(f.focus_distance_metres(), Some(1.7));
        // 1 / 1 = 1.0 m.
        let f = parse_fobr(&fobr_body(1, 1)).unwrap();
        assert_eq!(f.focus_distance_metres(), Some(1.0));
        // 5 / 100 = 0.05 m (macro range).
        let f = parse_fobr(&fobr_body(5, 100)).unwrap();
        assert_eq!(f.focus_distance_metres(), Some(0.05));
        // u16::MAX / 1 — extreme but representable on the wire.
        let f = parse_fobr(&fobr_body(u16::MAX, 1)).unwrap();
        assert_eq!(f.focus_distance_metres(), Some(f64::from(u16::MAX)));
        // §6.5.24.3 infinity sentinel: denominator zero.
        let f = parse_fobr(&fobr_body(0, 0)).unwrap();
        assert_eq!(f.focus_distance_metres(), None);
        // §6.5.24.3 NOTE: the writer `should` zero the numerator
        // too, but the helper returns `None` whenever the
        // denominator is zero (the `i.e.` clause is the
        // load-bearing predicate).
        let f = parse_fobr(&fobr_body(42, 0)).unwrap();
        assert_eq!(f.focus_distance_metres(), None);
    }

    /// §6.5.24.3 infinity sentinel: `focus_distance_denominator ==
    /// 0` signals focus at infinity. The predicate flips on
    /// exactly that condition regardless of the numerator.
    #[test]
    fn fobr_is_focus_at_infinity_predicate() {
        let f = parse_fobr(&fobr_body(0, 0)).unwrap();
        assert!(f.is_focus_at_infinity());
        // Spec NOTE: numerator `should` be zero, but a non-zero
        // numerator still reads as infinity per the `i.e.` clause.
        let f = parse_fobr(&fobr_body(42, 0)).unwrap();
        assert!(f.is_focus_at_infinity());
        // Any non-zero denominator flips off the predicate.
        for den in [1u16, 10, 100, 1000, u16::MAX] {
            let f = parse_fobr(&fobr_body(17, den)).unwrap();
            assert!(
                !f.is_focus_at_infinity(),
                "denominator {den} must NOT be infinity"
            );
        }
        assert_eq!(Fobr::INFINITY_DENOMINATOR, 0);
    }

    /// §6.5.24.3 strict-infinity sentinel: BOTH numerator AND
    /// denominator zero. The stricter predicate distinguishes a
    /// "well-formed" infinity (writer honoured the `should`) from
    /// a denominator-only zero (still infinity per the `i.e.` but
    /// violates the `should`).
    #[test]
    fn fobr_well_formed_infinity_sentinel_predicate() {
        // Strict infinity: both zero.
        let f = parse_fobr(&fobr_body(0, 0)).unwrap();
        assert!(f.has_well_formed_infinity_sentinel());
        // Denominator-only zero: still infinity, but the writer
        // violated the §6.5.24.3 `should`.
        let f = parse_fobr(&fobr_body(1, 0)).unwrap();
        assert!(!f.has_well_formed_infinity_sentinel());
        let f = parse_fobr(&fobr_body(u16::MAX, 0)).unwrap();
        assert!(!f.has_well_formed_infinity_sentinel());
        // Numerator zero with a non-zero denominator: 0 / N = 0 m
        // (focus at the front element); not the infinity sentinel.
        let f = parse_fobr(&fobr_body(0, 1)).unwrap();
        assert!(!f.has_well_formed_infinity_sentinel());
        assert_eq!(f.focus_distance_metres(), Some(0.0));
        // Generic non-infinity.
        let f = parse_fobr(&fobr_body(17, 10)).unwrap();
        assert!(!f.has_well_formed_infinity_sentinel());
    }

    #[test]
    fn fobr_rejects_unknown_version() {
        // version=1, flags=0; four zero bytes would otherwise be a
        // well-formed v0 body.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&[0, 0, 0, 0]);
        assert!(parse_fobr(&buf).is_err());
    }

    /// A body shorter than the four-byte fixed tail must be
    /// rejected so a truncated `fobr` cannot be partially read.
    #[test]
    fn fobr_rejects_truncated_body() {
        // FullBox header alone — no fields at all.
        let buf = vec![0u8; 4];
        assert!(parse_fobr(&buf).is_err());
        // FullBox header + `focus_distance_numerator` only
        // (missing the denominator).
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&[0x00, 0x11]);
        assert!(parse_fobr(&buf).is_err());
        // FullBox header + numerator + one byte of denominator
        // (missing the second byte of the denominator).
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&[0x00, 0x11, 0x00]);
        assert!(parse_fobr(&buf).is_err());
    }

    /// Trailing bytes past the four-byte tail are forward-compat
    /// space — the parser ignores them, mirroring the behaviour of
    /// every other FullBox-headed property parser in this module.
    #[test]
    fn fobr_tolerates_trailing_bytes() {
        let mut body = fobr_body(17, 10);
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let f = parse_fobr(&body).unwrap();
        assert_eq!(f.focus_distance_numerator, 17);
        assert_eq!(f.focus_distance_denominator, 10);
        assert_eq!(f.focus_distance_metres(), Some(1.7));
    }

    #[test]
    fn fobr_dispatched_through_parse_ipco() {
        let body = fobr_body(7, 2);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"fobr");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Fobr(f) => {
                assert_eq!(f.focus_distance_numerator, 7);
                assert_eq!(f.focus_distance_denominator, 2);
                assert_eq!(f.focus_distance_metres(), Some(3.5));
            }
            other => panic!("expected Fobr, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"fobr");
    }

    /// A recognised `fobr` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser does
    /// not reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn fobr_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Fobr(Fobr::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.24.1 caps `fobr` at one per item (`At most one`). A
    /// `Meta::property_for(item_id, &FOBR)` lookup finds the
    /// associated single instance via the standard `ipma` walk;
    /// the dispatch returns the `Fobr` variant which the caller
    /// pattern matches on. This test exercises the typical lookup
    /// shape end to end.
    #[test]
    fn fobr_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Fobr(Fobr {
                focus_distance_numerator: 17,
                focus_distance_denominator: 10,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 9,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(9, b"fobr") {
            Some(Property::Fobr(f)) => {
                assert_eq!(f.focus_distance_numerator, 17);
                assert_eq!(f.focus_distance_denominator, 10);
                assert_eq!(f.focus_distance_metres(), Some(1.7));
                assert!(!f.is_focus_at_infinity());
            }
            other => panic!("expected Fobr, got {other:?}"),
        }
        // No `fobr` for an item that doesn't carry the association.
        assert!(m.property_for(99, b"fobr").is_none());

        // Infinity sentinel via `property_for`.
        let m = Meta {
            properties: vec![Property::Fobr(Fobr {
                focus_distance_numerator: 0,
                focus_distance_denominator: 0,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 5,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(5, b"fobr") {
            Some(Property::Fobr(f)) => {
                assert!(f.is_focus_at_infinity());
                assert!(f.has_well_formed_infinity_sentinel());
                assert_eq!(f.focus_distance_metres(), None);
            }
            other => panic!("expected Fobr, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.25 FlashExposureProperty (`afbr`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build an `afbr` body — FullBox(v=0, f=0) followed by two
    /// `int(8)` fields in the §6.5.25.2 declaration order:
    /// `flash_exposure_numerator` then `flash_exposure_denominator`.
    /// Accepts `i8` so the test call sites read in spec-text units
    /// (negative values for the dark direction of the bracket).
    fn afbr_body(num: i8, den: i8) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        buf.push(num as u8);
        buf.push(den as u8);
        buf
    }

    #[test]
    fn afbr_round_trip_reads_numerator_then_denominator() {
        // Distinct, asymmetric values per field so a cross-wire
        // between numerator and denominator would surface
        // immediately. 1 / 2 expresses +0.5 stops of flash, a
        // realistic mid-bracket position.
        let a = parse_afbr(&afbr_body(1, 2)).unwrap();
        assert_eq!(a.flash_exposure_numerator, 1);
        assert_eq!(a.flash_exposure_denominator, 2);
    }

    /// Both fields are `int(8)` (signed). A writer that emits `0xFF`
    /// for the smallest dark direction must round-trip to `-1`, not
    /// `255` — i.e. the parser must interpret the byte as `i8`.
    #[test]
    fn afbr_fields_are_signed() {
        // Lone negative numerator: -1 / +2 = -0.5 stops (under).
        let a = parse_afbr(&afbr_body(-1, 2)).unwrap();
        assert_eq!(a.flash_exposure_numerator, -1);
        assert_eq!(a.flash_exposure_denominator, 2);
        // Lone negative denominator: +1 / -2 = -0.5 stops (under,
        // expressed via the sign of the denominator).
        let a = parse_afbr(&afbr_body(1, -2)).unwrap();
        assert_eq!(a.flash_exposure_numerator, 1);
        assert_eq!(a.flash_exposure_denominator, -2);
        // Both negative: -1 / -2 = +0.5 stops (over).
        let a = parse_afbr(&afbr_body(-1, -2)).unwrap();
        assert_eq!(a.flash_exposure_numerator, -1);
        assert_eq!(a.flash_exposure_denominator, -2);
        // Signed endpoints: i8::MIN / i8::MAX = -128 / 127.
        let a = parse_afbr(&afbr_body(i8::MIN, i8::MAX)).unwrap();
        assert_eq!(a.flash_exposure_numerator, i8::MIN);
        assert_eq!(a.flash_exposure_denominator, i8::MAX);
        // The raw `0xFF` byte must read as `-1`, NOT `255` —
        // pins the `as i8` cast against a stray `as u8` regression.
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&[0xFF, 0x02]);
        let a = parse_afbr(&buf).unwrap();
        assert_eq!(a.flash_exposure_numerator, -1);
        assert_eq!(a.flash_exposure_denominator, 2);
    }

    /// §6.5.25.3 projection: flash exposure in number of f-stops is
    /// `flash_exposure_numerator / flash_exposure_denominator`.
    /// The helper returns `Some(stops)` for a well-formed denominator
    /// and `None` for the (spec-undefined) zero denominator.
    #[test]
    fn afbr_flash_exposure_stops_projection() {
        // 1 / 2 = +0.5 stops (half-stop over).
        let a = parse_afbr(&afbr_body(1, 2)).unwrap();
        assert_eq!(a.flash_exposure_stops(), Some(0.5));
        // -1 / 2 = -0.5 stops (half-stop under).
        let a = parse_afbr(&afbr_body(-1, 2)).unwrap();
        assert_eq!(a.flash_exposure_stops(), Some(-0.5));
        // 1 / 1 = +1.0 stop (full-stop over).
        let a = parse_afbr(&afbr_body(1, 1)).unwrap();
        assert_eq!(a.flash_exposure_stops(), Some(1.0));
        // -2 / 3 = -0.6667 stops (two-third-stop under).
        let a = parse_afbr(&afbr_body(-2, 3)).unwrap();
        let v = a.flash_exposure_stops().unwrap();
        assert!((v - (-2.0 / 3.0)).abs() < 1e-12, "got {v}");
        // i8::MIN / -1 must NOT integer-overflow — the f64 widening
        // gives 128.0 cleanly. This pins the `f64::from` widening
        // against a hypothetical `i8 / i8` integer-only divide
        // regression that would panic.
        let a = parse_afbr(&afbr_body(i8::MIN, -1)).unwrap();
        assert_eq!(a.flash_exposure_stops(), Some(128.0));
        // Zero denominator — mathematically undefined per the spec's
        // silence (no §6.5.25.3 sentinel carve-out unlike `fobr`'s
        // infinity reading); the helper returns `None`.
        let a = parse_afbr(&afbr_body(1, 0)).unwrap();
        assert_eq!(a.flash_exposure_stops(), None);
        // Zero numerator, non-zero denominator: 0 / N = 0.0 stops
        // (no flash variation relative to the camera setting).
        let a = parse_afbr(&afbr_body(0, 1)).unwrap();
        assert_eq!(a.flash_exposure_stops(), Some(0.0));
        // Zero / zero: still undefined (denominator-zero path).
        let a = parse_afbr(&afbr_body(0, 0)).unwrap();
        assert_eq!(a.flash_exposure_stops(), None);
    }

    #[test]
    fn afbr_rejects_unknown_version() {
        // version=1, flags=0; two zero bytes would otherwise be a
        // well-formed v0 body.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&[0, 0]);
        assert!(parse_afbr(&buf).is_err());
    }

    /// A body shorter than the two-byte fixed tail must be rejected
    /// so a truncated `afbr` cannot be partially read.
    #[test]
    fn afbr_rejects_truncated_body() {
        // FullBox header alone — no fields at all.
        let buf = vec![0u8; 4];
        assert!(parse_afbr(&buf).is_err());
        // FullBox header + `flash_exposure_numerator` only
        // (missing the denominator).
        let mut buf = vec![0u8; 4];
        buf.push(0x01);
        assert!(parse_afbr(&buf).is_err());
    }

    /// Trailing bytes past the two-byte tail are forward-compat
    /// space — the parser ignores them, mirroring the behaviour of
    /// every other FullBox-headed property parser in this module.
    #[test]
    fn afbr_tolerates_trailing_bytes() {
        let mut body = afbr_body(1, 2);
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let a = parse_afbr(&body).unwrap();
        assert_eq!(a.flash_exposure_numerator, 1);
        assert_eq!(a.flash_exposure_denominator, 2);
        assert_eq!(a.flash_exposure_stops(), Some(0.5));
    }

    #[test]
    fn afbr_dispatched_through_parse_ipco() {
        let body = afbr_body(-3, 4);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"afbr");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Afbr(a) => {
                assert_eq!(a.flash_exposure_numerator, -3);
                assert_eq!(a.flash_exposure_denominator, 4);
                assert_eq!(a.flash_exposure_stops(), Some(-0.75));
            }
            other => panic!("expected Afbr, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"afbr");
    }

    /// A recognised `afbr` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser does
    /// not reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn afbr_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Afbr(Afbr::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.25.1 caps `afbr` at one per item (`At most one`). A
    /// `Meta::property_for(item_id, &AFBR)` lookup finds the
    /// associated single instance via the standard `ipma` walk;
    /// the dispatch returns the `Afbr` variant which the caller
    /// pattern matches on. This test exercises the typical lookup
    /// shape end to end.
    #[test]
    fn afbr_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Afbr(Afbr {
                flash_exposure_numerator: 1,
                flash_exposure_denominator: 2,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 9,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(9, b"afbr") {
            Some(Property::Afbr(a)) => {
                assert_eq!(a.flash_exposure_numerator, 1);
                assert_eq!(a.flash_exposure_denominator, 2);
                assert_eq!(a.flash_exposure_stops(), Some(0.5));
            }
            other => panic!("expected Afbr, got {other:?}"),
        }
        // No `afbr` for an item that doesn't carry the association.
        assert!(m.property_for(99, b"afbr").is_none());

        // Negative bracket position via `property_for`.
        let m = Meta {
            properties: vec![Property::Afbr(Afbr {
                flash_exposure_numerator: -3,
                flash_exposure_denominator: 4,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 5,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(5, b"afbr") {
            Some(Property::Afbr(a)) => {
                assert_eq!(a.flash_exposure_numerator, -3);
                assert_eq!(a.flash_exposure_denominator, 4);
                assert_eq!(a.flash_exposure_stops(), Some(-0.75));
            }
            other => panic!("expected Afbr, got {other:?}"),
        }

        // Zero-denominator "undefined" reading: still typed as Afbr,
        // just with `flash_exposure_stops() == None`.
        let m = Meta {
            properties: vec![Property::Afbr(Afbr {
                flash_exposure_numerator: 1,
                flash_exposure_denominator: 0,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 7,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(7, b"afbr") {
            Some(Property::Afbr(a)) => {
                assert_eq!(a.flash_exposure_numerator, 1);
                assert_eq!(a.flash_exposure_denominator, 0);
                assert_eq!(a.flash_exposure_stops(), None);
            }
            other => panic!("expected Afbr, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.26 DepthOfFieldProperty (`dobr`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build a `dobr` body — FullBox(v=0, f=0) followed by two
    /// `int(8)` fields in the §6.5.26.2 declaration order:
    /// `f_stop_numerator` then `f_stop_denominator`. Accepts `i8` so
    /// the test call sites read in spec-text units (negative values
    /// for the shallow direction of the bracket).
    fn dobr_body(num: i8, den: i8) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        buf.push(num as u8);
        buf.push(den as u8);
        buf
    }

    #[test]
    fn dobr_round_trip_reads_numerator_then_denominator() {
        // Distinct, asymmetric values per field so a cross-wire
        // between numerator and denominator would surface
        // immediately. 1 / 2 expresses +0.5 stops of aperture change,
        // a realistic mid-bracket position.
        let d = parse_dobr(&dobr_body(1, 2)).unwrap();
        assert_eq!(d.f_stop_numerator, 1);
        assert_eq!(d.f_stop_denominator, 2);
    }

    /// Both fields are `int(8)` (signed). A writer that emits `0xFF`
    /// for the smallest shallow direction must round-trip to `-1`, not
    /// `255` — i.e. the parser must interpret the byte as `i8`.
    #[test]
    fn dobr_fields_are_signed() {
        // Lone negative numerator: -1 / +2 = -0.5 stops (shallower).
        let d = parse_dobr(&dobr_body(-1, 2)).unwrap();
        assert_eq!(d.f_stop_numerator, -1);
        assert_eq!(d.f_stop_denominator, 2);
        // Lone negative denominator: +1 / -2 = -0.5 stops (shallower,
        // expressed via the sign of the denominator).
        let d = parse_dobr(&dobr_body(1, -2)).unwrap();
        assert_eq!(d.f_stop_numerator, 1);
        assert_eq!(d.f_stop_denominator, -2);
        // Both negative: -1 / -2 = +0.5 stops (deeper).
        let d = parse_dobr(&dobr_body(-1, -2)).unwrap();
        assert_eq!(d.f_stop_numerator, -1);
        assert_eq!(d.f_stop_denominator, -2);
        // Signed endpoints: i8::MIN / i8::MAX = -128 / 127.
        let d = parse_dobr(&dobr_body(i8::MIN, i8::MAX)).unwrap();
        assert_eq!(d.f_stop_numerator, i8::MIN);
        assert_eq!(d.f_stop_denominator, i8::MAX);
        // The raw `0xFF` byte must read as `-1`, NOT `255` —
        // pins the `as i8` cast against a stray `as u8` regression.
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&[0xFF, 0x02]);
        let d = parse_dobr(&buf).unwrap();
        assert_eq!(d.f_stop_numerator, -1);
        assert_eq!(d.f_stop_denominator, 2);
    }

    /// §6.5.26.3 projection: the depth-of-field variation as an
    /// aperture change in number of stops is `f_stop_numerator /
    /// f_stop_denominator`. The helper returns `Some(stops)` for a
    /// well-formed denominator and `None` for the (spec-undefined)
    /// zero denominator.
    #[test]
    fn dobr_aperture_stops_projection() {
        // 1 / 2 = +0.5 stops (half-stop deeper).
        let d = parse_dobr(&dobr_body(1, 2)).unwrap();
        assert_eq!(d.aperture_stops(), Some(0.5));
        // -1 / 2 = -0.5 stops (half-stop shallower).
        let d = parse_dobr(&dobr_body(-1, 2)).unwrap();
        assert_eq!(d.aperture_stops(), Some(-0.5));
        // 1 / 1 = +1.0 stop (full-stop deeper).
        let d = parse_dobr(&dobr_body(1, 1)).unwrap();
        assert_eq!(d.aperture_stops(), Some(1.0));
        // -2 / 3 = -0.6667 stops (two-third-stop shallower).
        let d = parse_dobr(&dobr_body(-2, 3)).unwrap();
        let v = d.aperture_stops().unwrap();
        assert!((v - (-2.0 / 3.0)).abs() < 1e-12, "got {v}");
        // i8::MIN / -1 must NOT integer-overflow — the f64 widening
        // gives 128.0 cleanly. This pins the `f64::from` widening
        // against a hypothetical `i8 / i8` integer-only divide
        // regression that would panic.
        let d = parse_dobr(&dobr_body(i8::MIN, -1)).unwrap();
        assert_eq!(d.aperture_stops(), Some(128.0));
        // Zero denominator — mathematically undefined per the spec's
        // silence (no §6.5.26.3 sentinel carve-out unlike `fobr`'s
        // infinity reading); the helper returns `None`.
        let d = parse_dobr(&dobr_body(1, 0)).unwrap();
        assert_eq!(d.aperture_stops(), None);
        // Zero numerator, non-zero denominator: 0 / N = 0.0 stops
        // (no aperture variation relative to the camera setting).
        let d = parse_dobr(&dobr_body(0, 1)).unwrap();
        assert_eq!(d.aperture_stops(), Some(0.0));
        // Zero / zero: still undefined (denominator-zero path).
        let d = parse_dobr(&dobr_body(0, 0)).unwrap();
        assert_eq!(d.aperture_stops(), None);
    }

    #[test]
    fn dobr_rejects_unknown_version() {
        // version=1, flags=0; two zero bytes would otherwise be a
        // well-formed v0 body.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&[0, 0]);
        assert!(parse_dobr(&buf).is_err());
    }

    /// A body shorter than the two-byte fixed tail must be rejected
    /// so a truncated `dobr` cannot be partially read.
    #[test]
    fn dobr_rejects_truncated_body() {
        // FullBox header alone — no fields at all.
        let buf = vec![0u8; 4];
        assert!(parse_dobr(&buf).is_err());
        // FullBox header + `f_stop_numerator` only
        // (missing the denominator).
        let mut buf = vec![0u8; 4];
        buf.push(0x01);
        assert!(parse_dobr(&buf).is_err());
    }

    /// Trailing bytes past the two-byte tail are forward-compat
    /// space — the parser ignores them, mirroring the behaviour of
    /// every other FullBox-headed property parser in this module.
    #[test]
    fn dobr_tolerates_trailing_bytes() {
        let mut body = dobr_body(1, 2);
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let d = parse_dobr(&body).unwrap();
        assert_eq!(d.f_stop_numerator, 1);
        assert_eq!(d.f_stop_denominator, 2);
        assert_eq!(d.aperture_stops(), Some(0.5));
    }

    #[test]
    fn dobr_dispatched_through_parse_ipco() {
        let body = dobr_body(-3, 4);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"dobr");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Dobr(d) => {
                assert_eq!(d.f_stop_numerator, -3);
                assert_eq!(d.f_stop_denominator, 4);
                assert_eq!(d.aperture_stops(), Some(-0.75));
            }
            other => panic!("expected Dobr, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"dobr");
    }

    /// A recognised `dobr` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser does
    /// not reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn dobr_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Dobr(Dobr::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.26.1 caps `dobr` at one per item (`At most one`). A
    /// `Meta::property_for(item_id, &DOBR)` lookup finds the
    /// associated single instance via the standard `ipma` walk;
    /// the dispatch returns the `Dobr` variant which the caller
    /// pattern matches on. This test exercises the typical lookup
    /// shape end to end.
    #[test]
    fn dobr_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Dobr(Dobr {
                f_stop_numerator: 1,
                f_stop_denominator: 2,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 9,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(9, b"dobr") {
            Some(Property::Dobr(d)) => {
                assert_eq!(d.f_stop_numerator, 1);
                assert_eq!(d.f_stop_denominator, 2);
                assert_eq!(d.aperture_stops(), Some(0.5));
            }
            other => panic!("expected Dobr, got {other:?}"),
        }
        // No `dobr` for an item that doesn't carry the association.
        assert!(m.property_for(99, b"dobr").is_none());

        // Negative bracket position via `property_for`.
        let m = Meta {
            properties: vec![Property::Dobr(Dobr {
                f_stop_numerator: -3,
                f_stop_denominator: 4,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 5,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(5, b"dobr") {
            Some(Property::Dobr(d)) => {
                assert_eq!(d.f_stop_numerator, -3);
                assert_eq!(d.f_stop_denominator, 4);
                assert_eq!(d.aperture_stops(), Some(-0.75));
            }
            other => panic!("expected Dobr, got {other:?}"),
        }

        // Zero-denominator "undefined" reading: still typed as Dobr,
        // just with `aperture_stops() == None`.
        let m = Meta {
            properties: vec![Property::Dobr(Dobr {
                f_stop_numerator: 1,
                f_stop_denominator: 0,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 7,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(7, b"dobr") {
            Some(Property::Dobr(d)) => {
                assert_eq!(d.f_stop_numerator, 1);
                assert_eq!(d.f_stop_denominator, 0);
                assert_eq!(d.aperture_stops(), None);
            }
            other => panic!("expected Dobr, got {other:?}"),
        }

        // `dobr` may legally co-occur with `afbr` on the same item
        // (different §6.5 properties); both resolve independently.
        let m = Meta {
            properties: vec![
                Property::Dobr(Dobr {
                    f_stop_numerator: 1,
                    f_stop_denominator: 2,
                }),
                Property::Afbr(Afbr {
                    flash_exposure_numerator: -1,
                    flash_exposure_denominator: 2,
                }),
            ],
            associations: vec![ItemPropertyAssociation {
                item_id: 3,
                entries: vec![
                    PropertyAssociation {
                        index: 0,
                        essential: false,
                    },
                    PropertyAssociation {
                        index: 1,
                        essential: false,
                    },
                ],
            }],
            ..Meta::default()
        };
        match m.property_for(3, b"dobr") {
            Some(Property::Dobr(d)) => assert_eq!(d.aperture_stops(), Some(0.5)),
            other => panic!("expected Dobr, got {other:?}"),
        }
        match m.property_for(3, b"afbr") {
            Some(Property::Afbr(a)) => assert_eq!(a.flash_exposure_stops(), Some(-0.5)),
            other => panic!("expected Afbr, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.40 CameraIntrinsicMatrixProperty (`cmin`) — parse +
    // helpers
    // -----------------------------------------------------------------

    /// Build a `cmin` body — FullBox(v=0, flags) followed by the three
    /// mandatory `signed int(32)` fields, and, when the full-intrinsics
    /// `flags & 1` bit is set, the two-field tail. The `flags` value is
    /// packed into the low three bytes of the FullBox header.
    fn cmin_body(flags: u32, fx: i32, cx: i32, cy: i32, tail: Option<(i32, i32)>) -> Vec<u8> {
        let mut buf = vec![0u8]; // version = 0
        buf.extend_from_slice(&flags.to_be_bytes()[1..]); // 24-bit flags
        buf.extend_from_slice(&(fx as u32).to_be_bytes());
        buf.extend_from_slice(&(cx as u32).to_be_bytes());
        buf.extend_from_slice(&(cy as u32).to_be_bytes());
        if let Some((fy, skew)) = tail {
            buf.extend_from_slice(&(fy as u32).to_be_bytes());
            buf.extend_from_slice(&(skew as u32).to_be_bytes());
        }
        buf
    }

    /// The simplified form (`flags & 1 == 0`) reads exactly the three
    /// mandatory fields and leaves `focal_length_y` / `skew_factor`
    /// absent. Distinct, asymmetric values per field so a cross-wire
    /// between them would surface immediately.
    #[test]
    fn cmin_simplified_reads_three_mandatory_fields() {
        let c = parse_cmin(&cmin_body(0, 100, 20, 30, None)).unwrap();
        assert_eq!(c.focal_length_x, 100);
        assert_eq!(c.principal_point_x, 20);
        assert_eq!(c.principal_point_y, 30);
        assert_eq!(c.focal_length_y, None);
        assert_eq!(c.skew_factor, None);
        assert!(!c.has_skew());
    }

    /// The full form (`flags & 1 == 1`) appends `focal_length_y` and
    /// `skew_factor` after the three mandatory fields, in the
    /// §6.5.40.2 declaration order.
    #[test]
    fn cmin_full_reads_focal_y_then_skew() {
        let c = parse_cmin(&cmin_body(1, 100, 20, 30, Some((110, 5)))).unwrap();
        assert_eq!(c.focal_length_x, 100);
        assert_eq!(c.principal_point_x, 20);
        assert_eq!(c.principal_point_y, 30);
        assert_eq!(c.focal_length_y, Some(110));
        assert_eq!(c.skew_factor, Some(5));
        assert!(c.has_skew());
    }

    /// All five fields are `signed int(32)`. A writer that emits a
    /// negative principal point (off-centre optical axis) or a negative
    /// skew must round-trip through the `as i32` cast — not wrap to a
    /// large unsigned value.
    #[test]
    fn cmin_fields_are_signed() {
        let c = parse_cmin(&cmin_body(1, -1, -20, -30, Some((-110, -5)))).unwrap();
        assert_eq!(c.focal_length_x, -1);
        assert_eq!(c.principal_point_x, -20);
        assert_eq!(c.principal_point_y, -30);
        assert_eq!(c.focal_length_y, Some(-110));
        assert_eq!(c.skew_factor, Some(-5));
        // Signed endpoints round-trip.
        let c = parse_cmin(&cmin_body(
            1,
            i32::MIN,
            i32::MAX,
            0,
            Some((i32::MIN, i32::MAX)),
        ))
        .unwrap();
        assert_eq!(c.focal_length_x, i32::MIN);
        assert_eq!(c.principal_point_x, i32::MAX);
        assert_eq!(c.focal_length_y, Some(i32::MIN));
        assert_eq!(c.skew_factor, Some(i32::MAX));
    }

    /// §6.5.40.1 — `denominator = 1 << ((flags & 0x001F00) >> 8)` and
    /// `skewDenominator = 1 << ((flags & 0x1F0000) >> 16)`. The shift
    /// operands are decoded from the two 5-bit flag sub-fields.
    #[test]
    fn cmin_denominators_decoded_from_flags() {
        // denominatorShiftOperand = 8 → denominator = 256;
        // skewDenominatorShiftOperand = 4 → skewDenominator = 16;
        // low bit set for the full form.
        let flags = (8u32 << 8) | (4u32 << 16) | 1;
        let c = parse_cmin(&cmin_body(flags, 0, 0, 0, Some((0, 0)))).unwrap();
        assert_eq!(c.denominator_shift(), 8);
        assert_eq!(c.skew_denominator_shift(), 4);
        assert_eq!(c.denominator(), 256);
        assert_eq!(c.skew_denominator(), 16);
        // A zero shift operand means denominator == 1 (no scaling).
        let c = parse_cmin(&cmin_body(0, 0, 0, 0, None)).unwrap();
        assert_eq!(c.denominator(), 1);
        assert_eq!(c.skew_denominator(), 1);
        // The maximum 5-bit shift operand (31) is representable in u64.
        let flags = (31u32 << 8) | (31u32 << 16);
        let c = parse_cmin(&cmin_body(flags, 0, 0, 0, None)).unwrap();
        assert_eq!(c.denominator(), 1u64 << 31);
        assert_eq!(c.skew_denominator(), 1u64 << 31);
    }

    /// §6.5.40.1 matrix-entry projections fold in the `image_width` /
    /// `image_height` from the associated `ispe` and the flag-derived
    /// denominator. `fx = focal_length_x × width / denominator`, etc.,
    /// using floating-point division per NOTE 3.
    #[test]
    fn cmin_matrix_entry_projections() {
        // denominatorShiftOperand = 8 → denominator = 256; full form.
        let flags = (8u32 << 8) | 1;
        // fx = 256 × 640 / 256 = 640; cx = 128 × 640 / 256 = 320;
        // cy = 96 × 480 / 256 = 180; fy = 256 × 480 / 256 = 480;
        // s = 0 / 1 = 0.
        let c = parse_cmin(&cmin_body(flags, 256, 128, 96, Some((256, 0)))).unwrap();
        assert!((c.focal_length_x_value(640) - 640.0).abs() < 1e-9);
        assert!((c.focal_length_y_value(640, 480) - 480.0).abs() < 1e-9);
        assert!((c.principal_point_x_value(640) - 320.0).abs() < 1e-9);
        assert!((c.principal_point_y_value(480) - 180.0).abs() < 1e-9);
        assert!((c.skew_value() - 0.0).abs() < 1e-12);
    }

    /// §6.5.40.3 inferences for the simplified form: when
    /// `focal_length_y` is absent `fy` is inferred equal to `fx`, and
    /// when `skew_factor` is absent `s` is inferred to be `0`.
    #[test]
    fn cmin_simplified_infers_fy_equals_fx_and_zero_skew() {
        // denominator = 1; fx = 100 × 640 / 1 = 64000.
        let c = parse_cmin(&cmin_body(0, 100, 0, 0, None)).unwrap();
        let fx = c.focal_length_x_value(640);
        // fy inferred equal to fx even though image_height differs.
        assert!((c.focal_length_y_value(640, 480) - fx).abs() < 1e-9);
        // s inferred to be 0 when skew_factor is absent.
        assert_eq!(c.skew_value(), 0.0);
    }

    /// §6.5.40.1 skew formula `s = skew_factor / skewDenominator`. A
    /// negative skew (non-square pixels with a shear) round-trips.
    #[test]
    fn cmin_skew_value_uses_skew_denominator() {
        // skewDenominatorShiftOperand = 2 → skewDenominator = 4; full.
        let flags = (2u32 << 16) | 1;
        let c = parse_cmin(&cmin_body(flags, 0, 0, 0, Some((0, 6)))).unwrap();
        assert!((c.skew_value() - 1.5).abs() < 1e-12);
        let c = parse_cmin(&cmin_body(flags, 0, 0, 0, Some((0, -6)))).unwrap();
        assert!((c.skew_value() - (-1.5)).abs() < 1e-12);
    }

    #[test]
    fn cmin_rejects_unknown_version() {
        // version=1 with an otherwise well-formed v0 simplified body.
        let mut buf = cmin_body(0, 1, 2, 3, None);
        buf[0] = 1;
        assert!(parse_cmin(&buf).is_err());
    }

    /// A body shorter than the three mandatory 32-bit fields is
    /// rejected; for the full form the parser additionally requires the
    /// two-field tail (a truncated tail must not be partially read).
    #[test]
    fn cmin_rejects_truncated_body() {
        // FullBox header alone — no fields.
        let buf = vec![0u8; 4];
        assert!(parse_cmin(&buf).is_err());
        // Header + only two of the three mandatory fields.
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&[0u8; 8]);
        assert!(parse_cmin(&buf).is_err());
        // Full form flagged but the tail is missing (only the three
        // mandatory fields present).
        let buf = cmin_body(0, 1, 2, 3, None);
        let mut buf_full = buf.clone();
        buf_full[3] = 1; // set flags low bit → full form
        assert!(parse_cmin(&buf_full).is_err());
        // Full form with only one of the two tail fields.
        let mut buf = cmin_body(1, 1, 2, 3, Some((4, 5)));
        buf.truncate(buf.len() - 4); // drop skew_factor
        assert!(parse_cmin(&buf).is_err());
    }

    /// Trailing bytes past the parsed fields are forward-compat space —
    /// ignored by the parser, mirroring every other FullBox-headed
    /// property parser in this module.
    #[test]
    fn cmin_tolerates_trailing_bytes() {
        let mut body = cmin_body(1, 100, 20, 30, Some((110, 5)));
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let c = parse_cmin(&body).unwrap();
        assert_eq!(c.focal_length_x, 100);
        assert_eq!(c.skew_factor, Some(5));
    }

    #[test]
    fn cmin_dispatched_through_parse_ipco() {
        let body = cmin_body(1, 256, 128, 96, Some((256, 0)));
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"cmin");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Cmin(c) => {
                assert_eq!(c.focal_length_x, 256);
                assert_eq!(c.focal_length_y, Some(256));
                assert_eq!(c.skew_factor, Some(0));
            }
            other => panic!("expected Cmin, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"cmin");
    }

    /// A recognised `cmin` property — even when flagged essential —
    /// does NOT trip [`Meta::unsupported_essential_properties`].
    #[test]
    fn cmin_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Cmin(Cmin::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.40.1 caps `cmin` at one per item (`At most one`). A
    /// `Meta::property_for(item_id, b"cmin")` lookup finds the
    /// associated single instance via the standard `ipma` walk.
    #[test]
    fn cmin_lookup_via_property_for() {
        let flags = (8u32 << 8) | 1;
        let m = Meta {
            properties: vec![Property::Cmin(Cmin {
                flags,
                focal_length_x: 256,
                principal_point_x: 128,
                principal_point_y: 96,
                focal_length_y: Some(256),
                skew_factor: Some(0),
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 9,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(9, b"cmin") {
            Some(Property::Cmin(c)) => {
                assert_eq!(c.denominator(), 256);
                assert!((c.focal_length_x_value(640) - 640.0).abs() < 1e-9);
            }
            other => panic!("expected Cmin, got {other:?}"),
        }
        assert!(m.property_for(99, b"cmin").is_none());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.39 CameraExtrinsicMatrixProperty (`cmex`) — parse +
    // helpers
    // -----------------------------------------------------------------

    /// Build a `cmex` body — FullBox(version, flags) followed by the
    /// present fields in §6.5.39.2 order. Positions are `signed
    /// int(32)`; the quaternion triplet is `signed int(16)` unless
    /// `rot_large_field_size` (bit 4) is set, in which case it is
    /// `signed int(32)`. `id` is `unsigned int(32)`.
    fn cmex_body(
        version: u8,
        flags: u32,
        pos: [Option<i32>; 3],
        quat: Option<[i32; 3]>,
        id: Option<u32>,
    ) -> Vec<u8> {
        let mut buf = vec![version];
        buf.extend_from_slice(&flags.to_be_bytes()[1..]); // 24-bit flags
        for p in pos.into_iter().flatten() {
            buf.extend_from_slice(&(p as u32).to_be_bytes());
        }
        if let Some(q) = quat {
            let large = flags & Cmex::FLAG_ROT_LARGE_FIELD_SIZE != 0;
            for c in q {
                if large {
                    buf.extend_from_slice(&(c as u32).to_be_bytes());
                } else {
                    buf.extend_from_slice(&(c as i16).to_be_bytes());
                }
            }
        }
        if let Some(i) = id {
            buf.extend_from_slice(&i.to_be_bytes());
        }
        buf
    }

    /// Stereo-pair use-case (§6.5.39.1): only `pos_x_present`. Reads the
    /// one position field, leaves all other fields absent (inferred 0).
    #[test]
    fn cmex_position_x_only() {
        let flags = Cmex::FLAG_POS_X_PRESENT;
        let c = parse_cmex(&cmex_body(0, flags, [Some(65_000), None, None], None, None)).unwrap();
        assert_eq!(c.pos_x, Some(65_000));
        assert_eq!(c.pos_y, None);
        assert_eq!(c.pos_z, None);
        assert_eq!(c.position(), [65_000, 0, 0]);
        assert!(c.pos_x_present());
        assert!(!c.orientation_present());
        assert_eq!(c.quaternion(), None);
    }

    /// All three positions present read in x, y, z order (§6.5.39.2),
    /// each `signed int(32)` in µm; negatives round-trip.
    #[test]
    fn cmex_positions_signed_in_order() {
        let flags = Cmex::FLAG_POS_X_PRESENT | Cmex::FLAG_POS_Y_PRESENT | Cmex::FLAG_POS_Z_PRESENT;
        let c = parse_cmex(&cmex_body(
            0,
            flags,
            [Some(-1), Some(2), Some(i32::MIN)],
            None,
            None,
        ))
        .unwrap();
        assert_eq!(c.position(), [-1, 2, i32::MIN]);
    }

    /// 16-bit quaternion (default field size): `quat_x/y/z` are signed
    /// 16-bit, scaled by `2^14`. Verify the §6.5.39.1 normalisation and
    /// the derived `qW = sqrt(1 - (qX^2+qY^2+qZ^2))`.
    #[test]
    fn cmex_quaternion_16bit_identity() {
        // Identity orientation: quat_x=quat_y=quat_z=0 → qW = 1.
        let flags = Cmex::FLAG_ORIENTATION_PRESENT;
        let c = parse_cmex(&cmex_body(0, flags, [None; 3], Some([0, 0, 0]), None)).unwrap();
        assert_eq!(c.orientation_precision(), 0);
        let q = c.quaternion().unwrap();
        assert!((q[0]).abs() < 1e-12);
        assert!((q[3] - 1.0).abs() < 1e-12);
    }

    /// A 90° rotation about z is `qZ = sin(45°) ≈ 0.7071`, `qW ≈
    /// 0.7071`. With the 2^14 scale, `quat_z = round(0.70710678 * 16384)
    /// = 11585`. Verify the quaternion and the §6.5.39.1 `RC` rotation
    /// matrix (which should swap x/y axes with a sign).
    #[test]
    fn cmex_quaternion_rotation_matrix_z_90deg() {
        let flags = Cmex::FLAG_ORIENTATION_PRESENT;
        let qz = 11585; // round(sin(45°) * 2^14)
        let c = parse_cmex(&cmex_body(0, flags, [None; 3], Some([0, 0, qz]), None)).unwrap();
        let q = c.quaternion().unwrap();
        let inv_sqrt2 = std::f64::consts::FRAC_1_SQRT_2;
        assert!((q[2] - inv_sqrt2).abs() < 1e-3);
        assert!((q[3] - inv_sqrt2).abs() < 1e-3);
        let r = c.rotation_matrix().unwrap();
        // RC for a +90° rotation about z: [[0,-1,0],[1,0,0],[0,0,1]].
        assert!((r[0][0]).abs() < 1e-3);
        assert!((r[0][1] - (-1.0)).abs() < 1e-3);
        assert!((r[1][0] - 1.0).abs() < 1e-3);
        assert!((r[2][2] - 1.0).abs() < 1e-3);
    }

    /// `rot_large_field_size` selects 32-bit quaternion elements and
    /// raises `orientationPrecision` to 16, so the scale is `2^30`.
    #[test]
    fn cmex_quaternion_32bit_large_field() {
        let flags = Cmex::FLAG_ORIENTATION_PRESENT | Cmex::FLAG_ROT_LARGE_FIELD_SIZE;
        // qZ = 0.5 → quat_z = round(0.5 * 2^30) = 536870912.
        let qz = 536_870_912;
        let c = parse_cmex(&cmex_body(0, flags, [None; 3], Some([0, 0, qz]), None)).unwrap();
        assert_eq!(c.orientation_precision(), 16);
        assert!(c.rot_large_field_size());
        let q = c.quaternion().unwrap();
        assert!((q[2] - 0.5).abs() < 1e-9);
        // qW = sqrt(1 - 0.25) = sqrt(0.75).
        assert!((q[3] - 0.75_f64.sqrt()).abs() < 1e-9);
    }

    /// Full 3D-mapping use-case: all three positions + orientation + id,
    /// read in §6.5.39.2 order. Confirms the `id` lands after the
    /// quaternion (byte-offset bookkeeping is correct).
    #[test]
    fn cmex_all_fields_with_id() {
        let flags = Cmex::FLAG_POS_X_PRESENT
            | Cmex::FLAG_POS_Y_PRESENT
            | Cmex::FLAG_POS_Z_PRESENT
            | Cmex::FLAG_ORIENTATION_PRESENT
            | Cmex::FLAG_ID_PRESENT;
        let c = parse_cmex(&cmex_body(
            0,
            flags,
            [Some(10), Some(20), Some(30)],
            Some([0, 0, 0]),
            Some(7),
        ))
        .unwrap();
        assert_eq!(c.position(), [10, 20, 30]);
        assert_eq!(c.id, Some(7));
        assert_eq!(c.coordinate_system_id(), 7);
        assert!((c.quaternion().unwrap()[3] - 1.0).abs() < 1e-12);
    }

    /// `id_present` without orientation reads the `id` immediately after
    /// the positions (no quaternion bytes in between).
    #[test]
    fn cmex_id_after_positions_no_orientation() {
        let flags = Cmex::FLAG_POS_X_PRESENT | Cmex::FLAG_ID_PRESENT;
        let c = parse_cmex(&cmex_body(0, flags, [Some(42), None, None], None, Some(99))).unwrap();
        assert_eq!(c.pos_x, Some(42));
        assert_eq!(c.id, Some(99));
    }

    /// `qW` is clamped to a non-negative radicand: a quaternion whose
    /// signalled components slightly exceed the unit sphere (allowed by
    /// rounding) yields `qW = 0`, never a NaN.
    #[test]
    fn cmex_quaternion_qw_non_negative() {
        let flags = Cmex::FLAG_ORIENTATION_PRESENT;
        // qX = qY = qZ = 1.0 (quat = 2^14 each) → radicand 1-3 < 0.
        let c = parse_cmex(&cmex_body(
            0,
            flags,
            [None; 3],
            Some([16_384, 16_384, 16_384]),
            None,
        ))
        .unwrap();
        let q = c.quaternion().unwrap();
        assert!(q[3].is_finite());
        assert_eq!(q[3], 0.0);
    }

    /// version 1 with orientation signals a
    /// ViewpointGlobalCoordinateSysRotationStruct (ISO/IEC 23090-7,
    /// outside the allow-list); the parser refuses rather than guess the
    /// struct length.
    #[test]
    fn cmex_v1_orientation_unsupported() {
        let flags = Cmex::FLAG_ORIENTATION_PRESENT;
        let body = cmex_body(1, flags, [None; 3], Some([0, 0, 0]), None);
        assert!(matches!(parse_cmex(&body), Err(Error::Unsupported(_))));
    }

    /// version 1 WITHOUT orientation (positions + id only) parses fine —
    /// none of those fields depend on the unparseable rotation struct.
    #[test]
    fn cmex_v1_positions_only_ok() {
        let flags = Cmex::FLAG_POS_X_PRESENT | Cmex::FLAG_ID_PRESENT;
        let c = parse_cmex(&cmex_body(1, flags, [Some(5), None, None], None, Some(3))).unwrap();
        assert_eq!(c.version, 1);
        assert_eq!(c.pos_x, Some(5));
        assert_eq!(c.id, Some(3));
        // No quaternion for version != 0.
        assert_eq!(c.quaternion(), None);
    }

    #[test]
    fn cmex_rejects_unknown_version() {
        let body = cmex_body(
            0,
            Cmex::FLAG_POS_X_PRESENT,
            [Some(1), None, None],
            None,
            None,
        );
        let mut buf = body;
        buf[0] = 2;
        assert!(parse_cmex(&buf).is_err());
    }

    /// A body that runs out before a flagged field is rejected (truncated
    /// reads never partially populate a field).
    #[test]
    fn cmex_rejects_truncated_body() {
        // pos_x flagged but no payload bytes.
        let buf = vec![0u8, 0, 0, Cmex::FLAG_POS_X_PRESENT as u8];
        assert!(parse_cmex(&buf).is_err());
        // 16-bit orientation flagged but only 4 of the 6 bytes present.
        let mut buf = vec![0u8, 0, 0, Cmex::FLAG_ORIENTATION_PRESENT as u8];
        buf.extend_from_slice(&[0u8; 4]);
        assert!(parse_cmex(&buf).is_err());
    }

    /// Trailing bytes past the parsed fields are ignored (forward-compat
    /// space), like the other FullBox-headed property parsers.
    #[test]
    fn cmex_tolerates_trailing_bytes() {
        let mut body = cmex_body(
            0,
            Cmex::FLAG_POS_X_PRESENT,
            [Some(7), None, None],
            None,
            None,
        );
        body.extend_from_slice(&[0xDE, 0xAD]);
        let c = parse_cmex(&body).unwrap();
        assert_eq!(c.pos_x, Some(7));
    }

    #[test]
    fn cmex_dispatched_through_parse_ipco() {
        let flags = Cmex::FLAG_POS_X_PRESENT | Cmex::FLAG_ORIENTATION_PRESENT;
        let body = cmex_body(0, flags, [Some(1234), None, None], Some([0, 0, 0]), None);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"cmex");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Cmex(c) => {
                assert_eq!(c.pos_x, Some(1234));
                assert!((c.quaternion().unwrap()[3] - 1.0).abs() < 1e-12);
            }
            other => panic!("expected Cmex, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"cmex");
    }

    /// A recognised `cmex` property — even when flagged essential — does
    /// NOT trip [`Meta::unsupported_essential_properties`].
    #[test]
    fn cmex_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Cmex(Cmex::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.39.1 allows zero-or-more `cmex` per item; `property_for`
    /// resolves an associated instance via the standard `ipma` walk.
    #[test]
    fn cmex_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Cmex(Cmex {
                flags: Cmex::FLAG_POS_X_PRESENT | Cmex::FLAG_ID_PRESENT,
                version: 0,
                pos_x: Some(500),
                id: Some(4),
                ..Cmex::default()
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 9,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(9, b"cmex") {
            Some(Property::Cmex(c)) => {
                assert_eq!(c.position(), [500, 0, 0]);
                assert_eq!(c.coordinate_system_id(), 4);
            }
            other => panic!("expected Cmex, got {other:?}"),
        }
        assert!(m.property_for(99, b"cmex").is_none());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.27 PanoramaProperty (`pano`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build a linear-direction `pano` body — FullBox(v=0, f=0)
    /// followed by the lone `unsigned int(8) panorama_direction` per
    /// the §6.5.27.2 syntax (the grid-shape bytes are conditionally
    /// absent for directions outside `4..=5`).
    fn pano_body_linear(direction: u8) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        buf.push(direction);
        buf
    }

    /// Build a grid-direction `pano` body — FullBox(v=0, f=0)
    /// followed by the three §6.5.27.2 bytes `panorama_direction`,
    /// `rows_minus_one`, `columns_minus_one`.
    fn pano_body_grid(direction: u8, rows_minus_one: u8, columns_minus_one: u8) -> Vec<u8> {
        let mut buf = pano_body_linear(direction);
        buf.push(rows_minus_one);
        buf.push(columns_minus_one);
        buf
    }

    /// The four linear directions (§6.5.27.3 values 0..=3) parse from
    /// a one-byte body with no grid shape attached.
    #[test]
    fn pano_linear_directions_have_no_grid() {
        for direction in [
            Pano::DIRECTION_LEFT_TO_RIGHT,
            Pano::DIRECTION_RIGHT_TO_LEFT,
            Pano::DIRECTION_BOTTOM_TO_TOP,
            Pano::DIRECTION_TOP_TO_BOTTOM,
        ] {
            let p = parse_pano(&pano_body_linear(direction)).unwrap();
            assert_eq!(p.panorama_direction, direction);
            assert_eq!(p.grid, None);
            assert!(p.is_defined_direction());
            assert!(!p.is_grid());
        }
    }

    /// The two grid directions (§6.5.27.3 values 4..=5) carry the
    /// conditional `rows_minus_one` / `columns_minus_one` pair, in
    /// that declaration order. Distinct asymmetric values per field so
    /// a rows/columns cross-wire would surface immediately.
    #[test]
    fn pano_grid_directions_carry_shape() {
        for direction in [Pano::DIRECTION_GRID_RASTER, Pano::DIRECTION_GRID_CONTINUOUS] {
            let p = parse_pano(&pano_body_grid(direction, 1, 2)).unwrap();
            assert_eq!(p.panorama_direction, direction);
            assert!(p.is_defined_direction());
            assert!(p.is_grid());
            let g = p.grid.expect("grid direction must carry shape");
            assert_eq!(g.rows_minus_one, 1);
            assert_eq!(g.columns_minus_one, 2);
            // §6.5.27.3 minus-one storage: wire 1/2 → 2 rows, 3 cols.
            assert_eq!(g.rows(), 2);
            assert_eq!(g.columns(), 3);
        }
    }

    /// §6.5.27.3 stores both grid extents minus one, so the `0xFF`
    /// wire endpoint means 256 — the projections must widen to `u16`
    /// rather than wrap a `u8` add to zero.
    #[test]
    fn pano_grid_dims_widen_past_u8() {
        let p = parse_pano(&pano_body_grid(Pano::DIRECTION_GRID_RASTER, 0xFF, 0xFF)).unwrap();
        let g = p.grid.unwrap();
        assert_eq!(g.rows(), 256);
        assert_eq!(g.columns(), 256);
        // And the all-zero floor reads as a 1×1 grid, not 0×0.
        let p = parse_pano(&pano_body_grid(Pano::DIRECTION_GRID_CONTINUOUS, 0, 0)).unwrap();
        let g = p.grid.unwrap();
        assert_eq!(g.rows(), 1);
        assert_eq!(g.columns(), 1);
    }

    /// §6.5.27.3 "other values are undefined" — an undefined
    /// direction is preserved verbatim (NOT a parse error) and reads
    /// as neither defined nor grid, so the §6.5.27.2 conditional
    /// grid-shape bytes are not consumed.
    #[test]
    fn pano_undefined_direction_is_preserved_not_rejected() {
        for direction in [6u8, 7, 0x7F, 0xFF] {
            let p = parse_pano(&pano_body_linear(direction)).unwrap();
            assert_eq!(p.panorama_direction, direction);
            assert_eq!(p.grid, None);
            assert!(!p.is_defined_direction());
            assert!(!p.is_grid());
        }
    }

    #[test]
    fn pano_rejects_unknown_version() {
        // version=1, flags=0; a direction byte that would otherwise
        // be a well-formed v0 linear body.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.push(Pano::DIRECTION_LEFT_TO_RIGHT);
        assert!(parse_pano(&buf).is_err());
    }

    /// A body without even the direction byte is rejected, and a grid
    /// direction whose conditional shape bytes are missing (in whole
    /// or in part) is rejected as truncated.
    #[test]
    fn pano_rejects_truncated_body() {
        // FullBox header alone — no direction byte at all.
        let buf = vec![0u8; 4];
        assert!(parse_pano(&buf).is_err());
        // Grid direction with no shape bytes.
        assert!(parse_pano(&pano_body_linear(Pano::DIRECTION_GRID_RASTER)).is_err());
        // Grid direction with only `rows_minus_one`
        // (missing `columns_minus_one`).
        let mut buf = pano_body_linear(Pano::DIRECTION_GRID_CONTINUOUS);
        buf.push(2);
        assert!(parse_pano(&buf).is_err());
    }

    /// Trailing bytes past the syntax-mandated tail are forward-compat
    /// space — the parser ignores them on both the linear (one-byte)
    /// and grid (three-byte) shapes, mirroring the behaviour of every
    /// other FullBox-headed property parser in this module.
    #[test]
    fn pano_tolerates_trailing_bytes() {
        let mut body = pano_body_linear(Pano::DIRECTION_TOP_TO_BOTTOM);
        body.extend_from_slice(&[0xDE, 0xAD]);
        let p = parse_pano(&body).unwrap();
        assert_eq!(p.panorama_direction, Pano::DIRECTION_TOP_TO_BOTTOM);
        assert_eq!(p.grid, None);

        let mut body = pano_body_grid(Pano::DIRECTION_GRID_RASTER, 3, 4);
        body.extend_from_slice(&[0xBE, 0xEF]);
        let p = parse_pano(&body).unwrap();
        let g = p.grid.unwrap();
        assert_eq!((g.rows(), g.columns()), (4, 5));
    }

    #[test]
    fn pano_dispatched_through_parse_ipco() {
        let body = pano_body_grid(Pano::DIRECTION_GRID_CONTINUOUS, 2, 3);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"pano");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Pano(p) => {
                assert_eq!(p.panorama_direction, Pano::DIRECTION_GRID_CONTINUOUS);
                let g = p.grid.unwrap();
                assert_eq!((g.rows(), g.columns()), (3, 4));
            }
            other => panic!("expected Pano, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"pano");
    }

    /// A recognised `pano` property — even when flagged essential
    /// (unusual for a descriptive property, but the parser does not
    /// reject the bit) — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn pano_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Pano(Pano::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.27.1 caps `pano` at one per associated item (`At most
    /// one`). A `Meta::property_for(item_id, b"pano")` lookup finds
    /// the associated single instance via the standard `ipma` walk.
    #[test]
    fn pano_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Pano(Pano {
                panorama_direction: Pano::DIRECTION_RIGHT_TO_LEFT,
                grid: None,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 9,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(9, b"pano") {
            Some(Property::Pano(p)) => {
                assert_eq!(p.panorama_direction, Pano::DIRECTION_RIGHT_TO_LEFT);
                assert!(p.is_defined_direction());
                assert!(!p.is_grid());
            }
            other => panic!("expected Pano, got {other:?}"),
        }
        // No `pano` for an item that doesn't carry the association.
        assert!(m.property_for(99, b"pano").is_none());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.28 SubSampleInformationBox (`subs`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build a `subs` body — FullBox('subs', version, flags), then the
    /// HEIF §6.5.28-fixed `entry_count = 1`, `sample_delta = 0`, the
    /// single entry's `subsample_count`, and that many tuples. Each tuple
    /// is `(size, priority, discardable, codec_specific_parameters)`;
    /// `size` is written 32-bit when `version == 1`, else 16-bit.
    fn subs_body(version: u8, flags: u32, samples: &[(u32, u8, u8, u32)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(version);
        buf.extend_from_slice(&[(flags >> 16) as u8, (flags >> 8) as u8, flags as u8]);
        buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        buf.extend_from_slice(&0u32.to_be_bytes()); // sample_delta
        buf.extend_from_slice(&(samples.len() as u16).to_be_bytes());
        for &(size, prio, disc, csp) in samples {
            if version == 1 {
                buf.extend_from_slice(&size.to_be_bytes());
            } else {
                buf.extend_from_slice(&(size as u16).to_be_bytes());
            }
            buf.push(prio);
            buf.push(disc);
            buf.extend_from_slice(&csp.to_be_bytes());
        }
        buf
    }

    #[test]
    fn subs_v0_round_trips_entries() {
        let samples = [(0x0102u32, 7u8, 1u8, 0xDEAD_BEEFu32), (0x0003, 0, 0, 0)];
        let s = parse_subs(&subs_body(0, 0, &samples)).unwrap();
        assert_eq!(s.flags, 0);
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.entries[0].subsample_size, 0x0102);
        assert_eq!(s.entries[0].subsample_priority, 7);
        assert_eq!(s.entries[0].discardable, 1);
        assert_eq!(s.entries[0].codec_specific_parameters, 0xDEAD_BEEF);
        assert_eq!(s.entries[1].subsample_size, 3);
    }

    /// v1 widens `subsample_size` to 32-bit (§8.7.7.2). A value past the
    /// 16-bit range only survives the v1 path.
    #[test]
    fn subs_v1_uses_32bit_size() {
        let samples = [(0x0001_0002u32, 0u8, 0u8, 0u32)];
        let s = parse_subs(&subs_body(1, 0, &samples)).unwrap();
        assert_eq!(s.entries[0].subsample_size, 0x0001_0002);
    }

    /// HEIF §6.5.28: `flags` distinguishes multiple `subs` on one item;
    /// the parser surfaces it from the FullBox header verbatim.
    #[test]
    fn subs_surfaces_flags() {
        let s = parse_subs(&subs_body(0, 0x00AB_CDEF & 0x00FF_FFFF, &[])).unwrap();
        assert_eq!(s.flags, 0x00AB_CDEF & 0x00FF_FFFF);
        assert!(s.entries.is_empty());
    }

    /// Zero sub-samples (`subsample_count == 0`) is well-formed: the entry
    /// exists but carries no tuples (§8.7.7.3).
    #[test]
    fn subs_empty_subsample_count_ok() {
        let s = parse_subs(&subs_body(1, 0, &[])).unwrap();
        assert!(s.entries.is_empty());
    }

    /// HEIF §6.5.28 fixes `entry_count == 1` for item-property use.
    #[test]
    fn subs_rejects_entry_count_not_one() {
        let mut buf = vec![0u8, 0, 0, 0];
        buf.extend_from_slice(&2u32.to_be_bytes()); // entry_count = 2
        assert!(parse_subs(&buf).is_err());
    }

    /// HEIF §6.5.28 fixes the single entry's `sample_delta == 0`.
    #[test]
    fn subs_rejects_nonzero_sample_delta() {
        let mut buf = vec![0u8, 0, 0, 0];
        buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
        buf.extend_from_slice(&5u32.to_be_bytes()); // sample_delta = 5
        buf.extend_from_slice(&0u16.to_be_bytes()); // subsample_count = 0
        assert!(parse_subs(&buf).is_err());
    }

    #[test]
    fn subs_rejects_unknown_version() {
        let mut buf = vec![2u8, 0, 0, 0]; // version = 2
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        assert!(parse_subs(&buf).is_err());
    }

    #[test]
    fn subs_rejects_truncated_tuple() {
        // Declares one sub-sample but the tuple bytes are short.
        let mut buf = vec![0u8, 0, 0, 0];
        buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        buf.extend_from_slice(&0u32.to_be_bytes()); // sample_delta
        buf.extend_from_slice(&1u16.to_be_bytes()); // subsample_count = 1
        buf.extend_from_slice(&0x0010u16.to_be_bytes()); // size only
        assert!(parse_subs(&buf).is_err());
    }

    /// Trailing bytes past the declared sub-samples are forward-compat
    /// space and are ignored.
    #[test]
    fn subs_tolerates_trailing_bytes() {
        let mut body = subs_body(0, 0, &[(4, 0, 0, 0)]);
        body.extend_from_slice(&[0xFF, 0xFF]);
        let s = parse_subs(&body).unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].subsample_size, 4);
    }

    #[test]
    fn subs_dispatched_through_parse_ipco() {
        let body = subs_body(1, 0, &[(0x0000_0009, 3, 0, 0x0000_0001)]);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"subs");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Subs(sub) => {
                assert_eq!(sub.entries.len(), 1);
                assert_eq!(sub.entries[0].subsample_size, 9);
                assert_eq!(sub.entries[0].subsample_priority, 3);
                assert_eq!(sub.entries[0].codec_specific_parameters, 1);
            }
            other => panic!("expected Subs, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"subs");
    }

    /// `subs` is descriptive (`Zero or more`); a recognised association —
    /// even if flagged essential — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn subs_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Subs(Subs::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// HEIF §6.5.28 allows `Zero or more` `subs` per item; a
    /// `Meta::property_for(item_id, b"subs")` lookup finds an associated
    /// instance via the standard `ipma` walk.
    #[test]
    fn subs_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Subs(Subs {
                flags: 1,
                entries: vec![SubsEntry {
                    subsample_size: 12,
                    subsample_priority: 2,
                    discardable: 0,
                    codec_specific_parameters: 0,
                }],
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 4,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(4, b"subs") {
            Some(Property::Subs(sub)) => {
                assert_eq!(sub.flags, 1);
                assert_eq!(sub.entries[0].subsample_size, 12);
            }
            other => panic!("expected Subs, got {other:?}"),
        }
        assert!(m.property_for(99, b"subs").is_none());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.29 TargetOlsProperty (`tols`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build a `tols` body — ItemFullProperty(v=0, f=0) followed by the
    /// big-endian `unsigned int(16) target_ols_idx` per §6.5.29.2.
    fn tols_body(target_ols_idx: u16) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(&target_ols_idx.to_be_bytes());
        buf
    }

    #[test]
    fn tols_round_trip_reads_target_ols_idx() {
        for raw in [0u16, 1, 7, 0x0102, u16::MAX] {
            let t = parse_tols(&tols_body(raw)).unwrap();
            assert_eq!(t.target_ols_idx, raw);
        }
    }

    /// §6.5.29.2 declares `target_ols_idx` as big-endian per ISO/IEC
    /// 14496-12 §4.2. A little-endian regression would surface `0x0102`
    /// = 258 as `0x0201` = 513.
    #[test]
    fn tols_target_ols_idx_is_big_endian() {
        let t = parse_tols(&tols_body(0x0102)).unwrap();
        assert_eq!(t.target_ols_idx, 0x0102);
        // Manually assemble the wire bytes high-then-low to pin order.
        let mut buf = vec![0u8; 4];
        buf.push(0x01);
        buf.push(0x02);
        assert_eq!(parse_tols(&buf).unwrap().target_ols_idx, 0x0102);
    }

    #[test]
    fn tols_rejects_unknown_version() {
        // version=1, flags=0 with an otherwise well-formed v0 payload.
        let mut buf = vec![1u8, 0, 0, 0];
        buf.extend_from_slice(&3u16.to_be_bytes());
        assert!(parse_tols(&buf).is_err());
    }

    #[test]
    fn tols_rejects_truncated_body() {
        // FullBox header alone — no field at all.
        let buf = vec![0u8; 4];
        assert!(parse_tols(&buf).is_err());
        // Header + only one of the two field bytes.
        let mut buf = vec![0u8; 4];
        buf.push(0x12);
        assert!(parse_tols(&buf).is_err());
    }

    /// Trailing bytes past the two-byte field are forward-compat space —
    /// the parser ignores them, matching every other FullBox-headed
    /// property parser in this module.
    #[test]
    fn tols_tolerates_trailing_bytes() {
        let mut body = tols_body(42);
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let t = parse_tols(&body).unwrap();
        assert_eq!(t.target_ols_idx, 42);
    }

    #[test]
    fn tols_dispatched_through_parse_ipco() {
        let body = tols_body(0xBEEF);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"tols");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Tols(t) => assert_eq!(t.target_ols_idx, 0xBEEF),
            other => panic!("expected Tols, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"tols");
    }

    /// §6.5.29.1 mandates `essential shall be equal to 1` for `tols` —
    /// it is the one descriptive §6.5.x property required to be
    /// essential. Because the parser surfaces a typed value, the item is
    /// recognised and does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn tols_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Tols(Tols { target_ols_idx: 2 })],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.29.1 caps `tols` at one per associated coded image item
    /// (`Zero or one`). A `Meta::property_for(item_id, b"tols")` lookup
    /// finds the associated instance via the standard `ipma` walk.
    #[test]
    fn tols_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Tols(Tols {
                target_ols_idx: 0x0304,
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 7,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(7, b"tols") {
            Some(Property::Tols(t)) => assert_eq!(t.target_ols_idx, 0x0304),
            other => panic!("expected Tols, got {other:?}"),
        }
        assert!(m.property_for(99, b"tols").is_none());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.37 ProgressiveDerivedImageItemInformationProperty
    // (`prdi`) — parse + helpers
    // -----------------------------------------------------------------

    /// Build a `prdi` body — FullBox('prdi', version=0, flags), then the
    /// §6.5.37.2 conditionally-present `step_count` and `item_count[]`
    /// fields, written exactly as the syntax guards prescribe given
    /// `flags`. `item_counts` is the per-step array a writer would emit
    /// (ignored when the `one_item_per_step` flag is set).
    fn prdi_body(version: u8, flags: u32, item_counts: &[u16]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(version);
        buf.extend_from_slice(&[(flags >> 16) as u8, (flags >> 8) as u8, flags as u8]);
        let one_item_per_step = flags & Prdi::FLAG_ONE_ITEM_PER_STEP != 0;
        let alt_is_candidate = flags & Prdi::FLAG_ALTERNATIVE_IS_CANDIDATE != 0;
        if !one_item_per_step || alt_is_candidate {
            buf.extend_from_slice(&(item_counts.len() as u16).to_be_bytes());
        }
        if !one_item_per_step {
            for &c in item_counts {
                buf.extend_from_slice(&c.to_be_bytes());
            }
        }
        buf
    }

    /// `one_item_per_step` clear → `step_count` and the per-step
    /// `item_count[]` array are both present (§6.5.37.2). The array is
    /// read with exactly `step_count` entries and surfaced verbatim.
    #[test]
    fn prdi_full_form_reads_step_count_and_item_counts() {
        let p = parse_prdi(&prdi_body(0, 0, &[3, 1, 2])).unwrap();
        assert_eq!(p.flags, 0);
        assert_eq!(p.step_count, Some(3));
        assert_eq!(p.item_counts.as_deref(), Some(&[3u16, 1, 2][..]));
        assert!(!p.is_one_item_per_step());
        assert!(!p.is_alternative_is_candidate());
        assert!(!p.is_item_reference_order());
        // Inference helpers return the present values unchanged.
        assert_eq!(p.step_count_or_inferred(99), 3);
        assert_eq!(p.item_count_for_step(0), Some(3));
        assert_eq!(p.item_count_for_step(2), Some(2));
        assert_eq!(p.item_count_for_step(3), None);
    }

    /// `one_item_per_step` set + `alternative_is_candidate` set →
    /// `step_count` is present (the count is no longer derivable from
    /// `'dimg'`) but the `item_count[]` array is absent (each step
    /// consumes one item). `item_count_for_step` infers `1`.
    #[test]
    fn prdi_one_item_per_step_with_alternatives_keeps_step_count_only() {
        let flags = Prdi::FLAG_ONE_ITEM_PER_STEP | Prdi::FLAG_ALTERNATIVE_IS_CANDIDATE;
        // `item_counts` carries the step count but no array bytes are
        // written for it in this flag combination.
        let p = parse_prdi(&prdi_body(0, flags, &[0, 0, 0, 0])).unwrap();
        assert_eq!(p.step_count, Some(4));
        assert_eq!(p.item_counts, None);
        assert!(p.is_one_item_per_step());
        assert!(p.is_alternative_is_candidate());
        // Every step consumes exactly one candidate input image item.
        for i in 0..4 {
            assert_eq!(p.item_count_for_step(i), Some(1));
        }
        assert_eq!(p.step_count_or_inferred(99), 4);
    }

    /// `one_item_per_step` set + `alternative_is_candidate` clear →
    /// the body is empty; both `step_count` and the `item_count[]` array
    /// are inferred per §6.5.37.3.
    #[test]
    fn prdi_one_item_per_step_no_alternatives_has_empty_body() {
        let flags = Prdi::FLAG_ONE_ITEM_PER_STEP;
        let body = prdi_body(0, flags, &[]);
        // FullBox header only — no payload bytes.
        assert_eq!(body.len(), 4);
        let p = parse_prdi(&body).unwrap();
        assert_eq!(p.step_count, None);
        assert_eq!(p.item_counts, None);
        // `step_count` is inferred to be the number of `'dimg'` inputs.
        assert_eq!(p.step_count_or_inferred(5), 5);
        // Each step consumes one candidate input image item.
        assert_eq!(p.item_count_for_step(0), Some(1));
        assert_eq!(p.item_count_for_step(42), Some(1));
    }

    /// The `item_reference_order` flag is orthogonal to the body shape —
    /// it does not gate any field, but it must round-trip through
    /// [`Prdi::flags`] and be reported by the projection. Combined here
    /// with the full form so the body is still read.
    #[test]
    fn prdi_item_reference_order_flag_round_trips() {
        let flags = Prdi::FLAG_ITEM_REFERENCE_ORDER;
        let p = parse_prdi(&prdi_body(0, flags, &[2, 2])).unwrap();
        assert!(p.is_item_reference_order());
        assert!(!p.is_one_item_per_step());
        assert_eq!(p.step_count, Some(2));
        assert_eq!(p.item_counts.as_deref(), Some(&[2u16, 2][..]));
        // The whole flags field is preserved, including the set bit.
        assert_eq!(p.flags, Prdi::FLAG_ITEM_REFERENCE_ORDER);
    }

    /// A zero `step_count` (reserved for future use per §6.5.37.3) reads
    /// as an empty `item_count[]` array — well-formed at the wire level,
    /// surfaced verbatim so a reader can apply the reserved-value policy.
    #[test]
    fn prdi_zero_step_count_yields_empty_item_counts() {
        let p = parse_prdi(&prdi_body(0, 0, &[])).unwrap();
        assert_eq!(p.step_count, Some(0));
        assert_eq!(p.item_counts.as_deref(), Some(&[][..]));
        assert_eq!(p.item_count_for_step(0), None);
    }

    #[test]
    fn prdi_rejects_unknown_version() {
        // version=1, flags=0; otherwise a well-formed v0 full-form body.
        let mut buf = prdi_body(0, 0, &[1]);
        buf[0] = 1;
        assert!(parse_prdi(&buf).is_err());
    }

    /// A truncated `step_count` or a short `item_count[]` array is
    /// rejected. The full form needs 2 bytes for `step_count` plus
    /// 2×`step_count` for the array; cutting either short fails.
    #[test]
    fn prdi_rejects_truncated_body() {
        // step_count present (flags=0) but body has only the header.
        assert!(parse_prdi(&[0u8, 0, 0, 0]).is_err());
        // step_count says 2 entries but only one is present.
        let mut buf = vec![0u8, 0, 0, 0];
        buf.extend_from_slice(&2u16.to_be_bytes()); // step_count = 2
        buf.extend_from_slice(&7u16.to_be_bytes()); // only one item_count
        assert!(parse_prdi(&buf).is_err());
        // one_item_per_step+alt: step_count required but missing.
        let flags = Prdi::FLAG_ONE_ITEM_PER_STEP | Prdi::FLAG_ALTERNATIVE_IS_CANDIDATE;
        let hdr = [0u8, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
        assert!(parse_prdi(&hdr).is_err());
    }

    /// Trailing bytes past the declared array are forward-compat space —
    /// the parser reads exactly `step_count` entries and ignores the
    /// rest, mirroring every other FullBox-headed property parser.
    #[test]
    fn prdi_tolerates_trailing_bytes() {
        let mut body = prdi_body(0, 0, &[1, 2]);
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let p = parse_prdi(&body).unwrap();
        assert_eq!(p.step_count, Some(2));
        assert_eq!(p.item_counts.as_deref(), Some(&[1u16, 2][..]));
    }

    #[test]
    fn prdi_dispatched_through_parse_ipco() {
        let body = prdi_body(0, 0, &[3, 1]);
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"prdi");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Prdi(p) => {
                assert_eq!(p.step_count, Some(2));
                assert_eq!(p.item_counts.as_deref(), Some(&[3u16, 1][..]));
            }
            other => panic!("expected Prdi, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"prdi");
    }

    /// A recognised `prdi` property — even when flagged essential — does
    /// NOT trip [`Meta::unsupported_essential_properties`].
    #[test]
    fn prdi_essential_association_is_recognised() {
        let m = Meta {
            properties: vec![Property::Prdi(Prdi::default())],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: true,
                }],
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// §6.5.37.1 caps `prdi` at zero-or-one per derived image item. A
    /// `Meta::property_for(item_id, b"prdi")` lookup finds the associated
    /// single instance via the standard `ipma` walk.
    #[test]
    fn prdi_lookup_via_property_for() {
        let m = Meta {
            properties: vec![Property::Prdi(Prdi {
                flags: Prdi::FLAG_ITEM_REFERENCE_ORDER,
                step_count: Some(2),
                item_counts: Some(vec![1, 1]),
            })],
            associations: vec![ItemPropertyAssociation {
                item_id: 9,
                entries: vec![PropertyAssociation {
                    index: 0,
                    essential: false,
                }],
            }],
            ..Meta::default()
        };
        match m.property_for(9, b"prdi") {
            Some(Property::Prdi(p)) => {
                assert!(p.is_item_reference_order());
                assert_eq!(p.step_count, Some(2));
            }
            other => panic!("expected Prdi, got {other:?}"),
        }
        assert!(m.property_for(99, b"prdi").is_none());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.38 SingleStreamProperty (`sstr`)
    // -----------------------------------------------------------------

    /// `sstr` is the bare `ItemProperty('sstr')` marker (§6.5.38.2): an
    /// empty body parses, dispatches through `parse_ipco`, and reports
    /// the `'sstr'` kind.
    #[test]
    fn sstr_empty_body_parses_and_dispatches() {
        assert_eq!(parse_sstr(&[]).unwrap(), Sstr);
        let mut ipco = Vec::new();
        ipco.extend_from_slice(&8u32.to_be_bytes());
        ipco.extend_from_slice(b"sstr");
        let props = parse_ipco(&ipco).unwrap();
        assert_eq!(props.len(), 1);
        assert!(matches!(props[0], Property::Sstr(_)));
        assert_eq!(props[0].kind(), *b"sstr");
    }

    /// Trailing bytes past the empty marker are forward-compat space and
    /// are tolerated.
    #[test]
    fn sstr_tolerates_trailing_bytes() {
        assert_eq!(parse_sstr(&[0xDE, 0xAD]).unwrap(), Sstr);
    }

    // -----------------------------------------------------------------
    // HEIF §6.10.2.1 TextLayoutProperty (`txlo`)
    // -----------------------------------------------------------------

    /// Build a `txlo` body — FullBox('txlo', version=0, flags), then the
    /// `(flags & 1)`-selected field-size block followed by `font_size`
    /// and the two NUL-terminated utf8strings (§6.10.2.1.2).
    #[allow(clippy::too_many_arguments)]
    fn txlo_body(
        flags: u32,
        reference_width: u32,
        reference_height: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        font_size: u16,
        direction: &str,
        writing_mode: &str,
    ) -> Vec<u8> {
        let mut buf = vec![0u8, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
        let large = flags & Txlo::FLAG_LARGE_FIELD_SIZE != 0;
        let push_u = |buf: &mut Vec<u8>, v: u32| {
            if large {
                buf.extend_from_slice(&v.to_be_bytes());
            } else {
                buf.extend_from_slice(&(v as u16).to_be_bytes());
            }
        };
        let push_i = |buf: &mut Vec<u8>, v: i32| {
            if large {
                buf.extend_from_slice(&v.to_be_bytes());
            } else {
                buf.extend_from_slice(&(v as i16).to_be_bytes());
            }
        };
        push_u(&mut buf, reference_width);
        push_u(&mut buf, reference_height);
        push_i(&mut buf, x);
        push_i(&mut buf, y);
        push_u(&mut buf, width);
        push_u(&mut buf, height);
        buf.extend_from_slice(&font_size.to_be_bytes());
        buf.extend_from_slice(direction.as_bytes());
        buf.push(0);
        buf.extend_from_slice(writing_mode.as_bytes());
        buf.push(0);
        buf
    }

    /// 16-bit field-size form (`flags & 1 == 0`): all six geometry fields
    /// are read as 16-bit, with `x`/`y` sign-extended.
    #[test]
    fn txlo_16bit_fields_parse() {
        let body = txlo_body(0, 1920, 1080, -5, -3, 640, 480, 0x0A00, "ltr", "lrtb");
        let t = parse_txlo(&body).unwrap();
        assert!(!t.is_large_field_size());
        assert_eq!(t.reference_width, 1920);
        assert_eq!(t.reference_height, 1080);
        assert_eq!(t.x, -5);
        assert_eq!(t.y, -3);
        assert_eq!(t.width, 640);
        assert_eq!(t.height, 480);
        assert_eq!(t.font_size, 0x0A00);
        assert_eq!(t.font_size_percent(), 10.0);
        assert_eq!(t.direction, "ltr");
        assert_eq!(t.writing_mode, "lrtb");
    }

    /// 32-bit field-size form (`flags & 1 == 1`): geometry fields exceed
    /// the 16-bit range and negative `x`/`y` sign-extend across 32 bits.
    #[test]
    fn txlo_32bit_fields_parse() {
        let body = txlo_body(1, 70000, 80000, -100000, 42, 65536, 1, 256, "rtl", "tblr");
        let t = parse_txlo(&body).unwrap();
        assert!(t.is_large_field_size());
        assert_eq!(t.reference_width, 70000);
        assert_eq!(t.reference_height, 80000);
        assert_eq!(t.x, -100000);
        assert_eq!(t.y, 42);
        assert_eq!(t.width, 65536);
        assert_eq!(t.font_size_percent(), 1.0);
        assert_eq!(t.direction, "rtl");
        assert_eq!(t.writing_mode, "tblr");
    }

    /// Defaulted (empty) `direction` / `writing_mode` strings round-trip
    /// as empty (the spec assumes "ltr"/"lrtb" defaults at render time,
    /// but the wire value is surfaced verbatim).
    #[test]
    fn txlo_empty_strings_round_trip_and_dispatch() {
        let body = txlo_body(0, 100, 100, 0, 0, 100, 100, 0x1900, "", "");
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"txlo");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        match &props[0] {
            Property::Txlo(t) => {
                assert_eq!(t.direction, "");
                assert_eq!(t.writing_mode, "");
                assert_eq!(t.font_size_percent(), 25.0);
            }
            other => panic!("expected Txlo, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"txlo");
    }

    #[test]
    fn txlo_rejects_unknown_version() {
        let mut body = txlo_body(0, 10, 10, 0, 0, 10, 10, 0, "ltr", "lrtb");
        body[0] = 1;
        assert!(parse_txlo(&body).is_err());
    }

    #[test]
    fn txlo_rejects_truncated_geometry() {
        // flags=0 ⇒ 6×2 geometry + 2 font_size = 14 bytes minimum after
        // the FullBox header; truncate inside the geometry block.
        let body = vec![0u8, 0, 0, 0, 0x07, 0x80, 0x04];
        assert!(parse_txlo(&body).is_err());
    }

    // -----------------------------------------------------------------
    // HEIF §6.10.2.2 ExtendedLanguageProperty (`elng`)
    // -----------------------------------------------------------------

    fn elng_body(version: u8, lang: &str) -> Vec<u8> {
        let mut buf = vec![version, 0, 0, 0];
        buf.extend_from_slice(lang.as_bytes());
        buf.push(0);
        buf
    }

    #[test]
    fn elng_parses_language_tag_and_dispatches() {
        let body = elng_body(0, "en-US");
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"elng");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        match &props[0] {
            Property::Elng(e) => assert_eq!(e.extended_language, "en-US"),
            other => panic!("expected Elng, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"elng");
    }

    #[test]
    fn elng_empty_tag_round_trips() {
        let e = parse_elng(&elng_body(0, "")).unwrap();
        assert_eq!(e.extended_language, "");
    }

    #[test]
    fn elng_rejects_unknown_version() {
        assert!(parse_elng(&elng_body(1, "de")).is_err());
    }

    // -----------------------------------------------------------------
    // HEIF §6.10.4.1 FontCharacteristicsProperty (`fnch`)
    // -----------------------------------------------------------------

    fn fnch_body(version: u8, family: &str, style: &str, weight: &str) -> Vec<u8> {
        let mut buf = vec![version, 0, 0, 0];
        for s in [family, style, weight] {
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn fnch_parses_three_strings_and_dispatches() {
        let body = fnch_body(0, "Helvetica", "italic", "bold");
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(b"fnch");
        ipco.extend_from_slice(&body);
        let props = parse_ipco(&ipco).unwrap();
        match &props[0] {
            Property::Fnch(f) => {
                assert_eq!(f.font_family, "Helvetica");
                assert_eq!(f.font_style, "italic");
                assert_eq!(f.font_weight, "bold");
            }
            other => panic!("expected Fnch, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"fnch");
    }

    #[test]
    fn fnch_empty_strings_round_trip() {
        let f = parse_fnch(&fnch_body(0, "", "", "")).unwrap();
        assert_eq!(f.font_family, "");
        assert_eq!(f.font_style, "");
        assert_eq!(f.font_weight, "");
    }

    #[test]
    fn fnch_rejects_unknown_version() {
        assert!(parse_fnch(&fnch_body(2, "Arial", "normal", "normal")).is_err());
    }

    #[test]
    fn fnch_rejects_unterminated_string() {
        // Header + "Arial" with no NUL terminator → unterminated cstr.
        let mut body = vec![0u8, 0, 0, 0];
        body.extend_from_slice(b"Arial");
        assert!(parse_fnch(&body).is_err());
    }

    // -----------------------------------------------------------------
    // HEIF §6.5.30-6.5.35 slideshow transition-effect properties
    // (`wipe` / `zoom` / `fade` / `splt` / `stpe` / `ssld`)
    // -----------------------------------------------------------------

    /// Wrap a property body in an `ipco` so the dispatch path is
    /// exercised end-to-end. `fourcc` is the four-CC; `body` already
    /// includes the FullBox header bytes.
    fn ipco_one(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut ipco = Vec::new();
        let s = 8 + body.len() as u32;
        ipco.extend_from_slice(&s.to_be_bytes());
        ipco.extend_from_slice(fourcc);
        ipco.extend_from_slice(body);
        ipco
    }

    /// FullBox(version=0, flags=0) header followed by the given payload.
    fn full_box_v0(payload: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; 4];
        buf.extend_from_slice(payload);
        buf
    }

    #[test]
    fn wipe_parses_every_defined_direction() {
        for dir in 0u8..=7 {
            let w = parse_wipe(&full_box_v0(&[dir])).unwrap();
            assert_eq!(w.transition_direction, dir);
            assert!(w.is_known_direction());
        }
        // Named constants line up with the spec values.
        assert_eq!(Wipe::FROM_LEFT, 0);
        assert_eq!(Wipe::FROM_RIGHT, 1);
        assert_eq!(Wipe::FROM_TOP, 2);
        assert_eq!(Wipe::FROM_BOTTOM, 3);
        assert_eq!(Wipe::FROM_RIGHT_BOTTOM, 7);
    }

    #[test]
    fn wipe_reserved_direction_surfaced_not_known() {
        let w = parse_wipe(&full_box_v0(&[8])).unwrap();
        assert_eq!(w.transition_direction, 8);
        assert!(!w.is_known_direction());
    }

    #[test]
    fn wipe_rejects_short_and_unknown_version() {
        assert!(parse_wipe(&full_box_v0(&[])).is_err());
        // version = 1
        assert!(parse_wipe(&[1u8, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn wipe_tolerates_trailing_bytes_and_dispatches() {
        let body = full_box_v0(&[Wipe::FROM_LEFT_TOP, 0xAA, 0xBB]);
        let props = parse_ipco(&ipco_one(b"wipe", &body)).unwrap();
        assert_eq!(props.len(), 1);
        match &props[0] {
            Property::Wipe(w) => assert_eq!(w.transition_direction, Wipe::FROM_LEFT_TOP),
            other => panic!("expected Wipe, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"wipe");
    }

    #[test]
    fn zoom_unpacks_direction_and_shape() {
        // direction = 1 (top bit), shape = 2 (star)
        let packed = (Zoom::DIRECTION_OUT << 7) | Zoom::SHAPE_STAR;
        let z = parse_zoom(&full_box_v0(&[packed])).unwrap();
        assert_eq!(z.transition_direction, Zoom::DIRECTION_OUT);
        assert_eq!(z.transition_shape, Zoom::SHAPE_STAR);
        assert!(z.is_known_shape());

        // direction = 0, shape = 0 (rectangular)
        let z = parse_zoom(&full_box_v0(&[0])).unwrap();
        assert_eq!(z.transition_direction, Zoom::DIRECTION_IN);
        assert_eq!(z.transition_shape, Zoom::SHAPE_RECTANGULAR);

        // direction = 0, shape = 1 (circle)
        let z = parse_zoom(&full_box_v0(&[Zoom::SHAPE_CIRCLE])).unwrap();
        assert_eq!(z.transition_direction, Zoom::DIRECTION_IN);
        assert_eq!(z.transition_shape, Zoom::SHAPE_CIRCLE);
    }

    #[test]
    fn zoom_reserved_shape_surfaced_not_known() {
        // direction = 1, shape = 0x7f (max 7-bit, reserved)
        let z = parse_zoom(&full_box_v0(&[0xFF])).unwrap();
        assert_eq!(z.transition_direction, Zoom::DIRECTION_OUT);
        assert_eq!(z.transition_shape, 0x7f);
        assert!(!z.is_known_shape());
    }

    #[test]
    fn zoom_rejects_short_and_unknown_version() {
        assert!(parse_zoom(&full_box_v0(&[])).is_err());
        assert!(parse_zoom(&[3u8, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn zoom_dispatches_through_ipco() {
        let z = (Zoom::DIRECTION_IN << 7) | Zoom::SHAPE_CIRCLE;
        let props = parse_ipco(&ipco_one(b"zoom", &full_box_v0(&[z]))).unwrap();
        match &props[0] {
            Property::Zoom(z) => {
                assert_eq!(z.transition_direction, Zoom::DIRECTION_IN);
                assert_eq!(z.transition_shape, Zoom::SHAPE_CIRCLE);
            }
            other => panic!("expected Zoom, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"zoom");
    }

    #[test]
    fn fade_parses_every_defined_direction() {
        for dir in 0u8..=2 {
            let f = parse_fade(&full_box_v0(&[dir])).unwrap();
            assert_eq!(f.transition_direction, dir);
            assert!(f.is_known_direction());
        }
        assert_eq!(Fade::THROUGH_WHITE, 0);
        assert_eq!(Fade::THROUGH_BLACK, 1);
        assert_eq!(Fade::DISSOLVE, 2);
        let f = parse_fade(&full_box_v0(&[3])).unwrap();
        assert!(!f.is_known_direction());
    }

    #[test]
    fn fade_rejects_short_and_unknown_version() {
        assert!(parse_fade(&full_box_v0(&[])).is_err());
        assert!(parse_fade(&[2u8, 0, 0, 0, 1]).is_err());
    }

    #[test]
    fn splt_parses_every_defined_direction_and_dispatches() {
        for dir in 0u8..=3 {
            let s = parse_splt(&full_box_v0(&[dir])).unwrap();
            assert_eq!(s.transition_direction, dir);
            assert!(s.is_known_direction());
        }
        assert_eq!(Splt::VERTICAL_IN, 0);
        assert_eq!(Splt::VERTICAL_OUT, 1);
        assert_eq!(Splt::HORIZONTAL_IN, 2);
        assert_eq!(Splt::HORIZONTAL_OUT, 3);
        let s = parse_splt(&full_box_v0(&[4])).unwrap();
        assert!(!s.is_known_direction());

        let props = parse_ipco(&ipco_one(b"splt", &full_box_v0(&[Splt::HORIZONTAL_IN]))).unwrap();
        match &props[0] {
            Property::Splt(s) => assert_eq!(s.transition_direction, Splt::HORIZONTAL_IN),
            other => panic!("expected Splt, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"splt");
    }

    #[test]
    fn splt_rejects_short_and_unknown_version() {
        assert!(parse_splt(&full_box_v0(&[])).is_err());
        assert!(parse_splt(&[1u8, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn stpe_parses_and_converts_period() {
        // 32 units of 1/16 s == 2.0 s
        let p = parse_stpe(&full_box_v0(&[32])).unwrap();
        assert_eq!(p.transition_period, 32);
        assert!((p.seconds() - 2.0).abs() < 1e-9);
        let p = parse_stpe(&full_box_v0(&[8])).unwrap();
        assert!((p.seconds() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn stpe_rejects_short_and_unknown_version() {
        assert!(parse_stpe(&full_box_v0(&[])).is_err());
        assert!(parse_stpe(&[1u8, 0, 0, 0, 5]).is_err());
    }

    #[test]
    fn ssld_parses_converts_and_flags_reserved() {
        // 48 units of 1/16 s == 3.0 s
        let d = parse_ssld(&full_box_v0(&64u16.to_be_bytes())).unwrap();
        assert_eq!(d.duration, 64);
        assert!((d.seconds() - 4.0).abs() < 1e-9);
        assert!(!d.is_reserved());

        // duration == 0 is the §6.5.35.3 reserved sentinel
        let d = parse_ssld(&full_box_v0(&0u16.to_be_bytes())).unwrap();
        assert!(d.is_reserved());
        assert_eq!(d.seconds(), 0.0);
    }

    #[test]
    fn ssld_rejects_short_and_unknown_version() {
        // one byte short of the u16 duration
        assert!(parse_ssld(&full_box_v0(&[0])).is_err());
        assert!(parse_ssld(&full_box_v0(&[])).is_err());
        assert!(parse_ssld(&[1u8, 0, 0, 0, 0, 16]).is_err());
    }

    #[test]
    fn ssld_dispatches_and_tolerates_trailing() {
        let mut body = full_box_v0(&80u16.to_be_bytes());
        body.extend_from_slice(&[0xDE, 0xAD]);
        let props = parse_ipco(&ipco_one(b"ssld", &body)).unwrap();
        match &props[0] {
            Property::Ssld(d) => assert_eq!(d.duration, 80),
            other => panic!("expected Ssld, got {other:?}"),
        }
        assert_eq!(props[0].kind(), *b"ssld");
    }

    /// The transition effects are listed as *transformative* in
    /// §6.5.30-6.5.33 but are slideshow-presentation hints, not pixel
    /// transforms: a recognised association — even when flagged
    /// essential — does NOT trip
    /// [`Meta::unsupported_essential_properties`].
    #[test]
    fn transition_effects_essential_association_recognised() {
        let m = Meta {
            properties: vec![
                Property::Wipe(Wipe::default()),
                Property::Zoom(Zoom::default()),
                Property::Fade(Fade::default()),
                Property::Splt(Splt::default()),
                Property::Stpe(Stpe::default()),
                Property::Ssld(Ssld::default()),
            ],
            associations: vec![ItemPropertyAssociation {
                item_id: 1,
                entries: (0..6)
                    .map(|i| PropertyAssociation {
                        index: i,
                        essential: true,
                    })
                    .collect(),
            }],
            ..Meta::default()
        };
        assert!(!m.has_unsupported_essential_property(1));
        assert!(m.unsupported_essential_properties(1).is_empty());
    }

    /// Each transition property is reachable via the standard
    /// `property_for` (`ipma`) lookup.
    #[test]
    fn transition_effects_lookup_via_property_for() {
        let m = Meta {
            properties: vec![
                Property::Wipe(Wipe {
                    transition_direction: Wipe::FROM_TOP,
                }),
                Property::Ssld(Ssld { duration: 96 }),
            ],
            associations: vec![ItemPropertyAssociation {
                item_id: 7,
                entries: vec![
                    PropertyAssociation {
                        index: 0,
                        essential: false,
                    },
                    PropertyAssociation {
                        index: 1,
                        essential: false,
                    },
                ],
            }],
            ..Meta::default()
        };
        match m.property_for(7, b"wipe") {
            Some(Property::Wipe(w)) => assert_eq!(w.transition_direction, Wipe::FROM_TOP),
            other => panic!("expected Wipe, got {other:?}"),
        }
        match m.property_for(7, b"ssld") {
            Some(Property::Ssld(d)) => assert_eq!(d.duration, 96),
            other => panic!("expected Ssld, got {other:?}"),
        }
        assert!(m.property_for(99, b"wipe").is_none());
    }
}
