//! HEIF region items and region geometries (ISO/IEC 23008-12 §11.2 / §11.3).
//!
//! A *region item* (`item_type == 'rgan'`, §11.3.2) describes one or more
//! regions of an image item against a reference space, and is attached to
//! the image item via a `'cdsc'` (content-describes) item reference. Each
//! region is a [`RegionGeometry`] — a point, rectangle, ellipse, polygon,
//! polyline, referenced mask, or inline mask (§11.2.1). A *derived region
//! item* (§11.3.3) carries a `'drgn'` item reference to input region items
//! and applies an operation identified by its `item_type` (e.g. `'iden'`).
//!
//! The companion *mask item* (`'mski'`, §11.2.2) and its mandatory
//! [`crate::meta::MaskC`] (`mskC`) configuration property live in
//! [`crate::meta`]; this module parses the region-item **data** (the bytes
//! resolved through the item's `iloc`) into a typed [`RegionItem`].
//!
//! This is a *descriptive* surface: parsing the geometries does not decode
//! pixels. A renderer overlays the regions on the decoded image item using
//! the reference-space → image-space scaling rule of §11.3.2.

use crate::error::{AvifError as Error, Result};
use crate::meta::Meta;

/// HEIF §11.3.2 — region item type (`'rgan'`).
pub const ITEM_TYPE_RGAN: [u8; 4] = *b"rgan";
/// HEIF §11.2.2 — mask item type (`'mski'`).
pub const ITEM_TYPE_MSKI: [u8; 4] = *b"mski";
/// HEIF §11.3.3 — derived-region item reference type (`'drgn'`).
pub const REF_TYPE_DRGN: [u8; 4] = *b"drgn";
/// HEIF §6.6.2.1 — identity derivation item type (`'iden'`), reused by
/// §11.3.3.2.1 for the identity derived-region item.
// Internal module-local duplicate — the public constant is the crate-root
// re-export of `crate::meta::ITEM_TYPE_IDEN`.
#[doc(hidden)]
pub const ITEM_TYPE_IDEN: [u8; 4] = *b"iden";

/// One region geometry inside a [`RegionItem`] (HEIF §11.2.1).
///
/// The `geometry_type` byte selects the variant; all coordinate / size
/// fields use the region item's `(flags & 1)`-selected 16- or 32-bit width
/// (§11.2.1.3), sign-extended to `i32` for signed fields. `x` / `y` may be
/// negative to place a corner / centre / point outside the image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionGeometry {
    /// `geometry_type == 0` — a single point at `(x, y)`.
    Point { x: i32, y: i32 },
    /// `geometry_type == 1` — a rectangle with top-left `(x, y)`.
    Rectangle {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    /// `geometry_type == 2` — an ellipse centred at `(x, y)`.
    Ellipse {
        x: i32,
        y: i32,
        radius_x: u32,
        radius_y: u32,
    },
    /// `geometry_type == 3` — a closed polygon (first point is not
    /// repeated as the last, §11.2.1.3 NOTE 2). `points` are `(px, py)`.
    Polygon { points: Vec<(i32, i32)> },
    /// `geometry_type == 6` — an open polyline. `points` are `(px, py)`.
    Polyline { points: Vec<(i32, i32)> },
    /// `geometry_type == 4` — a mask defined in a referenced image / mask
    /// item (or, in a region track, a referenced track sample). `width` /
    /// `height` equal to `0` defer to the referenced item's
    /// `ImageSpatialExtentsProperty`. `mask_ref_idx` is present only in a
    /// region *track* (`is_region_track == 1`); a region *item* leaves it
    /// `None`.
    ReferencedMask {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        mask_ref_idx: Option<u32>,
    },
    /// `geometry_type == 5` — a mask whose pixels are coded inline in the
    /// structure data (§11.2.1.2). `mask_coding_method` 0 = uncompressed,
    /// 1 = `deflate()` (RFC 1951) with `mask_coding_parameters` giving the
    /// coded byte count. `data` is the raw (coded or uncompressed) bit
    /// payload; each pixel is one bit, 8 pixels per byte, big-endian, no
    /// per-line padding (§11.2.1.3).
    InlineMask {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        mask_coding_method: u8,
        mask_coding_parameters: Option<u32>,
        data: Vec<u8>,
    },
}

