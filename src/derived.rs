//! Derived-image descriptors and entity grouping — HEIF §6.6.2 + §9.4.
//!
//! AVIF restricts derived-image carriage to the `grid` form (av1-avif
//! §4.2), but a reader that walks AVIF / HEIF files in the wild will
//! encounter HEIF features layered on top of an AVIF brand:
//!
//! * **`iovl` image-overlay derivations** (HEIF §6.6.2.2): one or more
//!   source images placed at signed `(x, y)` offsets on a fixed canvas
//!   with an RGBA fill colour.
//! * **`iden` identity derivations** (HEIF §6.6.2.1): the source image
//!   passed through unchanged, useful when transformative properties
//!   on the derivation differ from those on the source.
//! * **Entity grouping** (HEIF §9.4): `grpl` containing one or more
//!   `EntityToGroupBox` per grouping type. The common groupings are
//!   `altr` (alternates), `ster` (stereo pair), `eqiv` (timeline
//!   equivalence to a track sample).
//!
//! All parsers here operate on raw box payload bytes — they're
//! independent of the [`crate::parser`] file walker and the
//! [`crate::meta`] item-property pipeline, so a caller can apply them
//! to any byte range that follows the documented layout. The
//! [`crate::parser::AvifHeader`] walker now exposes a `grpl` slice
//! through [`crate::meta::Meta::groups`] for callers that need to
//! enumerate AVIF/HEIF alternates without rebuilding the container
//! traversal.

use crate::box_parser::{iter_boxes, parse_full_box, read_u16, read_u32, type_str, BoxType};
use crate::error::{AvifError as Error, Result};

/// One placed image inside an `iovl` overlay descriptor (HEIF §6.6.2.2).
/// `horizontal_offset` + `vertical_offset` are signed pixel offsets from
/// the top-left corner of the canvas; per spec, source pixels with a
/// negative coordinate (or coordinates `>= output_width / output_height`)
/// are clipped out of the reconstructed image.
///
/// The actual source image item id isn't stored here — `iovl` payload
/// only carries the offsets; the source ids come from the parallel
/// `dimg` iref's `to_ids` list (in the same order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayEntry {
    pub horizontal_offset: i32,
    pub vertical_offset: i32,
}

/// Parsed `iovl` ImageOverlay descriptor (HEIF §6.6.2.2). Bottom-most
/// input image is `entries[0]`; the top-most is `entries[entries.len()-1]`.
///
/// `canvas_fill_value` is RGBA in sRGB (R, G, B, A) per spec; the A
/// channel runs 0 (transparent) to 65535 (opaque) linearly. RGB values
/// are also 16-bit, padded with zeros if the writer thought in 8-bit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageOverlay {
    pub canvas_fill_value: [u16; 4],
    pub output_width: u32,
    pub output_height: u32,
    pub entries: Vec<OverlayEntry>,
}

impl ImageOverlay {
    /// Parse `iovl` payload bytes against the `reference_count`
    /// argument. `reference_count` is the number of `dimg` `to_ids`
    /// for the overlay item — the `iovl` payload doesn't carry the
    /// count itself, so the caller is responsible for supplying it
    /// from the iref.
    ///
    /// Spec: ISO/IEC 23008-12 §6.6.2.2.2 (Syntax). The first byte is
    /// `version` (must be 0), then `flags`; `flags & 1` selects 32-bit
    /// over 16-bit field widths for `output_*` and `*_offset`.
    pub fn parse(payload: &[u8], reference_count: usize) -> Result<Self> {
        if payload.len() < 2 {
            return Err(Error::invalid("avif: iovl header too short"));
        }
        let version = payload[0];
        if version != 0 {
            return Err(Error::invalid(format!("avif: iovl version {version} != 0")));
        }
        let flags = payload[1];
        // FieldLength = ((flags & 1) + 1) * 16 bits = 2 or 4 bytes.
        let field_len = if flags & 1 != 0 { 4 } else { 2 };
        // Header: canvas_fill_value (4 × u16) + output_width + output_height
        let mut cursor = 2usize;
        let min = 2 + 4 * 2 + 2 * field_len + reference_count * 2 * field_len;
        if payload.len() < min {
            return Err(Error::invalid(format!(
                "avif: iovl too short ({} < {min})",
                payload.len()
            )));
        }
        let mut canvas = [0u16; 4];
        for slot in canvas.iter_mut() {
            *slot = read_u16(payload, cursor)?;
            cursor += 2;
        }
        let output_width = read_field_u32(payload, cursor, field_len)?;
        cursor += field_len;
        let output_height = read_field_u32(payload, cursor, field_len)?;
        cursor += field_len;
        let mut entries = Vec::with_capacity(reference_count);
        for _ in 0..reference_count {
            let h = read_field_i32(payload, cursor, field_len)?;
            cursor += field_len;
            let v = read_field_i32(payload, cursor, field_len)?;
            cursor += field_len;
            entries.push(OverlayEntry {
                horizontal_offset: h,
                vertical_offset: v,
            });
        }
        Ok(ImageOverlay {
            canvas_fill_value: canvas,
            output_width,
            output_height,
            entries,
        })
    }
}

