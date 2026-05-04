//! Top-level AVIF file walk: `ftyp` brand check → `meta` parse →
//! resolve the primary item's payload inside `mdat` → return the AV1
//! OBU byte slice + the properties the downstream decoder needs.

use crate::error::{AvifError as Error, Result};

use crate::box_parser::{b, iter_boxes, read_u32, type_str, BoxType};
use crate::meta::{Colr, Ispe, ItemInfo, ItemLocation, Meta, Pasp, Pixi, Property};

const FTYP: BoxType = b(b"ftyp");
const META: BoxType = b(b"meta");
const MDAT: BoxType = b(b"mdat");

/// `avif` — AV1 image / image collection brand (av1-avif §6.2).
pub const BRAND_AVIF: BoxType = b(b"avif");
/// `avis` — AV1 image sequence brand (av1-avif §6.3).
pub const BRAND_AVIS: BoxType = b(b"avis");
/// `avio` — AV1 image / sequence intra-only profile (av1-avif §6.2 / §6.3).
pub const BRAND_AVIO: BoxType = b(b"avio");
/// `mif1` — HEIF image-item structural brand (HEIF §10.2.2).
pub const BRAND_MIF1: BoxType = b(b"mif1");
/// `msf1` — HEIF image-sequence structural brand (HEIF §10.3.2).
pub const BRAND_MSF1: BoxType = b(b"msf1");
/// `miaf` — Multi-image Application Format compatibility brand
/// (ISO/IEC 23000-22 §7).
pub const BRAND_MIAF: BoxType = b(b"miaf");
/// `MA1B` — AVIF Baseline Profile (av1-avif §8.2).
pub const BRAND_MA1B: BoxType = b(b"MA1B");
/// `MA1A` — AVIF Advanced Profile (av1-avif §8.3).
pub const BRAND_MA1A: BoxType = b(b"MA1A");

pub const ITEM_TYPE_AV01: BoxType = b(b"av01");
pub const ITEM_TYPE_GRID: BoxType = b(b"grid");

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

/// Header-only parse that stops after `ftyp` + `meta` have been walked.
/// Used by callers (grid composition, AVIS) that need access to the
/// full `Meta` without committing to the single-av01-item contract
/// [`parse`] enforces.
pub struct AvifHeader<'a> {
    pub file: &'a [u8],
    pub major_brand: BoxType,
    pub minor_version: u32,
    pub compatible_brands: Vec<BoxType>,
    pub meta: Meta,
}

pub fn parse_header(file: &[u8]) -> Result<AvifHeader<'_>> {
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
    classify_brands(&major_brand, &compatible_brands)?;
    let meta_p = meta_payload.ok_or_else(|| Error::invalid("avif: missing meta"))?;
    let meta = Meta::parse(meta_p)?;
    Ok(AvifHeader {
        file,
        major_brand,
        minor_version,
        compatible_brands,
        meta,
    })
}

/// Resolve an item's payload bytes via its `iloc` entry. Public so the
/// grid / alpha paths can independently fetch tile + alpha items.
pub fn item_bytes<'a>(file: &'a [u8], loc: &ItemLocation) -> Result<&'a [u8]> {
    resolve_item_bytes(file, loc)
}

/// Parse an AVIF file, returning the bits needed to decode the primary
/// image. The input must contain the whole file; box offsets in `iloc`
/// are absolute.
///
/// This path errors out when the primary item type is not `av01` — use
/// [`parse_header`] + the `grid` module to handle grid primaries.
pub fn parse(file: &[u8]) -> Result<AvifImage<'_>> {
    let hdr = parse_header(file)?;
    let AvifHeader {
        file: _,
        major_brand,
        minor_version,
        compatible_brands,
        meta,
    } = hdr;

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

/// Classification of an AVIF / HEIF `ftyp` box per av1-avif §6 + §7 +
/// §8 and ISO/IEC 23000-22 (MIAF) §7. Surfaces both the structural
/// brand the file claims and the optional AVIF profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BrandClass {
    /// File declares an AV1 image / image-collection (`avif`) brand.
    pub is_image: bool,
    /// File declares an AV1 image sequence (`avis`) brand.
    pub is_sequence: bool,
    /// File declares the intra-only profile (`avio`).
    pub is_intra_only: bool,
    /// File declares the MIAF compatibility brand (`miaf`,
    /// ISO/IEC 23000-22 §7).
    pub is_miaf: bool,
    /// HEIF structural brand: image-item (`mif1`).
    pub has_mif1: bool,
    /// HEIF structural brand: image-sequence (`msf1`).
    pub has_msf1: bool,
    /// AVIF Baseline Profile brand (`MA1B`).
    pub is_baseline_profile: bool,
    /// AVIF Advanced Profile brand (`MA1A`).
    pub is_advanced_profile: bool,
}

