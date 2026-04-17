//! Top-level AVIF file walk: `ftyp` brand check → `meta` parse →
//! resolve the primary item's payload inside `mdat` → return the AV1
//! OBU byte slice + the properties the downstream decoder needs.

use oxideav_core::{Error, Result};

use crate::box_parser::{b, iter_boxes, read_u32, type_str, BoxType};
use crate::meta::{Colr, Ispe, ItemInfo, ItemLocation, Meta, Pasp, Pixi, Property};

const FTYP: BoxType = b(b"ftyp");
const META: BoxType = b(b"meta");
const MDAT: BoxType = b(b"mdat");

const BRAND_AVIF: BoxType = b(b"avif");
const BRAND_AVIS: BoxType = b(b"avis");
const BRAND_MIF1: BoxType = b(b"mif1");
const BRAND_MSF1: BoxType = b(b"msf1");
const BRAND_MIAF: BoxType = b(b"miaf");

const ITEM_TYPE_AV01: BoxType = b(b"av01");

/// Decoded AVIF file, ready for hand-off to an AV1 OBU decoder.
#[derive(Clone, Debug)]
pub struct AvifImage<'a> {
    pub major_brand: BoxType,
    pub minor_version: u32,
    pub compatible_brands: Vec<BoxType>,
    pub meta: Meta,
    pub primary_item_id: u32,
    pub primary_item: ItemInfo,
    /// AV1 OBU bitstream for the primary item (a slice into the file).
    pub primary_item_data: &'a [u8],
    /// `av1C` configuration record body (the 4-byte AV1CodecConfigurationRecord
    /// prefix + embedded config OBUs). `None` if the property store
    /// doesn't ship one — a malformed file.
    pub av1c: Option<Vec<u8>>,
    pub ispe: Option<Ispe>,
    pub colr: Option<Colr>,
    pub pixi: Option<Pixi>,
    pub pasp: Option<Pasp>,
}

/// Parse an AVIF file, returning the bits needed to decode the primary
/// image. The input must contain the whole file; box offsets in `iloc`
/// are absolute.
pub fn parse(file: &[u8]) -> Result<AvifImage<'_>> {
    // Walk top-level boxes, collecting what we need.
    let mut ftyp_payload: Option<&[u8]> = None;
    let mut meta_payload: Option<&[u8]> = None;
    for hdr in iter_boxes(file) {
        let hdr = hdr?;
        let payload = &file[hdr.payload_start..hdr.end()];
        match &hdr.box_type {
            x if x == &FTYP => ftyp_payload = Some(payload),
            x if x == &META => meta_payload = Some(payload),
            x if x == &MDAT => {}
            _ => {}
        }
    }
    let ftyp = ftyp_payload.ok_or_else(|| Error::invalid("avif: missing ftyp"))?;
    let (major_brand, minor_version, compatible_brands) = parse_ftyp(ftyp)?;

    // AVIF profile check — accept any file whose brand set contains at
    // least one of the AVIF / HEIF image brands.
    if !is_avif_compatible(&major_brand, &compatible_brands) {
        return Err(Error::invalid(format!(
            "avif: ftyp major='{}' compatible_brands={} doesn't claim any AVIF/HEIF brand",
            type_str(&major_brand),
            compatible_brands
                .iter()
                .map(type_str)
                .collect::<Vec<_>>()
                .join(",")
        )));
    }

    let meta_p = meta_payload.ok_or_else(|| Error::invalid("avif: missing meta"))?;
    let meta = Meta::parse(meta_p)?;

    let primary_id = meta
        .primary_item_id
        .ok_or_else(|| Error::invalid("avif: missing pitm"))?;
    let primary_info = meta
        .item_by_id(primary_id)
        .ok_or_else(|| Error::invalid("avif: pitm references unknown item"))?
        .clone();
    if primary_info.item_type != ITEM_TYPE_AV01 {
        return Err(Error::unsupported(format!(
            "avif: primary item type '{}' != 'av01'",
            type_str(&primary_info.item_type)
        )));
    }

    let loc = meta
        .location_by_id(primary_id)
        .ok_or_else(|| Error::invalid("avif: primary item missing in iloc"))?;
    let primary_data = resolve_item_bytes(file, loc)?;

    let av1c = match meta.property_for(primary_id, b"av1C") {
        Some(Property::Av1C(bytes)) => Some(bytes.clone()),
        _ => None,
    };
    let ispe = match meta.property_for(primary_id, b"ispe") {
        Some(Property::Ispe(v)) => Some(*v),
        _ => None,
    };
    let colr = match meta.property_for(primary_id, b"colr") {
        Some(Property::Colr(v)) => Some(v.clone()),
        _ => None,
    };
    let pixi = match meta.property_for(primary_id, b"pixi") {
        Some(Property::Pixi(v)) => Some(v.clone()),
        _ => None,
    };
    let pasp = match meta.property_for(primary_id, b"pasp") {
        Some(Property::Pasp(v)) => Some(*v),
        _ => None,
    };

    Ok(AvifImage {
        major_brand,
        minor_version,
        compatible_brands,
        meta,
        primary_item_id: primary_id,
        primary_item: primary_info,
        primary_item_data: primary_data,
        av1c,
        ispe,
        colr,
        pixi,
        pasp,
    })
}