impl RegionGeometry {
    /// The `geometry_type` byte that introduces this variant (§11.2.1.3).
    pub fn geometry_type(&self) -> u8 {
        match self {
            RegionGeometry::Point { .. } => 0,
            RegionGeometry::Rectangle { .. } => 1,
            RegionGeometry::Ellipse { .. } => 2,
            RegionGeometry::Polygon { .. } => 3,
            RegionGeometry::ReferencedMask { .. } => 4,
            RegionGeometry::InlineMask { .. } => 5,
            RegionGeometry::Polyline { .. } => 6,
        }
    }

    /// `true` for the two mask geometries (`geometry_type` 4 / 5), which
    /// pull their pixels from a referenced item or inline coded data.
    pub fn is_mask(&self) -> bool {
        matches!(
            self,
            RegionGeometry::ReferencedMask { .. } | RegionGeometry::InlineMask { .. }
        )
    }
}

/// A parsed region item (HEIF §11.3.2) — the typed form of a `'rgan'`
/// item's data.
///
/// The reference space is a 2D coordinate system with origin `(0, 0)` at
/// the top-left and a maximum size of `reference_width` × `reference_height`
/// (§11.3.2.1). Each [`RegionGeometry`] is expressed against that space; a
/// renderer scales it to the image item's pixel extents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionItem {
    /// The full 8-bit `flags` field. Only bit 0 (the field-size selector)
    /// is defined; the whole value is retained. [`Self::is_large_field_size`]
    /// projects bit 0.
    pub flags: u8,
    /// Width, in pixels, of the reference space (§11.3.2.3).
    pub reference_width: u32,
    /// Height, in pixels, of the reference space (§11.3.2.3).
    pub reference_height: u32,
    /// The regions described by the item, in declaration order.
    pub regions: Vec<RegionGeometry>,
}

impl RegionItem {
    /// §11.3.2.2 `flags` bit 0 — when set the geometry coordinate / size
    /// fields are 32-bit (else 16-bit).
    pub const FLAG_LARGE_FIELD_SIZE: u8 = 0x01;

    /// `true` when the 32-bit field-size selector (`flags & 1`) is set.
    pub fn is_large_field_size(&self) -> bool {
        self.flags & Self::FLAG_LARGE_FIELD_SIZE != 0
    }

    /// Number of regions described by the item.
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Parse the data of a region item (`'rgan'`, §11.3.2.2):
    ///
    /// ```text
    /// unsigned int(8)  version = 0;
    /// unsigned int(8)  flags;
    /// field_size = ((flags & 1) + 1) * 16;   // 16 or 32 bits
    /// unsigned int(field_size) reference_width;
    /// unsigned int(field_size) reference_height;
    /// unsigned int(8)  region_count;
    /// for (r = 0; r < region_count; r++)
    ///     RegionGeometryStruct(version, flags, 0) region_geometry;
    /// ```
    ///
    /// `is_region_track` is fixed to `0` for a region item, so the
    /// referenced-mask `mask_ref_idx` field is absent (§11.2.1.2). Returns
    /// `Unsupported` for an unknown `version` (only `0` is defined) or an
    /// unknown `geometry_type`, and `InvalidData` for a truncated payload.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut cur = Cursor::new(payload);
        let version = cur.u8()?;
        if version != 0 {
            return Err(Error::unsupported(format!(
                "avif region item: version {version} != 0"
            )));
        }
        let flags = cur.u8()?;
        let large = flags & Self::FLAG_LARGE_FIELD_SIZE != 0;
        let reference_width = cur.field_u(large)?;
        let reference_height = cur.field_u(large)?;
        let region_count = cur.u8()? as usize;
        let mut regions = Vec::with_capacity(region_count);
        for _ in 0..region_count {
            regions.push(parse_geometry(&mut cur, large, false)?);
        }
        Ok(RegionItem {
            flags,
            reference_width,
            reference_height,
            regions,
        })
    }
}

