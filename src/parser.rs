//! Top-level AVIF file walk: `ftyp` brand check → `meta` parse →
//! resolve the primary item's payload inside `mdat` → return the AV1
//! OBU byte slice + the properties the downstream decoder needs.

use crate::derived::Mif1Compliance;
use crate::error::{AvifError as Error, Result};

use crate::box_parser::{b, iter_boxes, parse_full_box, read_u32, type_str, BoxType};
use crate::meta::{
    Cclv, Clli, Colr, Ispe, ItemInfo, ItemLocation, Mdcv, Meta, Pasp, Pixi, Property,
};

const FTYP: BoxType = b(b"ftyp");
const META: BoxType = b(b"meta");
const MDAT: BoxType = b(b"mdat");
const HDLR: BoxType = b(b"hdlr");
const PITM: BoxType = b(b"pitm");
const IINF: BoxType = b(b"iinf");
const INFE: BoxType = b(b"infe");
const ILOC: BoxType = b(b"iloc");
const IPRP: BoxType = b(b"iprp");

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
    /// Mastering display colour volume (SMPTE ST 2086 / CTA-861-G HDR).
    pub mdcv: Option<Mdcv>,
    /// Content light level info (MaxCLL / MaxFALL in cd/m²).
    pub clli: Option<Clli>,
    /// Colour volume luminance (`cclv` draft extension, same semantics as `clli`).
    pub cclv: Option<Cclv>,
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

/// Audit a file against the HEIF §10.2.1.1 `mif1` structural-brand
/// requirements. The returned [`Mif1Compliance`] is informational —
/// every flag is reported even when some are missing, so a caller that
/// wants a strict pass / fail can decide per-flag.
///
/// We accept files that fail strict mif1 (lots of in-the-wild AVIFs
/// omit one or another mif1 mandatory box) — this is a deliberate
/// helper for callers who want stricter validation than the default
/// reader path.
///
/// `claims_mif1` is set when `mif1` appears in either major_brand or
/// the compatible_brands array. Files that don't claim mif1 can still
/// be audited — the flag distinguishes "this file says it's mif1 and
/// fails" from "this file makes no mif1 claim and so missing boxes
/// are fine."
pub fn audit_mif1(file: &[u8]) -> Result<Mif1Compliance> {
    // Walk top-level boxes for ftyp + meta location.
    let mut ftyp_payload: Option<&[u8]> = None;
    let mut meta_payload: Option<&[u8]> = None;
    for hdr in iter_boxes(file) {
        let hdr = hdr?;
        let payload = &file[hdr.payload_start..hdr.end()];
        match &hdr.box_type {
            x if x == &FTYP => ftyp_payload = Some(payload),
            x if x == &META => meta_payload = Some(payload),
            _ => {}
        }
    }
    let mut out = Mif1Compliance::default();
    let Some(ftyp) = ftyp_payload else {
        return Err(Error::invalid("avif: audit_mif1 missing ftyp"));
    };
    let (major, _minor, compat) = parse_ftyp(ftyp)?;
    out.claims_mif1 = major == BRAND_MIF1 || compat.contains(&BRAND_MIF1);
    let Some(meta_p) = meta_payload else {
        return Ok(out);
    };
    let (_v, _f, meta_body) = parse_full_box(meta_p)?;
    // mif1 requires hdlr, pitm, iinf (with at least one infe), iloc,
    // iprp directly under the meta box. Walk the body once.
    for hdr in iter_boxes(meta_body) {
        let hdr = hdr?;
        let payload = &meta_body[hdr.payload_start..hdr.end()];
        match &hdr.box_type {
            x if x == &HDLR => out.has_hdlr = true,
            x if x == &PITM => out.has_pitm = true,
            x if x == &IINF => {
                out.has_iinf = true;
                out.infe_count = count_infe(payload)?;
            }
            x if x == &ILOC => out.has_iloc = true,
            x if x == &IPRP => out.has_iprp = true,
            _ => {}
        }
    }
    Ok(out)
}