/// Classify the brand set of a parsed `ftyp` box. Errors when the file
/// claims neither `avif` nor `avis` nor any HEIF structural brand
/// (`mif1` / `msf1`) — without one of those there is nothing AVIF /
/// HEIF can do with the file. The error string lists every brand seen
/// so the caller doesn't have to dig the bytes back out.
///
/// Spec references:
///
/// * av1-avif §6.2 — `avif` for AV1 image / image collection
/// * av1-avif §6.3 — `avis` for AV1 image sequences
/// * av1-avif §6.2 / §6.3 — `avio` intra-only
/// * av1-avif §7 General constraints — file shall list `miaf`
/// * av1-avif §8.2 / §8.3 — `MA1B` Baseline / `MA1A` Advanced profile
/// * HEIF §10.2.2 — `mif1` image-item structural brand
/// * HEIF §10.3.2 — `msf1` image-sequence structural brand
/// * ISO/IEC 23000-22 §7 — `miaf` compatibility brand
pub fn classify_brands(major: &BoxType, compat: &[BoxType]) -> Result<BrandClass> {
    let mut cls = BrandClass::default();
    let candidates: Vec<&BoxType> = std::iter::once(major).chain(compat.iter()).collect();
    for brand in &candidates {
        match **brand {
            x if x == BRAND_AVIF => cls.is_image = true,
            x if x == BRAND_AVIS => cls.is_sequence = true,
            x if x == BRAND_AVIO => cls.is_intra_only = true,
            x if x == BRAND_MIAF => cls.is_miaf = true,
            x if x == BRAND_MIF1 => cls.has_mif1 = true,
            x if x == BRAND_MSF1 => cls.has_msf1 = true,
            x if x == BRAND_MA1B => cls.is_baseline_profile = true,
            x if x == BRAND_MA1A => cls.is_advanced_profile = true,
            _ => {}
        }
    }
    // Files in the wild often omit `miaf` even though av1-avif §7
    // requires it. We accept any combination that includes either
    // an AVIF brand (`avif`/`avis`) or a HEIF structural brand
    // (`mif1`/`msf1`) — anything less can't be a real AVIF / HEIF
    // file. Strict MIAF compliance can be checked via the returned
    // `BrandClass.is_miaf` flag.
    let usable =
        cls.is_image || cls.is_sequence || cls.has_mif1 || cls.has_msf1 || cls.is_intra_only;
    if !usable {
        return Err(Error::invalid(format!(
            "avif: ftyp major='{}' compatible_brands=[{}] declares no AVIF/HEIF brand \
             (need one of avif, avis, avio, mif1, msf1)",
            type_str(major),
            compat.iter().map(type_str).collect::<Vec<_>>().join(",")
        )));
    }
    Ok(cls)
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
    fn classify_brands_baseline_profile() {
        // Real `monochrome.avif` ftyp: major=avif compat=[mif1,avif,miaf,MA1B].
        let cls = classify_brands(
            &BRAND_AVIF,
            &[BRAND_MIF1, BRAND_AVIF, BRAND_MIAF, BRAND_MA1B],
        )
        .unwrap();
        assert!(cls.is_image);
        assert!(cls.is_miaf);
        assert!(cls.has_mif1);
        assert!(cls.is_baseline_profile);
        assert!(!cls.is_sequence);
        assert!(!cls.is_advanced_profile);
    }

    #[test]
    fn classify_brands_advanced_profile() {
        let cls = classify_brands(&BRAND_AVIF, &[BRAND_MIF1, BRAND_MIAF, BRAND_MA1A]).unwrap();
        assert!(cls.is_advanced_profile);
        assert!(!cls.is_baseline_profile);
        assert!(cls.is_image);
    }

    #[test]
    fn classify_brands_sequence_with_intra_only() {
        // Per av1-avif §6.3 + §8.2: avis + avio + msf1 + miaf + MA1B.
        let cls = classify_brands(
            &BRAND_AVIS,
            &[BRAND_AVIO, BRAND_MSF1, BRAND_MIAF, BRAND_MA1B],
        )
        .unwrap();
        assert!(cls.is_sequence);
        assert!(cls.is_intra_only);
        assert!(cls.has_msf1);
        assert!(cls.is_miaf);
        assert!(cls.is_baseline_profile);
        assert!(!cls.is_image);
    }

    #[test]
    fn classify_brands_rejects_no_avif_or_heif_brand() {
        // `mp42` + `isom` is a valid generic ISOBMFF ftyp but says
        // nothing about AVIF/HEIF.
        let err = classify_brands(b"mp42", &[*b"isom"]).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("declares no AVIF/HEIF brand"),
            "expected AVIF/HEIF rejection, got: {msg}"
        );
        // Error must list the brands seen so the caller can debug.
        assert!(msg.contains("mp42"));
        assert!(msg.contains("isom"));
    }

    #[test]
    fn classify_brands_accepts_mif1_only() {
        // Pure HEIF file with no AVIF brand still parses: caller can
        // inspect the meta to find out whether it carries an `av01` item.
        let cls = classify_brands(&BRAND_MIF1, &[]).unwrap();
        assert!(cls.has_mif1);
        assert!(!cls.is_image);
        assert!(!cls.is_sequence);
    }

    #[test]
    fn classify_brands_lone_miaf_without_anchor_is_rejected() {
        // `miaf` alone isn't enough — a file must also claim either an
        // AVIF brand (`avif` / `avis` / `avio`) or a HEIF structural
        // brand (`mif1` / `msf1`).
        let err = classify_brands(&BRAND_MIAF, &[]).unwrap_err();
        assert!(format!("{err}").contains("declares no AVIF/HEIF brand"));
    }

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