fn read_field_u32(buf: &[u8], cursor: usize, field_len: usize) -> Result<u32> {
    match field_len {
        2 => Ok(read_u16(buf, cursor)? as u32),
        4 => read_u32(buf, cursor),
        n => Err(Error::invalid(format!("avif: iovl field length {n}"))),
    }
}

fn read_field_i32(buf: &[u8], cursor: usize, field_len: usize) -> Result<i32> {
    match field_len {
        2 => Ok(read_u16(buf, cursor)? as i16 as i32),
        4 => Ok(read_u32(buf, cursor)? as i32),
        n => Err(Error::invalid(format!("avif: iovl field length {n}"))),
    }
}

/// One `EntityToGroupBox` entry (HEIF / ISOBMFF §8.15.3 / 23008-12 §9.4.3).
/// `grouping_type` is a four-CC declaring the relationship between the
/// listed entity ids: `altr` (alternates), `ster` (stereo pair), `eqiv`
/// (equivalence to a track sample), and others.
///
/// `entity_ids` are conventionally `item_ID` values from the same `meta`
/// (file-level `grpl` references file-level items; per §9.4.1). When a
/// grouping mixes items and tracks, the resolver chooses based on which
/// id matches — that's a caller-side concern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityGroup {
    pub grouping_type: BoxType,
    pub group_id: u32,
    pub entity_ids: Vec<u32>,
}

impl EntityGroup {
    /// True when the grouping type signals stereo pair (HEIF §9.4.3.1).
    /// Reader convention: `entity_ids[0]` is the left view,
    /// `entity_ids[1]` is the right view.
    pub fn is_stereo_pair(&self) -> bool {
        &self.grouping_type == b"ster"
    }

    /// True when the grouping type signals an alternate set
    /// (HEIF §9.4.3.1) — the reader picks one of `entity_ids` and
    /// discards the others.
    pub fn is_alternates(&self) -> bool {
        &self.grouping_type == b"altr"
    }

    /// True when the grouping type signals timeline equivalence to a
    /// track sample (HEIF §6.8.1).
    pub fn is_equivalence(&self) -> bool {
        &self.grouping_type == b"eqiv"
    }
}