fn parse_ftyp(payload: &[u8]) -> Result<(BoxType, u32, Vec<BoxType>)> {
    if payload.len() < 8 {
        return Err(Error::invalid("avif: ftyp too short"));
    }
    let mut major = [0u8; 4];
    major.copy_from_slice(&payload[..4]);
    let minor = read_u32(payload, 4)?;
    let mut brands = Vec::new();
    let mut cursor = 8;
    while cursor + 4 <= payload.len() {
        let mut b4 = [0u8; 4];
        b4.copy_from_slice(&payload[cursor..cursor + 4]);
        brands.push(b4);
        cursor += 4;
    }
    Ok((major, minor, brands))
}

fn is_avif_compatible(major: &BoxType, compat: &[BoxType]) -> bool {
    let candidates = [major].into_iter().chain(compat.iter());
    for b in candidates {
        if b == &BRAND_AVIF || b == &BRAND_AVIS || b == &BRAND_MIF1 || b == &BRAND_MSF1
            || b == &BRAND_MIAF
        {
            return true;
        }
    }
    false
}

fn resolve_item_bytes<'a>(file: &'a [u8], loc: &ItemLocation) -> Result<&'a [u8]> {
    if loc.construction_method != 0 {
        return Err(Error::unsupported(format!(
            "avif: iloc construction_method {} not supported (only file-offset is handled)",
            loc.construction_method
        )));
    }
    // AVIF primary items normally carry a single extent; if they don't
    // we'd need to concatenate. Surface a clean error until a real file
    // exercises that path.
    match loc.extents.len() {
        0 => Err(Error::invalid("avif: iloc entry has no extents")),
        1 => {
            let e = &loc.extents[0];
            let start = loc
                .base_offset
                .checked_add(e.offset)
                .ok_or_else(|| Error::invalid("avif: iloc offset overflow"))?;
            let end = start
                .checked_add(e.length)
                .ok_or_else(|| Error::invalid("avif: iloc length overflow"))?;
            let (start, end) = (start as usize, end as usize);
            if end > file.len() {
                return Err(Error::invalid(format!(
                    "avif: iloc extent {start}..{end} exceeds file length {}",
                    file.len()
                )));
            }
            Ok(&file[start..end])
        }
        _ => Err(Error::unsupported(
            "avif: multi-extent items not yet handled",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/monochrome.avif");

    #[test]
    fn parses_monochrome_fixture() {
        let img = parse(FIXTURE).expect("parse avif");
        assert_eq!(&img.major_brand, b"avif");
        assert!(img.compatible_brands.iter().any(|b| b == b"mif1"));
        assert_eq!(img.primary_item_id, 1);
        assert_eq!(&img.primary_item.item_type, b"av01");
        let ispe = img.ispe.expect("ispe for primary item");
        assert_eq!(ispe.width, 1280);
        assert_eq!(ispe.height, 720);
        // av1C starts with marker(1) | version(7) = 0x81.
        let av1c = img.av1c.as_deref().expect("av1C property");
        assert_eq!(av1c[0], 0x81);
        // The primary item bytes live in mdat and begin with an AV1
        // temporal-delimiter OBU (obu_type=2). Bit layout of the OBU
        // header byte: obu_forbidden(1) | obu_type(4) | obu_extension_flag(1)
        // | obu_has_size_field(1) | obu_reserved(1) — TD is type 2, and
        // this aom-encoded file sets has_size (low bit before reserved).
        let obu_type = (img.primary_item_data[0] >> 3) & 0x0f;
        assert_eq!(obu_type, 2, "first OBU should be temporal delimiter");
        // Monochrome file has a pixi with one 8-bit channel.
        let pixi = img.pixi.as_ref().expect("pixi");
        assert_eq!(pixi.bits_per_channel, vec![8]);
    }
}