/// Count `infe` children inside an `iinf` payload without fully
/// parsing each one. Used by [`audit_mif1`].
fn count_infe(payload: &[u8]) -> Result<usize> {
    let (version, _flags, body) = parse_full_box(payload)?;
    // header carries entry_count (u16 for v0, u32 for v1) then the
    // declared number of infe children. Use the declared count when it
    // matches reality; otherwise walk for safety.
    let _declared = match version {
        0 => {
            if body.len() < 2 {
                0
            } else {
                u16::from_be_bytes([body[0], body[1]]) as usize
            }
        }
        _ => {
            if body.len() < 4 {
                0
            } else {
                u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize
            }
        }
    };
    // Walk for the actual count — robust against malformed entry_count.
    let start = if version == 0 { 2 } else { 4 };
    if body.len() < start {
        return Ok(0);
    }
    let tail = &body[start..];
    let mut n = 0;
    for hdr in iter_boxes(tail) {
        let hdr = hdr?;
        if hdr.box_type == INFE {
            n += 1;
        }
    }
    Ok(n)
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
    let mdcv = match meta.property_for(primary_id, b"mdcv") {
        Some(Property::Mdcv(v)) => Some(*v),
        _ => None,
    };
    let clli = match meta.property_for(primary_id, b"clli") {
        Some(Property::Clli(v)) => Some(*v),
        _ => None,
    };
    let cclv = match meta.property_for(primary_id, b"cclv") {
        Some(Property::Cclv(v)) => Some(*v),
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
        mdcv,
        clli,
        cclv,
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
    match loc.extents.len() {
        0 => Err(Error::invalid("avif: iloc entry has no extents")),
        1 => {
            // Fast path: single extent — return a slice directly with no allocation.
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
        _ => {
            // Multi-extent items: concatenate all extents. HEIF §8.11.3.3
            // requires that the decoder assemble the item data by appending
            // extents in order. The caller gets a fresh allocation — the
            // returned `&[u8]` is a reference to the owned buffer stored in
            // a leak-safe place (see `item_bytes_owned`).
            //
            // We surface this as an error and provide a separate
            // `item_bytes_owned` helper that returns a `Vec<u8>`. Callers
            // that handle multi-extent items call `item_bytes_owned` directly.
            //
            // The single caller of `resolve_item_bytes` that may encounter
            // multi-extent items is `item_bytes` which is used in the grid
            // and alpha paths. Grid tile items in the wild are always
            // single-extent (each tile is one contiguous mdat run), so this
            // path is extremely rare. Surface Unsupported until a real file
            // exercises the path in production.
            Err(Error::unsupported(format!(
                "avif: item {} has {} extents; multi-extent items require item_bytes_owned()",
                loc.id,
                loc.extents.len()
            )))
        }
    }
}

/// Resolve an item's payload bytes, concatenating multiple extents when
/// necessary. This may allocate when the item spans more than one extent.
///
/// For the common single-extent case this calls [`item_bytes`] internally
/// and returns the slice as an owned copy — a small allocation to keep the
/// API uniform. Callers that want zero-copy access for single-extent items
/// should use [`item_bytes`] directly.
pub fn item_bytes_owned(file: &[u8], loc: &ItemLocation) -> Result<Vec<u8>> {
    if loc.construction_method != 0 {
        return Err(Error::unsupported(format!(
            "avif: iloc construction_method {} not supported",
            loc.construction_method
        )));
    }
    if loc.extents.is_empty() {
        return Err(Error::invalid("avif: iloc entry has no extents"));
    }
    if loc.extents.len() == 1 {
        // Common path: single extent — delegate to slice resolver.
        return item_bytes(file, loc).map(|s| s.to_vec());
    }
    // Multi-extent: concatenate all extents in order (HEIF §8.11.3.3).
    let mut out = Vec::new();
    for (i, e) in loc.extents.iter().enumerate() {
        let start = loc
            .base_offset
            .checked_add(e.offset)
            .ok_or_else(|| Error::invalid(format!("avif: iloc extent {i} offset overflow")))?;
        let end = start
            .checked_add(e.length)
            .ok_or_else(|| Error::invalid(format!("avif: iloc extent {i} length overflow")))?;
        let (start, end) = (start as usize, end as usize);
        if end > file.len() {
            return Err(Error::invalid(format!(
                "avif: iloc extent {i} {start}..{end} exceeds file length {}",
                file.len()
            )));
        }
        out.extend_from_slice(&file[start..end]);
    }
    Ok(out)
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
        // SDR fixture carries no HDR metadata.
        assert!(img.mdcv.is_none(), "SDR fixture must not have mdcv");
        assert!(img.clli.is_none(), "SDR fixture must not have clli");
        assert!(img.cclv.is_none(), "SDR fixture must not have cclv");
    }

    /// `item_bytes_owned` on a single-extent iloc returns the same data
    /// as `item_bytes`. This path allocates but produces equal bytes.
    #[test]
    fn item_bytes_owned_single_extent_matches_item_bytes() {
        use crate::meta::{IlocExtent, ItemLocation};
        let data = b"hello world";
        // Synthetic loc: base_offset 0, extent offset=0, length=11.
        let loc = ItemLocation {
            id: 1,
            construction_method: 0,
            data_reference_index: 0,
            base_offset: 0,
            extents: vec![IlocExtent {
                offset: 0,
                length: 11,
            }],
        };
        let slice = item_bytes(data, &loc).unwrap();
        let owned = item_bytes_owned(data, &loc).unwrap();
        assert_eq!(slice, owned.as_slice());
        assert_eq!(owned, b"hello world");
    }

    /// `item_bytes_owned` on a multi-extent iloc concatenates all extents.
    #[test]
    fn item_bytes_owned_multi_extent_concatenates() {
        use crate::meta::{IlocExtent, ItemLocation};
        let data = b"AAAA____BBBB"; // 12 bytes; extents at 0..4 and 8..12
        let loc = ItemLocation {
            id: 1,
            construction_method: 0,
            data_reference_index: 0,
            base_offset: 0,
            extents: vec![
                IlocExtent {
                    offset: 0,
                    length: 4,
                },
                IlocExtent {
                    offset: 8,
                    length: 4,
                },
            ],
        };
        let owned = item_bytes_owned(data, &loc).unwrap();
        assert_eq!(owned, b"AAAABBBB");
        // item_bytes must return Unsupported for multi-extent.
        let err = item_bytes(data, &loc).unwrap_err();
        assert!(
            matches!(err, crate::error::AvifError::Unsupported(_)),
            "item_bytes must return Unsupported for multi-extent: {err:?}"
        );
    }

    /// `item_bytes_owned` rejects construction_method != 0.
    #[test]
    fn item_bytes_owned_rejects_idat_method() {
        use crate::meta::{IlocExtent, ItemLocation};
        let loc = ItemLocation {
            id: 1,
            construction_method: 1, // idat-based
            data_reference_index: 0,
            base_offset: 0,
            extents: vec![IlocExtent {
                offset: 0,
                length: 4,
            }],
        };
        let err = item_bytes_owned(b"data", &loc).unwrap_err();
        assert!(matches!(err, crate::error::AvifError::Unsupported(_)));
    }

    /// `audit_mif1` against the Microsoft monochrome AVIF fixture
    /// reports full mif1 compliance plus the mif1 brand claim.
    /// The fixture's `ftyp` is `avif` major with `mif1` in
    /// compatible_brands and its `meta` ships every mif1-mandatory
    /// child box (hdlr / pitm / iinf with one infe / iloc / iprp).
    #[test]
    fn audit_mif1_monochrome_fixture_is_compliant() {
        let m = audit_mif1(FIXTURE).expect("audit");
        assert!(m.claims_mif1, "fixture has mif1 in compatible_brands");
        assert!(m.has_hdlr);
        assert!(m.has_pitm);
        assert!(m.has_iinf);
        assert!(m.has_iloc);
        assert!(m.has_iprp);
        assert!(m.infe_count >= 1);
        assert!(m.is_compliant());
        assert!(m.missing().is_empty());
    }

    /// `audit_mif1` on bytes with no meta box reports `claims_mif1`
    /// from the ftyp and every other flag false. Useful gate for
    /// "is this even a plausible mif1 carrier".
    #[test]
    fn audit_mif1_ftyp_only_reports_brand_no_boxes() {
        // Synth ftyp: major='mif1' minor=0 compat=[mif1]
        let mut buf = Vec::new();
        let payload_len: u32 = 4 + 4 + 4; // major + minor + one compat
        let size: u32 = 8 + payload_len;
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"ftyp");
        buf.extend_from_slice(b"mif1");
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(b"mif1");
        let m = audit_mif1(&buf).expect("audit");
        assert!(m.claims_mif1);
        assert!(!m.is_compliant());
        let missing = m.missing();
        assert!(missing.contains(&"hdlr"));
        assert!(missing.contains(&"pitm"));
    }
}