/// Parse a `GroupsListBox` (`grpl`) payload into its set of entity
/// groups. Spec: ISO/IEC 23008-12 §9.4.2 (file-level grouping).
///
/// `grpl` itself is a plain Box containing one or more `EntityToGroupBox`
/// children, each a FullBox keyed by `grouping_type` four-CC.
pub fn parse_grpl(payload: &[u8]) -> Result<Vec<EntityGroup>> {
    let mut out = Vec::new();
    for hdr in iter_boxes(payload) {
        let hdr = hdr?;
        let child = &payload[hdr.payload_start..hdr.end()];
        let (_version, _flags, body) = parse_full_box(child)?;
        if body.len() < 8 {
            return Err(Error::invalid(format!(
                "avif: EntityToGroupBox '{}' body too short ({} < 8)",
                type_str(&hdr.box_type),
                body.len()
            )));
        }
        let group_id = read_u32(body, 0)?;
        let num_entities = read_u32(body, 4)? as usize;
        let need = 8 + num_entities * 4;
        if body.len() < need {
            return Err(Error::invalid(format!(
                "avif: EntityToGroupBox '{}' truncated entity list ({} < {need})",
                type_str(&hdr.box_type),
                body.len()
            )));
        }
        let mut entity_ids = Vec::with_capacity(num_entities);
        for i in 0..num_entities {
            entity_ids.push(read_u32(body, 8 + i * 4)?);
        }
        out.push(EntityGroup {
            grouping_type: hdr.box_type,
            group_id,
            entity_ids,
        });
    }
    Ok(out)
}

/// Result of a `mif1` brand compliance audit (HEIF §10.2.1.1).
///
/// A `mif1` file must contain a top-level `ftyp` + `meta` and its `meta`
/// must contain `hdlr`, `pitm`, `iinf` + `infe` entries, `iloc`, and
/// `iprp`. The audit is informational: AVIF files in the wild ship
/// `mif1` as a compatible brand without strict compliance (e.g. ones
/// emitted by ImageMagick), and our reader still accepts them. The
/// validator exists so callers that want to enforce strict-mif1 mode
/// can.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Mif1Compliance {
    pub has_hdlr: bool,
    pub has_pitm: bool,
    pub has_iinf: bool,
    pub has_iloc: bool,
    pub has_iprp: bool,
    /// Number of `infe` entries inside `iinf`. mif1 requires at least
    /// one image item (the primary).
    pub infe_count: usize,
    /// Brand carries `mif1` in major_brand or compatible_brands.
    pub claims_mif1: bool,
}

impl Mif1Compliance {
    /// True when every mandatory mif1 reader-side box is present.
    /// Strict spec interpretation per §10.2.1.1 table — does not include
    /// the optional `iref` / `idat` / `iprp` of §10.2.1.2 entries that
    /// are reader-side suggestions only.
    pub fn is_compliant(&self) -> bool {
        self.has_hdlr
            && self.has_pitm
            && self.has_iinf
            && self.has_iloc
            && self.has_iprp
            && self.infe_count > 0
    }

