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

use oxideav_core::{Error, Result};

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

const AV1C: BoxType = b(b"av1C");
const ISPE: BoxType = b(b"ispe");
const COLR: BoxType = b(b"colr");
const PIXI: BoxType = b(b"pixi");
const PASP: BoxType = b(b"pasp");

/// One `infe` entry.
#[derive(Clone, Debug)]
pub struct ItemInfo {
    pub id: u32,
    pub item_type: BoxType,
    pub name: String,
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
#[derive(Clone, Debug)]
pub struct Pixi {
    pub bits_per_channel: Vec<u8>,
}

/// Pixel aspect ratio (`pasp`).
#[derive(Clone, Copy, Debug)]
pub struct Pasp {
    pub h_spacing: u32,
    pub v_spacing: u32,
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

/// One property box, kept typed for the four AVIF cares about + a raw
/// fallback so an unknown property still gets an index for association.
#[derive(Clone, Debug)]
pub enum Property {
    Av1C(Vec<u8>),
    Ispe(Ispe),
    Colr(Colr),
    Pixi(Pixi),
    Pasp(Pasp),
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
            Property::Other(t, _) => *t,
        }
    }
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
                _ => {}
            }
        }
        Ok(me)
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
    // Remaining fields (content_type, URI, …) depend on item_type; we
    // don't need them for AVIF decoding.
    let _ = cursor;
    Ok(ItemInfo {
        id,
        item_type,
        name,
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
}