/// Compliance + resolution of a §11.3.3.2.1 identity (`'iden'`) derived
/// region item.
///
/// A derived region item carries a `'drgn'` item reference to its input
/// region item(s) (§11.3.3.1). The only derivation type the spec defines
/// is the identity transformation (`item_type == 'iden'`, §11.3.3.2.1),
/// which `shall` have no item body and a `'drgn'` `reference_count` of
/// exactly 1; the resulting regions are the input region item's regions
/// with the derived item's transformative item properties applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedRegionItem {
    /// Item id of the derived region item (an `'iden'` item).
    pub derived_item_id: u32,
    /// The single input region item id named by the `'drgn'` reference,
    /// when `drgn_reference_count == 1`. `None` when the input count is
    /// non-conformant.
    pub source_region_item_id: Option<u32>,
    /// Number of `'drgn'` targets (§11.3.3.2.1 requires exactly 1).
    pub drgn_reference_count: usize,
    /// Number of distinct `'drgn'` iref entries sharing this item as their
    /// `from_item_ID` (§11.3.3.1: shall not be greater than 1).
    pub drgn_iref_count: usize,
    /// `true` when the derived item carries a non-empty item body
    /// (§11.3.3.2.1 requires no item body — an `'iden'` derived region
    /// item shall have no extents).
    pub has_item_body: bool,
}

impl DerivedRegionItem {
    /// `true` when every §11.3.3.2.1 / §11.3.3.1 `shall` passes: exactly
    /// one `'drgn'` input, at most one `'drgn'` iref entry, and no item
    /// body.
    pub fn is_compliant(&self) -> bool {
        self.drgn_reference_count == 1 && self.drgn_iref_count == 1 && !self.has_item_body
    }

    /// Human-readable list of failed `shall`s; empty when
    /// [`Self::is_compliant`].
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.drgn_reference_count != 1 {
            out.push("drgn-reference-count-eq-1");
        }
        if self.drgn_iref_count != 1 {
            out.push("drgn-iref-count-eq-1");
        }
        if self.has_item_body {
            out.push("no-item-body");
        }
        out
    }
}

/// Resolve and audit every identity (`'iden'`) derived region item carried
/// in `meta` (HEIF §11.3.3.2.1).
///
/// A derived region item is an `'iden'`-typed item with a `'drgn'` item
/// reference to its input region item(s). This finds every `'iden'` item
/// that has at least one outgoing `'drgn'` reference, resolves its single
/// input region item (when the `reference_count` is the conformant `1`),
/// and reports the §11.3.3.2.1 `shall`-level constraints in a
/// [`DerivedRegionItem`]. Returns the records in `iinf` declaration order;
/// empty when the file ships no derived region items.
///
/// Plain region items (`'rgan'`) and `'iden'` *image* derivations
/// (`'dimg'`-referenced, §6.6.2.1) are not in scope here — a derived
/// *region* item is distinguished by carrying a `'drgn'` reference.
pub fn resolve_derived_region_items(meta: &Meta) -> Vec<DerivedRegionItem> {
    let mut out = Vec::new();
    for item in &meta.items {
        if item.item_type != ITEM_TYPE_IDEN {
            continue;
        }
        let drgn_iref_count = meta
            .irefs
            .iter()
            .filter(|e| e.reference_type == REF_TYPE_DRGN && e.from_id == item.id)
            .count();
        if drgn_iref_count == 0 {
            // An 'iden' item with no 'drgn' reference is an image-level
            // identity derivation (§6.6.2.1), not a derived region item.
            continue;
        }
        let inputs = meta.iref_targets_of(&REF_TYPE_DRGN, item.id);
        let drgn_reference_count = inputs.len();
        let source_region_item_id = if drgn_reference_count == 1 {
            Some(inputs[0])
        } else {
            None
        };
        let has_item_body = meta
            .location_by_id(item.id)
            .map(|loc| loc.extents.iter().any(|x| x.length > 0))
            .unwrap_or(false);
        out.push(DerivedRegionItem {
            derived_item_id: item.id,
            source_region_item_id,
            drgn_reference_count,
            drgn_iref_count,
            has_item_body,
        });
    }
    out
}