    /// A human-friendly list of missing required boxes, useful for
    /// diagnostics. Returns an empty list when [`Self::is_compliant`].
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.has_hdlr {
            out.push("hdlr");
        }
        if !self.has_pitm {
            out.push("pitm");
        }
        if !self.has_iinf {
            out.push("iinf");
        }
        if self.infe_count == 0 {
            out.push("infe");
        }
        if !self.has_iloc {
            out.push("iloc");
        }
        if !self.has_iprp {
            out.push("iprp");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16-bit `iovl` with two source images stacked at `(10, 20)` and
    /// `(30, 40)` on a 256×256 canvas filled white-opaque.
    #[test]
    fn iovl_parses_two_entries_16bit_fields() {
        let mut buf = Vec::new();
        buf.push(0); // version
        buf.push(0); // flags = 0 → 16-bit fields
        for v in [65535u16, 65535, 65535, 65535] {
            buf.extend_from_slice(&v.to_be_bytes());
        }
        buf.extend_from_slice(&256u16.to_be_bytes()); // output_width
        buf.extend_from_slice(&256u16.to_be_bytes()); // output_height
        buf.extend_from_slice(&10i16.to_be_bytes()); // h
        buf.extend_from_slice(&20i16.to_be_bytes()); // v
        buf.extend_from_slice(&30i16.to_be_bytes()); // h
        buf.extend_from_slice(&40i16.to_be_bytes()); // v
        let o = ImageOverlay::parse(&buf, 2).unwrap();
        assert_eq!(o.canvas_fill_value, [65535, 65535, 65535, 65535]);
        assert_eq!(o.output_width, 256);
        assert_eq!(o.output_height, 256);
        assert_eq!(
            o.entries,
            vec![
                OverlayEntry {
                    horizontal_offset: 10,
                    vertical_offset: 20
                },
                OverlayEntry {
                    horizontal_offset: 30,
                    vertical_offset: 40
                }
            ]
        );
    }

    /// 32-bit field variant (`flags & 1 == 1`) — needed for canvases
    /// larger than 65535 pixels.
    #[test]
    fn iovl_parses_32bit_fields() {
        let mut buf = Vec::new();
        buf.push(0); // version
        buf.push(1); // flags = 1 → 32-bit fields
        for v in [0u16, 0, 0, 0] {
            buf.extend_from_slice(&v.to_be_bytes());
        }
        buf.extend_from_slice(&100_000u32.to_be_bytes()); // output_width
        buf.extend_from_slice(&80_000u32.to_be_bytes()); // output_height
        buf.extend_from_slice(&(-5i32).to_be_bytes()); // h (negative clips)
        buf.extend_from_slice(&10_000i32.to_be_bytes()); // v
        let o = ImageOverlay::parse(&buf, 1).unwrap();
        assert_eq!(o.output_width, 100_000);
        assert_eq!(o.output_height, 80_000);
        assert_eq!(o.entries[0].horizontal_offset, -5);
        assert_eq!(o.entries[0].vertical_offset, 10_000);
    }

    /// Negative `horizontal_offset` (signed) decoded correctly in
    /// 16-bit mode — a placement intentionally clipped at the left
    /// edge.
    #[test]
    fn iovl_negative_offset_signed_round_trip() {
        let mut buf = Vec::new();
        buf.push(0);
        buf.push(0);
        for _ in 0..4 {
            buf.extend_from_slice(&0u16.to_be_bytes());
        }
        buf.extend_from_slice(&64u16.to_be_bytes());
        buf.extend_from_slice(&64u16.to_be_bytes());
        buf.extend_from_slice(&(-3i16).to_be_bytes());
        buf.extend_from_slice(&(-4i16).to_be_bytes());
        let o = ImageOverlay::parse(&buf, 1).unwrap();
        assert_eq!(o.entries[0].horizontal_offset, -3);
        assert_eq!(o.entries[0].vertical_offset, -4);
    }

    /// `iovl` with `reference_count` larger than payload is rejected
    /// before allocating.
    #[test]
    fn iovl_rejects_oversized_reference_count() {
        let mut buf = Vec::new();
        buf.push(0);
        buf.push(0);
        for _ in 0..4 {
            buf.extend_from_slice(&0u16.to_be_bytes());
        }
        buf.extend_from_slice(&16u16.to_be_bytes());
        buf.extend_from_slice(&16u16.to_be_bytes());
        // Payload only has room for 0 entries; claim 100.
        assert!(ImageOverlay::parse(&buf, 100).is_err());
    }

    /// `iovl` rejects unrecognised versions.
    #[test]
    fn iovl_rejects_nonzero_version() {
        let buf = vec![1u8, 0]; // version=1
        assert!(ImageOverlay::parse(&buf, 0).is_err());
    }

    /// Build a minimal `grpl` containing one `altr` group with three
    /// alternate item ids.
    fn build_grpl_altr() -> Vec<u8> {
        let mut buf = Vec::new();
        // EntityToGroupBox: size(4) + 'altr' + FullBox(v=0,f=0) + group_id(4) + count(4) + ids
        let mut child = vec![0u8; 4]; // FullBox
        child.extend_from_slice(&42u32.to_be_bytes()); // group_id
        child.extend_from_slice(&3u32.to_be_bytes()); // num_entities
        child.extend_from_slice(&1u32.to_be_bytes());
        child.extend_from_slice(&2u32.to_be_bytes());
        child.extend_from_slice(&3u32.to_be_bytes());
        let size = (8 + child.len()) as u32;
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"altr");
        buf.extend_from_slice(&child);
        buf
    }

    /// `parse_grpl` extracts an `altr` group and surfaces its entity
    /// list in declaration order.
    #[test]
    fn grpl_parses_altr_group() {
        let grpl = build_grpl_altr();
        let groups = parse_grpl(&grpl).unwrap();
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(&g.grouping_type, b"altr");
        assert!(g.is_alternates());
        assert!(!g.is_stereo_pair());
        assert!(!g.is_equivalence());
        assert_eq!(g.group_id, 42);
        assert_eq!(g.entity_ids, vec![1, 2, 3]);
    }

    /// `ster` group convention: two entities, first is left view.
    #[test]
    fn grpl_parses_ster_pair() {
        let mut buf = Vec::new();
        let mut child = vec![0u8; 4]; // FullBox
        child.extend_from_slice(&7u32.to_be_bytes()); // group_id
        child.extend_from_slice(&2u32.to_be_bytes()); // num_entities
        child.extend_from_slice(&10u32.to_be_bytes()); // left view
        child.extend_from_slice(&11u32.to_be_bytes()); // right view
        let size = (8 + child.len()) as u32;
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"ster");
        buf.extend_from_slice(&child);
        let groups = parse_grpl(&buf).unwrap();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].is_stereo_pair());
        assert_eq!(groups[0].entity_ids, vec![10, 11]);
    }

    /// Multiple groups in one `grpl` come out in declaration order.
    #[test]
    fn grpl_parses_multiple_groups() {
        let mut buf = Vec::new();
        // altr group
        let mut a = vec![0u8; 4];
        a.extend_from_slice(&1u32.to_be_bytes());
        a.extend_from_slice(&1u32.to_be_bytes());
        a.extend_from_slice(&100u32.to_be_bytes());
        let asz = (8 + a.len()) as u32;
        buf.extend_from_slice(&asz.to_be_bytes());
        buf.extend_from_slice(b"altr");
        buf.extend_from_slice(&a);
        // eqiv group
        let mut e = vec![0u8; 4];
        e.extend_from_slice(&2u32.to_be_bytes());
        e.extend_from_slice(&0u32.to_be_bytes()); // empty group
        let esz = (8 + e.len()) as u32;
        buf.extend_from_slice(&esz.to_be_bytes());
        buf.extend_from_slice(b"eqiv");
        buf.extend_from_slice(&e);
        let groups = parse_grpl(&buf).unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups[0].is_alternates());
        assert!(groups[1].is_equivalence());
        assert!(groups[1].entity_ids.is_empty());
    }

    /// Truncated entity list is rejected before allocation overflow.
    #[test]
    fn grpl_rejects_truncated_entity_list() {
        let mut buf = Vec::new();
        let mut child = vec![0u8; 4];
        child.extend_from_slice(&1u32.to_be_bytes()); // group_id
        child.extend_from_slice(&5u32.to_be_bytes()); // claims 5 entities…
        child.extend_from_slice(&100u32.to_be_bytes()); // …but ships only 1
        let size = (8 + child.len()) as u32;
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"altr");
        buf.extend_from_slice(&child);
        assert!(parse_grpl(&buf).is_err());
    }

    /// Mif1Compliance flags every missing required box.
    #[test]
    fn mif1_compliance_missing_list() {
        let bare = Mif1Compliance::default();
        assert!(!bare.is_compliant());
        let m = bare.missing();
        // Order is fixed; every required box should appear.
        assert!(m.contains(&"hdlr"));
        assert!(m.contains(&"pitm"));
        assert!(m.contains(&"iinf"));
        assert!(m.contains(&"infe"));
        assert!(m.contains(&"iloc"));
        assert!(m.contains(&"iprp"));
    }

    /// Mif1Compliance with every required flag set reports compliant.
    #[test]
    fn mif1_compliance_full() {
        let m = Mif1Compliance {
            has_hdlr: true,
            has_pitm: true,
            has_iinf: true,
            has_iloc: true,
            has_iprp: true,
            infe_count: 1,
            claims_mif1: true,
        };
        assert!(m.is_compliant());
        assert!(m.missing().is_empty());
    }
}