/// Parse a single `RegionGeometryStruct` (§11.2.1.2) from `cur`. `large`
/// selects the 32-bit field width; `is_region_track` gates the
/// referenced-mask `mask_ref_idx` field (`true` only for region tracks).
fn parse_geometry(
    cur: &mut Cursor<'_>,
    large: bool,
    is_region_track: bool,
) -> Result<RegionGeometry> {
    let geometry_type = cur.u8()?;
    match geometry_type {
        0 => Ok(RegionGeometry::Point {
            x: cur.field_i(large)?,
            y: cur.field_i(large)?,
        }),
        1 => Ok(RegionGeometry::Rectangle {
            x: cur.field_i(large)?,
            y: cur.field_i(large)?,
            width: cur.field_u(large)?,
            height: cur.field_u(large)?,
        }),
        2 => Ok(RegionGeometry::Ellipse {
            x: cur.field_i(large)?,
            y: cur.field_i(large)?,
            radius_x: cur.field_u(large)?,
            radius_y: cur.field_u(large)?,
        }),
        3 | 6 => {
            let point_count = cur.field_u(large)? as usize;
            let mut points = Vec::with_capacity(point_count);
            for _ in 0..point_count {
                let px = cur.field_i(large)?;
                let py = cur.field_i(large)?;
                points.push((px, py));
            }
            if geometry_type == 3 {
                Ok(RegionGeometry::Polygon { points })
            } else {
                Ok(RegionGeometry::Polyline { points })
            }
        }
        4 => {
            let x = cur.field_i(large)?;
            let y = cur.field_i(large)?;
            let width = cur.field_u(large)?;
            let height = cur.field_u(large)?;
            let mask_ref_idx = if is_region_track {
                Some(cur.field_u(large)?)
            } else {
                None
            };
            Ok(RegionGeometry::ReferencedMask {
                x,
                y,
                width,
                height,
                mask_ref_idx,
            })
        }
        5 => {
            let x = cur.field_i(large)?;
            let y = cur.field_i(large)?;
            let width = cur.field_u(large)?;
            let height = cur.field_u(large)?;
            let mask_coding_method = cur.u8()?;
            let mask_coding_parameters = if mask_coding_method != 0 {
                Some(cur.u32()?)
            } else {
                None
            };
            // The inline mask `data[]` consumes the remainder of the
            // structure. For an uncompressed mask it is
            // ceil(width*height / 8) bytes; for a deflate()-coded mask it is
            // `mask_coding_parameters` bytes. We take the rest of the
            // region-item payload, which — because the inline-mask geometry
            // is necessarily the last region in the item (its data runs to
            // the end) — is exactly the mask payload.
            let data = cur.rest().to_vec();
            Ok(RegionGeometry::InlineMask {
                x,
                y,
                width,
                height,
                mask_coding_method,
                mask_coding_parameters,
                data,
            })
        }
        other => Err(Error::unsupported(format!(
            "avif region geometry: reserved geometry_type {other}"
        ))),
    }
}

/// A minimal big-endian byte cursor for region-item parsing.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| Error::invalid("avif region item: offset overflow"))?;
        if end > self.buf.len() {
            return Err(Error::invalid(format!(
                "avif region item: truncated read ({end} > {})",
                self.buf.len()
            )));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// Read a `(flags & 1)`-selected **unsigned** field (16- or 32-bit).
    fn field_u(&mut self, large: bool) -> Result<u32> {
        if large {
            self.u32()
        } else {
            let s = self.take(2)?;
            Ok(u32::from(u16::from_be_bytes([s[0], s[1]])))
        }
    }

    /// Read a `(flags & 1)`-selected **signed** field, sign-extended to
    /// `i32`.
    fn field_i(&mut self, large: bool) -> Result<i32> {
        if large {
            Ok(self.u32()? as i32)
        } else {
            let s = self.take(2)?;
            Ok(i32::from(i16::from_be_bytes([s[0], s[1]])))
        }
    }

    /// The unconsumed remainder of the buffer.
    fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 16-bit-field region-item payload from a list of geometry
    /// byte-blobs (each already encoding its `geometry_type` + fields).
    fn region_item_body(flags: u8, ref_w: u16, ref_h: u16, geoms: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = vec![0u8, flags];
        let large = flags & 1 != 0;
        let push_u = |buf: &mut Vec<u8>, v: u32| {
            if large {
                buf.extend_from_slice(&v.to_be_bytes());
            } else {
                buf.extend_from_slice(&(v as u16).to_be_bytes());
            }
        };
        push_u(&mut buf, ref_w as u32);
        push_u(&mut buf, ref_h as u32);
        buf.push(geoms.len() as u8);
        for g in geoms {
            buf.extend_from_slice(g);
        }
        buf
    }

    fn i16b(v: i16) -> [u8; 2] {
        v.to_be_bytes()
    }
    fn u16b(v: u16) -> [u8; 2] {
        v.to_be_bytes()
    }

    #[test]
    fn parses_point_rectangle_ellipse() {
        let mut point = vec![0u8]; // geometry_type 0
        point.extend_from_slice(&i16b(-5));
        point.extend_from_slice(&i16b(7));

        let mut rect = vec![1u8]; // geometry_type 1
        rect.extend_from_slice(&i16b(10));
        rect.extend_from_slice(&i16b(20));
        rect.extend_from_slice(&u16b(30));
        rect.extend_from_slice(&u16b(40));

        let mut ell = vec![2u8]; // geometry_type 2
        ell.extend_from_slice(&i16b(50));
        ell.extend_from_slice(&i16b(60));
        ell.extend_from_slice(&u16b(8));
        ell.extend_from_slice(&u16b(9));

        let body = region_item_body(0, 1920, 1080, &[point, rect, ell]);
        let ri = RegionItem::parse(&body).unwrap();
        assert!(!ri.is_large_field_size());
        assert_eq!(ri.reference_width, 1920);
        assert_eq!(ri.reference_height, 1080);
        assert_eq!(ri.region_count(), 3);
        assert_eq!(ri.regions[0], RegionGeometry::Point { x: -5, y: 7 });
        assert_eq!(
            ri.regions[1],
            RegionGeometry::Rectangle {
                x: 10,
                y: 20,
                width: 30,
                height: 40
            }
        );
        assert_eq!(
            ri.regions[2],
            RegionGeometry::Ellipse {
                x: 50,
                y: 60,
                radius_x: 8,
                radius_y: 9
            }
        );
        assert_eq!(ri.regions[0].geometry_type(), 0);
        assert_eq!(ri.regions[2].geometry_type(), 2);
    }

    #[test]
    fn parses_polygon_and_polyline() {
        let mut poly = vec![3u8]; // geometry_type 3 (polygon)
        poly.extend_from_slice(&u16b(3)); // point_count
        for (px, py) in [(0i16, 0i16), (10, 0), (5, 8)] {
            poly.extend_from_slice(&i16b(px));
            poly.extend_from_slice(&i16b(py));
        }
        let mut line = vec![6u8]; // geometry_type 6 (polyline)
        line.extend_from_slice(&u16b(2));
        for (px, py) in [(-3i16, -4i16), (3, 4)] {
            line.extend_from_slice(&i16b(px));
            line.extend_from_slice(&i16b(py));
        }
        let body = region_item_body(0, 100, 100, &[poly, line]);
        let ri = RegionItem::parse(&body).unwrap();
        assert_eq!(
            ri.regions[0],
            RegionGeometry::Polygon {
                points: vec![(0, 0), (10, 0), (5, 8)]
            }
        );
        assert_eq!(
            ri.regions[1],
            RegionGeometry::Polyline {
                points: vec![(-3, -4), (3, 4)]
            }
        );
    }

    #[test]
    fn referenced_mask_has_no_ref_idx_in_region_item() {
        let mut mask = vec![4u8]; // geometry_type 4
        mask.extend_from_slice(&i16b(1));
        mask.extend_from_slice(&i16b(2));
        mask.extend_from_slice(&u16b(0)); // width 0 → defer to ispe
        mask.extend_from_slice(&u16b(0)); // height 0 → defer to ispe
        let body = region_item_body(0, 64, 64, &[mask]);
        let ri = RegionItem::parse(&body).unwrap();
        assert_eq!(
            ri.regions[0],
            RegionGeometry::ReferencedMask {
                x: 1,
                y: 2,
                width: 0,
                height: 0,
                mask_ref_idx: None,
            }
        );
        assert!(ri.regions[0].is_mask());
    }

    #[test]
    fn inline_mask_uncompressed_takes_trailing_bits() {
        let mut mask = vec![5u8]; // geometry_type 5
        mask.extend_from_slice(&i16b(0));
        mask.extend_from_slice(&i16b(0));
        mask.extend_from_slice(&u16b(16)); // 16x2 mask = 32 bits = 4 bytes
        mask.extend_from_slice(&u16b(2));
        mask.push(0); // mask_coding_method = 0 (uncompressed)
        mask.extend_from_slice(&[0xAA, 0x55, 0xF0, 0x0F]); // 4 data bytes
        let body = region_item_body(0, 16, 2, &[mask]);
        let ri = RegionItem::parse(&body).unwrap();
        match &ri.regions[0] {
            RegionGeometry::InlineMask {
                width,
                height,
                mask_coding_method,
                mask_coding_parameters,
                data,
                ..
            } => {
                assert_eq!(*width, 16);
                assert_eq!(*height, 2);
                assert_eq!(*mask_coding_method, 0);
                assert_eq!(*mask_coding_parameters, None);
                assert_eq!(data, &[0xAA, 0x55, 0xF0, 0x0F]);
            }
            other => panic!("expected InlineMask, got {other:?}"),
        }
    }

    #[test]
    fn inline_mask_deflate_carries_coding_parameters() {
        let mut mask = vec![5u8];
        mask.extend_from_slice(&i16b(0));
        mask.extend_from_slice(&i16b(0));
        mask.extend_from_slice(&u16b(8));
        mask.extend_from_slice(&u16b(8));
        mask.push(1); // deflate
        mask.extend_from_slice(&3u32.to_be_bytes()); // 3 coded bytes
        mask.extend_from_slice(&[0x01, 0x02, 0x03]);
        let body = region_item_body(0, 8, 8, &[mask]);
        let ri = RegionItem::parse(&body).unwrap();
        match &ri.regions[0] {
            RegionGeometry::InlineMask {
                mask_coding_method,
                mask_coding_parameters,
                data,
                ..
            } => {
                assert_eq!(*mask_coding_method, 1);
                assert_eq!(*mask_coding_parameters, Some(3));
                assert_eq!(data, &[0x01, 0x02, 0x03]);
            }
            other => panic!("expected InlineMask, got {other:?}"),
        }
    }

    #[test]
    fn large_field_size_uses_32bit_fields() {
        let mut rect = vec![1u8];
        rect.extend_from_slice(&(-100000i32).to_be_bytes());
        rect.extend_from_slice(&42i32.to_be_bytes());
        rect.extend_from_slice(&70000u32.to_be_bytes());
        rect.extend_from_slice(&80000u32.to_be_bytes());
        let body = region_item_body(1, 65535, 65535, &[rect]);
        let ri = RegionItem::parse(&body).unwrap();
        assert!(ri.is_large_field_size());
        assert_eq!(
            ri.regions[0],
            RegionGeometry::Rectangle {
                x: -100000,
                y: 42,
                width: 70000,
                height: 80000,
            }
        );
    }

    #[test]
    fn rejects_unknown_version() {
        let mut body = region_item_body(0, 10, 10, &[]);
        body[0] = 1;
        assert!(RegionItem::parse(&body).is_err());
    }

    #[test]
    fn rejects_reserved_geometry_type() {
        let body = region_item_body(0, 10, 10, &[vec![7u8]]);
        assert!(RegionItem::parse(&body).is_err());
    }

    #[test]
    fn rejects_truncated_region_array() {
        // region_count says 1 but no geometry bytes follow.
        let mut body = vec![0u8, 0];
        body.extend_from_slice(&u16b(10));
        body.extend_from_slice(&u16b(10));
        body.push(1); // region_count = 1
        assert!(RegionItem::parse(&body).is_err());
    }

    #[test]
    fn zero_regions_parses_empty() {
        let body = region_item_body(0, 320, 240, &[]);
        let ri = RegionItem::parse(&body).unwrap();
        assert_eq!(ri.region_count(), 0);
        assert_eq!(ri.reference_width, 320);
    }

    // -----------------------------------------------------------------
    // Derived region items (HEIF §11.3.3.2.1)
    // -----------------------------------------------------------------

    use crate::meta::{IrefEntry, ItemInfo, Meta};

    fn iden_item(id: u32) -> ItemInfo {
        ItemInfo {
            id,
            item_type: ITEM_TYPE_IDEN,
            name: String::new(),
            content_type: None,
            content_encoding: None,
            item_uri_type: None,
            flags: 0,
        }
    }

    fn rgan_item(id: u32) -> ItemInfo {
        ItemInfo {
            id,
            item_type: ITEM_TYPE_RGAN,
            name: String::new(),
            content_type: None,
            content_encoding: None,
            item_uri_type: None,
            flags: 0,
        }
    }

    /// A conformant identity derived region item: `'iden'` typed, one
    /// `'drgn'` reference to a region item, no item body.
    #[test]
    fn derived_region_iden_is_compliant() {
        let meta = Meta {
            items: vec![iden_item(2), rgan_item(3)],
            irefs: vec![IrefEntry {
                reference_type: REF_TYPE_DRGN,
                from_id: 2,
                to_ids: vec![3],
            }],
            ..Meta::default()
        };
        let derived = resolve_derived_region_items(&meta);
        assert_eq!(derived.len(), 1);
        assert_eq!(derived[0].derived_item_id, 2);
        assert_eq!(derived[0].source_region_item_id, Some(3));
        assert_eq!(derived[0].drgn_reference_count, 1);
        assert!(derived[0].is_compliant());
        assert!(derived[0].missing().is_empty());
    }

    /// A `'drgn'` reference_count other than 1 is a §11.3.3.2.1 violation
    /// and leaves the source unresolved.
    #[test]
    fn derived_region_multi_input_is_non_compliant() {
        let meta = Meta {
            items: vec![iden_item(2)],
            irefs: vec![IrefEntry {
                reference_type: REF_TYPE_DRGN,
                from_id: 2,
                to_ids: vec![3, 4],
            }],
            ..Meta::default()
        };
        let d = &resolve_derived_region_items(&meta)[0];
        assert_eq!(d.drgn_reference_count, 2);
        assert_eq!(d.source_region_item_id, None);
        assert!(!d.is_compliant());
        assert!(d.missing().contains(&"drgn-reference-count-eq-1"));
    }

    /// An `'iden'` item with no `'drgn'` reference is an *image*-level
    /// identity derivation (§6.6.2.1), NOT a derived region item — it is
    /// skipped here.
    #[test]
    fn iden_without_drgn_is_not_a_derived_region() {
        let meta = Meta {
            items: vec![iden_item(2)],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 2,
                to_ids: vec![3],
            }],
            ..Meta::default()
        };
        assert!(resolve_derived_region_items(&meta).is_empty());
    }
}
