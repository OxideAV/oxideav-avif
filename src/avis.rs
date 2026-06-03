//! AVIS (AVIF image sequences) sample-table walk.
//!
//! An AVIS file layers an ISO/IEC 14496-12 movie box (`moov`) on top of
//! the AVIF still-image container. Each frame of the sequence is a
//! sample in a single video track; sample byte ranges are recovered by
//! walking `stbl` (`stts`, `stsc`, `stsz`, `stco`/`co64`, optional
//! `stss`). Display dimensions come from `tkhd`; the movie timescale
//! comes from `mvhd`.
//!
//! This module's job is strictly container-side: it does not feed the
//! AV1 decoder, it just produces a flat [`Sample`] table + presentation
//! metadata. The caller pairs the table with a standard
//! [`oxideav_av1::Av1Decoder`] to decode frames end-to-end. The
//! decoder needs the track's `AV1CodecConfigurationRecord` to seed its
//! sequence header — that record is extracted from `stsd` → `av01` →
//! `av1C` and surfaced as [`AvisMeta::av1_codec_config`].

use crate::error::{AvifError as Error, Result};

use crate::box_parser::{b, find_box, iter_boxes, parse_full_box, read_u32, read_u64, BoxType};

const MOOV: BoxType = b(b"moov");
const MVHD: BoxType = b(b"mvhd");
const TRAK: BoxType = b(b"trak");
const TKHD: BoxType = b(b"tkhd");
const EDTS: BoxType = b(b"edts");
const ELST: BoxType = b(b"elst");
const MDIA: BoxType = b(b"mdia");
const HDLR: BoxType = b(b"hdlr");
const MINF: BoxType = b(b"minf");
const STBL: BoxType = b(b"stbl");
const STTS: BoxType = b(b"stts");
const STSC: BoxType = b(b"stsc");
const STSZ: BoxType = b(b"stsz");
const STCO: BoxType = b(b"stco");
const CO64: BoxType = b(b"co64");
const STSS: BoxType = b(b"stss");
const STSD: BoxType = b(b"stsd");
const AV01: BoxType = b(b"av01");
const AV1C: BoxType = b(b"av1C");

/// Four-CC for the picture track handler (ISO/IEC 14496-12 §8.4.3).
/// `mdia/hdlr/handler_type` carries this for any image sequence track,
/// and av1-avif v1.2.0 §3 requires it for an AV1 Image Sequence track.
pub const HANDLER_PICT: BoxType = *b"pict";

/// One sample in the AVIS track. `offset` is absolute inside the source
/// file; `size` is the sample's byte length; `duration` is expressed in
/// the movie's timescale (see [`AvisMeta::timescale`]). `is_sync` flags
/// sync samples — keyframes that can be decoded standalone. When `stss`
/// is absent every sample is a sync sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    pub duration: u32,
    pub is_sync: bool,
}

/// One `elst` entry: a single segment of the track's presentation
/// timeline.
///
/// Spec: ISO/IEC 14496-12 §8.6.6 (`EditListBox`). Each entry is one of
/// three shapes:
///
/// * **Normal segment** — `media_time >= 0` and `media_rate_integer ==
///   1`. The slice of the media starting at `media_time` (in media
///   timescale units) plays for `segment_duration` (movie timescale
///   units) at native rate.
/// * **Empty edit** — `media_time == -1`. The presentation timeline
///   advances by `segment_duration` while no media is presented (used
///   to offset a track's start). §8.6.6.3: "The last edit in a track
///   shall never be an empty edit."
/// * **Dwell** — `media_rate_integer == 0`. The single media frame at
///   `media_time` is held for `segment_duration`. §8.6.6.3 constrains
///   the rate field: "Otherwise this field shall contain the value 1"
///   (i.e. `media_rate_integer` is exactly `0` or exactly `1`).
///
/// `segment_duration` and `media_time` widen v0's 32-bit fields to the
/// v1 64-bit shape so the entry shape stays version-agnostic for
/// callers. `media_rate_fraction` is preserved as a diagnostic — the
/// spec sets it to `0` and gives no use for non-zero values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditListEntry {
    /// `segment_duration` in movie-timescale (`mvhd::timescale`) units.
    pub segment_duration: u64,
    /// `media_time` in media-timescale (`mdhd::timescale`, not surfaced
    /// here) units. `-1` flags an empty edit. v0 sign-extends the
    /// signed-32 wire field to `i64`.
    pub media_time: i64,
    /// `media_rate_integer` from the wire (16-bit signed; almost
    /// always `0` for dwell or `1` for normal-rate playback).
    pub media_rate_integer: i16,
    /// `media_rate_fraction` from the wire (16-bit signed; spec
    /// equation: rate = `integer + fraction / 65536`. Almost always
    /// `0`).
    pub media_rate_fraction: i16,
}

impl EditListEntry {
    /// `true` when this entry signals an empty edit
    /// (`media_time == -1`, §8.6.6.3): the presentation advances by
    /// `segment_duration` with no media presented.
    pub fn is_empty_edit(&self) -> bool {
        self.media_time == -1
    }

    /// `true` when this entry signals a dwell (`media_rate_integer ==
    /// 0`, §8.6.6.3): the single frame at `media_time` is held for
    /// `segment_duration`.
    pub fn is_dwell(&self) -> bool {
        self.media_rate_integer == 0
    }
}

/// Container-side description of an AVIS image sequence.
#[derive(Clone, Debug)]
pub struct AvisMeta {
    /// Movie timescale from `mvhd`. A duration of `timescale` == 1s.
    pub timescale: u32,
    /// Declared display width / height from `tkhd`. `None` when tkhd is
    /// missing or malformed.
    pub display_dims: Option<(u32, u32)>,
    /// Ordered list of sample byte-ranges + durations + sync flags.
    pub samples: Vec<Sample>,
    /// Raw `AV1CodecConfigurationRecord` bytes extracted from the
    /// track's `stsd` → `av01` → `av1C` chain. `None` when the AVIS
    /// track is not AV1-coded or the config record is missing.
    /// Required by the AV1 decoder to bootstrap the sequence header.
    /// Spec: AV1-AVIF §2.2.1, ISO/IEC 14496-12 §8.5.2 (`stsd`).
    pub av1_codec_config: Option<Vec<u8>>,
    /// `handler_type` four-CC extracted from the track's
    /// `mdia/hdlr` box (ISO/IEC 14496-12 §8.4.3). `None` when the
    /// `hdlr` box is missing or its body is truncated. av1-avif v1.2.0
    /// §3 requires this to equal [`HANDLER_PICT`] (`'pict'`) for an
    /// AV1 Image Sequence track.
    pub handler: Option<BoxType>,
    /// Four-CC sample-entry types decoded from the track's
    /// `stbl/stsd` box in declaration order (ISO/IEC 14496-12 §8.5.2).
    /// For a compliant AV1 Image Sequence (av1-avif v1.2.0 §3) this
    /// list shall be `['av01']`. An empty list signals a missing or
    /// truncated `stsd`.
    pub sample_description_types: Vec<BoxType>,
    /// Entries from the first track's `edts/elst` box in declaration
    /// order (ISO/IEC 14496-12 §8.6.6, `EditListBox`). Empty when the
    /// track carries no `edts` (the §8.6.5 implicit-identity case) or
    /// the `elst` body is truncated. v0 (32-bit `segment_duration` /
    /// signed-32 `media_time`) and v1 (64-bit / signed-64) entries are
    /// widened to a single shape so callers stay version-agnostic;
    /// `audit_edit_list` consumes this field directly.
    pub edit_list: Vec<EditListEntry>,
}

/// Walk the container and build a sample table. The input buffer must
/// contain the full file — sample offsets are absolute.
///
/// Returns `Error::InvalidData` when a required box is missing or
/// inconsistent. AVIS files lacking a `moov` produce an error.
pub fn parse_avis(file: &[u8]) -> Result<AvisMeta> {
    let (moov_payload, _) = find_box(file, &MOOV)?
        .ok_or_else(|| Error::InvalidData("avis: missing moov".to_string()))?;

    let timescale = find_mvhd_timescale(moov_payload).unwrap_or(1000);
    let display_dims = find_tkhd_display_size(moov_payload);
    let handler = find_first_track_handler(moov_payload);
    let edit_list = find_first_track_edit_list(moov_payload);

    // Locate the first track's stbl — AVIS carries a single image track.
    let stbl = find_first_track_stbl(moov_payload)
        .ok_or_else(|| Error::InvalidData("avis: missing trak/mdia/minf/stbl".to_string()))?;
    let samples = sample_table(stbl)?;
    let av1_codec_config = find_av1c_in_stbl(stbl);
    let sample_description_types = sample_description_types_in_stbl(stbl);
    Ok(AvisMeta {
        timescale,
        display_dims,
        samples,
        av1_codec_config,
        handler,
        sample_description_types,
        edit_list,
    })
}

/// Convert a sample duration (in `timescale` units) to a
/// `(num, den)` rational of seconds — the same shape oxideav's
/// [`oxideav_core::TimeBase`] uses.
pub fn sample_duration_seconds(duration: u32, timescale: u32) -> (u32, u32) {
    if timescale == 0 {
        (duration, 1)
    } else {
        (duration, timescale)
    }
}

/// Resolve the byte slice of an AVIS sample inside the source file.
/// Returns `Err(InvalidData)` when the declared `(offset, size)`
/// doesn't fit inside the file. Callers feed this slice to the AV1
/// decoder per sample.
pub fn sample_bytes<'a>(file: &'a [u8], sample: &Sample) -> Result<&'a [u8]> {
    let start = sample.offset as usize;
    let end = sample
        .offset
        .checked_add(sample.size as u64)
        .ok_or_else(|| Error::InvalidData("avis: sample range overflow".to_string()))?
        as usize;
    if end > file.len() {
        return Err(Error::InvalidData(format!(
            "avis: sample {start}..{end} exceeds file length {}",
            file.len()
        )));
    }
    Ok(&file[start..end])
}

/// Find mvhd's timescale. Payload starts with a FullBox header; the
/// timescale lives at payload offset 12 (v0) or 20 (v1).
fn find_mvhd_timescale(moov_payload: &[u8]) -> Option<u32> {
    let (p, _) = find_box(moov_payload, &MVHD).ok()??;
    if p.is_empty() {
        return None;
    }
    let (version, _flags, body) = parse_full_box(p).ok()?;
    match version {
        0 => {
            // creation(4) + modification(4) + timescale(4) + duration(4)
            if body.len() < 16 {
                return None;
            }
            Some(u32::from_be_bytes([body[8], body[9], body[10], body[11]]))
        }
        1 => {
            // creation(8) + modification(8) + timescale(4) + duration(8)
            if body.len() < 28 {
                return None;
            }
            Some(u32::from_be_bytes([body[16], body[17], body[18], body[19]]))
        }
        _ => None,
    }
}

/// Find the first `trak`'s `tkhd` display width + height. tkhd stores
/// width/height as 32.16 fixed-point at offsets 76 (v0) or 88 (v1)
/// from the start of the full payload (not the FullBox body).
fn find_tkhd_display_size(moov_payload: &[u8]) -> Option<(u32, u32)> {
    for hdr in iter_boxes(moov_payload) {
        let hdr = hdr.ok()?;
        if hdr.box_type != TRAK {
            continue;
        }
        let trak_payload = &moov_payload[hdr.payload_start..hdr.end()];
        let (p, _) = find_box(trak_payload, &TKHD).ok()??;
        if p.is_empty() {
            continue;
        }
        let version = p[0];
        let off = match version {
            0 => 76,
            1 => 88,
            _ => continue,
        };
        if p.len() < off + 8 {
            continue;
        }
        let w = u32::from_be_bytes([p[off], p[off + 1], p[off + 2], p[off + 3]]) >> 16;
        let h = u32::from_be_bytes([p[off + 4], p[off + 5], p[off + 6], p[off + 7]]) >> 16;
        return Some((w, h));
    }
    None
}

/// Walk `stbl` → `stsd` → first `av01` SampleEntry → `av1C` and return
/// the raw `AV1CodecConfigurationRecord` byte slice.
///
/// `stsd` layout (ISO/IEC 14496-12 §8.5.2): FullBox header (4 bytes) +
/// `entry_count`(u32) + N SampleEntry boxes packed contiguously. For an
/// AVIS track each SampleEntry is `av01` (a `VisualSampleEntry`,
/// §12.1.3). `VisualSampleEntry` reserves a fixed 78-byte header before
/// any child boxes — `reserved(6) + data_reference_index(2) +
/// pre_defined(2) + reserved(2) + pre_defined(12) + width(2) +
/// height(2) + horizresolution(4) + vertresolution(4) + reserved(4) +
/// frame_count(2) + compressorname(32) + depth(2) + pre_defined(2)`.
/// After that header, child boxes (`av1C`, `pasp`, `colr`, etc.) follow.
fn find_av1c_in_stbl(stbl: &[u8]) -> Option<Vec<u8>> {
    let (stsd_payload, _) = find_box(stbl, &STSD).ok()??;
    let (_v, _f, body) = parse_full_box(stsd_payload).ok()?;
    if body.len() < 4 {
        return None;
    }
    let entry_count = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    if entry_count == 0 {
        return None;
    }
    // Walk the SampleEntry boxes packed after the entry_count.
    let entries = &body[4..];
    for hdr in iter_boxes(entries) {
        let hdr = hdr.ok()?;
        if hdr.box_type != AV01 {
            continue;
        }
        let entry_payload = &entries[hdr.payload_start..hdr.end()];
        // Skip the 78-byte VisualSampleEntry fixed header.
        const VISUAL_HEADER_LEN: usize = 78;
        if entry_payload.len() <= VISUAL_HEADER_LEN {
            return None;
        }
        let children = &entry_payload[VISUAL_HEADER_LEN..];
        let (av1c_payload, _) = find_box(children, &AV1C).ok()??;
        return Some(av1c_payload.to_vec());
    }
    None
}

/// Walk moov/trak/mdia/minf/stbl to the first track's stbl payload.
fn find_first_track_stbl(moov_payload: &[u8]) -> Option<&[u8]> {
    for hdr in iter_boxes(moov_payload) {
        let hdr = hdr.ok()?;
        if hdr.box_type != TRAK {
            continue;
        }
        let trak_payload = &moov_payload[hdr.payload_start..hdr.end()];
        let (mdia, _) = find_box(trak_payload, &MDIA).ok()??;
        let (minf, _) = find_box(mdia, &MINF).ok()??;
        let (stbl, _) = find_box(minf, &STBL).ok()??;
        return Some(stbl);
    }
    None
}

/// Walk the first track's `trak/edts/elst` and decode its entries.
/// Returns an empty `Vec` when `edts` or `elst` is absent (the §8.6.5
/// implicit identity case), the FullBox body is truncated, or
/// `entry_count` is zero. Both v0 (32-bit) and v1 (64-bit) shapes are
/// supported per ISO/IEC 14496-12 §8.6.6.2.
fn find_first_track_edit_list(moov_payload: &[u8]) -> Vec<EditListEntry> {
    for hdr in iter_boxes(moov_payload) {
        let Ok(hdr) = hdr else {
            return Vec::new();
        };
        if hdr.box_type != TRAK {
            continue;
        }
        let trak_payload = &moov_payload[hdr.payload_start..hdr.end()];
        let Ok(Some((edts_payload, _))) = find_box(trak_payload, &EDTS) else {
            return Vec::new();
        };
        let Ok(Some((elst_payload, _))) = find_box(edts_payload, &ELST) else {
            return Vec::new();
        };
        return parse_edit_list_box(elst_payload).unwrap_or_default();
    }
    Vec::new()
}

/// Decode an `EditListBox` payload (ISO/IEC 14496-12 §8.6.6.2). The
/// FullBox body layout is `entry_count(32)` followed by `entry_count`
/// records sized by `version`:
///
/// * **v0:** `segment_duration(32) + media_time(s32) + media_rate(32)`
///   per entry (12 bytes).
/// * **v1:** `segment_duration(64) + media_time(s64) + media_rate(32)`
///   per entry (20 bytes).
///
/// `media_rate` packs `media_rate_integer(s16)` followed by
/// `media_rate_fraction(s16)` — they're decoded into separate fields
/// on [`EditListEntry`] so callers can inspect either half.
///
/// Returns `Err(InvalidData)` only when the FullBox header itself is
/// malformed; a truncated entry table is treated as the boundary of
/// the recognised entries (every well-formed entry up to that point
/// is returned). A `version > 1` payload is treated as having no
/// recognised entries.
fn parse_edit_list_box(elst_payload: &[u8]) -> Result<Vec<EditListEntry>> {
    let (version, _flags, body) = parse_full_box(elst_payload)?;
    if body.len() < 4 {
        return Ok(Vec::new());
    }
    let entry_count = read_u32(body, 0)? as usize;
    let mut out = Vec::with_capacity(entry_count.min(64));
    let mut cursor = 4usize;
    let entry_size = match version {
        0 => 12usize,
        1 => 20usize,
        _ => return Ok(Vec::new()),
    };
    for _ in 0..entry_count {
        if cursor + entry_size > body.len() {
            break;
        }
        let (segment_duration, media_time) = match version {
            0 => {
                let seg = read_u32(body, cursor)? as u64;
                let mt = read_u32(body, cursor + 4)? as i32 as i64;
                (seg, mt)
            }
            _ => {
                // version == 1.
                let seg = read_u64(body, cursor)?;
                let mt = read_u64(body, cursor + 8)? as i64;
                (seg, mt)
            }
        };
        let rate_at = cursor + entry_size - 4;
        let media_rate_integer = i16::from_be_bytes([body[rate_at], body[rate_at + 1]]);
        let media_rate_fraction = i16::from_be_bytes([body[rate_at + 2], body[rate_at + 3]]);
        out.push(EditListEntry {
            segment_duration,
            media_time,
            media_rate_integer,
            media_rate_fraction,
        });
        cursor += entry_size;
    }
    Ok(out)
}

/// Extract the four-CC `handler_type` from the first track's
/// `mdia/hdlr` box. ISO/IEC 14496-12 §8.4.3 FullBox layout:
/// `version(1) + flags(3) + pre_defined(4) + handler_type(4) +
/// reserved(12) + name(string)`. Returns `None` when the box is
/// missing or the body cannot fit the `handler_type` field.
fn find_first_track_handler(moov_payload: &[u8]) -> Option<BoxType> {
    for hdr in iter_boxes(moov_payload) {
        let hdr = hdr.ok()?;
        if hdr.box_type != TRAK {
            continue;
        }
        let trak_payload = &moov_payload[hdr.payload_start..hdr.end()];
        let (mdia, _) = find_box(trak_payload, &MDIA).ok()??;
        let (hdlr_payload, _) = find_box(mdia, &HDLR).ok()??;
        let (_v, _f, body) = parse_full_box(hdlr_payload).ok()?;
        // body: pre_defined(4) + handler_type(4) + reserved(12) + name
        if body.len() < 8 {
            return None;
        }
        return Some([body[4], body[5], body[6], body[7]]);
    }
    None
}

/// Decode the `stsd` FullBox in `stbl` and return the four-CC of each
/// SampleEntry in declaration order. Returns an empty `Vec` when
/// `stsd` is missing, the FullBox body is truncated, or the declared
/// `entry_count` is zero.
///
/// av1-avif v1.2.0 §3 mandates that for an AV1 Image Sequence the
/// track shall have only one AV1 Sample description entry — i.e. the
/// returned slice shall be exactly `['av01']`. This walker is
/// permissive: it surfaces every SampleEntry four-CC, regardless of
/// type, so the audit layer can distinguish "wrong count" from "wrong
/// type" failure modes.
fn sample_description_types_in_stbl(stbl: &[u8]) -> Vec<BoxType> {
    let mut out = Vec::new();
    let Ok(Some((stsd_payload, _))) = find_box(stbl, &STSD) else {
        return out;
    };
    let Ok((_v, _f, body)) = parse_full_box(stsd_payload) else {
        return out;
    };
    if body.len() < 4 {
        return out;
    }
    let entry_count = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    if entry_count == 0 {
        return out;
    }
    let entries = &body[4..];
    for hdr in iter_boxes(entries).flatten() {
        out.push(hdr.box_type);
        if out.len() as u32 >= entry_count {
            break;
        }
    }
    out
}

/// Build a flat list of samples from an `stbl` payload by expanding
/// stts/stsc/stsz/stco+co64 the same way §8.6.1 prescribes.
pub fn sample_table(stbl: &[u8]) -> Result<Vec<Sample>> {
    let mut stts_payload: Option<&[u8]> = None;
    let mut stsc_payload: Option<&[u8]> = None;
    let mut stsz_payload: Option<&[u8]> = None;
    let mut stco_payload: Option<&[u8]> = None;
    let mut co64_payload: Option<&[u8]> = None;
    let mut stss_payload: Option<&[u8]> = None;
    for hdr in iter_boxes(stbl) {
        let hdr = hdr?;
        let p = &stbl[hdr.payload_start..hdr.end()];
        match &hdr.box_type {
            x if x == &STTS => stts_payload = Some(p),
            x if x == &STSC => stsc_payload = Some(p),
            x if x == &STSZ => stsz_payload = Some(p),
            x if x == &STCO => stco_payload = Some(p),
            x if x == &CO64 => co64_payload = Some(p),
            x if x == &STSS => stss_payload = Some(p),
            _ => {}
        }
    }
    let stts_p =
        stts_payload.ok_or_else(|| Error::InvalidData("avis: stbl missing stts".to_string()))?;
    let stsc_p =
        stsc_payload.ok_or_else(|| Error::InvalidData("avis: stbl missing stsc".to_string()))?;
    let stsz_p =
        stsz_payload.ok_or_else(|| Error::InvalidData("avis: stbl missing stsz".to_string()))?;
    let (sample_size, sizes) = parse_stsz(stsz_p)?;
    let stsc_entries = parse_stsc(stsc_p)?;
    let sample_deltas = parse_stts(stts_p)?;
    let stss_set = match stss_payload {
        Some(p) => Some(parse_stss(p)?),
        None => None,
    };
    let chunk_offsets: Vec<u64> = if let Some(p) = stco_payload {
        parse_stco(p)?
    } else if let Some(p) = co64_payload {
        parse_co64(p)?
    } else {
        return Err(Error::InvalidData(
            "avis: stbl missing stco/co64".to_string(),
        ));
    };
    let chunk_count = chunk_offsets.len();
    // Expand stsc to a per-chunk samples_per_chunk slice.
    let mut per_chunk = vec![0u32; chunk_count];
    for (i, e) in stsc_entries.iter().enumerate() {
        let start = (e.first_chunk.saturating_sub(1)) as usize;
        let end = if i + 1 < stsc_entries.len() {
            (stsc_entries[i + 1].first_chunk.saturating_sub(1)) as usize
        } else {
            chunk_count
        };
        if start > end || end > chunk_count {
            return Err(Error::InvalidData(format!(
                "avis: stsc entry {i} out of range (start={start} end={end} chunks={chunk_count})"
            )));
        }
        for c in &mut per_chunk[start..end] {
            *c = e.samples_per_chunk;
        }
    }
    // Soft cap on the total number of samples we are willing to expand
    // from stsc/stsz. Adversarial files often inflate `samples_per_chunk`
    // to `0xFFFF_FFFF`, which would otherwise spin in this loop or OOM
    // the per-sample Vec for hours. AVIS streams in the wild stay well
    // below this cap (a 60fps hour-long sequence is ~216K samples).
    const MAX_TOTAL_SAMPLES: usize = 16 * 1024 * 1024;
    let total_expected: u64 = per_chunk
        .iter()
        .map(|&n| n as u64)
        .fold(0u64, u64::saturating_add);
    if total_expected > MAX_TOTAL_SAMPLES as u64 {
        return Err(Error::InvalidData(format!(
            "avis: stsc expands to {total_expected} samples, soft cap is {MAX_TOTAL_SAMPLES}"
        )));
    }
    let mut out = Vec::new();
    let mut sample_idx: u32 = 0;
    for c in 0..chunk_count {
        let mut off = chunk_offsets[c];
        for _ in 0..per_chunk[c] {
            let size = if sample_size != 0 {
                sample_size
            } else {
                let idx = sample_idx as usize;
                if idx >= sizes.len() {
                    return Err(Error::InvalidData(format!(
                        "avis: stsz has {} sizes but sample index {idx}",
                        sizes.len()
                    )));
                }
                sizes[idx]
            };
            let duration = if (sample_idx as usize) < sample_deltas.len() {
                sample_deltas[sample_idx as usize]
            } else {
                0
            };
            let is_sync = match &stss_set {
                Some(s) => s.binary_search(&(sample_idx + 1)).is_ok(),
                None => true,
            };
            out.push(Sample {
                offset: off,
                size,
                duration,
                is_sync,
            });
            off = off.saturating_add(size as u64);
            sample_idx = sample_idx.saturating_add(1);
        }
    }
    Ok(out)
}

fn parse_stts(payload: &[u8]) -> Result<Vec<u32>> {
    let (_v, _f, body) = parse_full_box(payload)?;
    if body.len() < 4 {
        return Err(Error::InvalidData("avis: stts truncated".to_string()));
    }
    let n = read_u32(body, 0)? as usize;
    let mut cursor = 4usize;
    let mut out = Vec::new();
    for _ in 0..n {
        if cursor + 8 > body.len() {
            return Err(Error::InvalidData(
                "avis: stts entries truncated".to_string(),
            ));
        }
        let count = read_u32(body, cursor)?;
        cursor += 4;
        let delta = read_u32(body, cursor)?;
        cursor += 4;
        for _ in 0..count {
            out.push(delta);
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug)]
struct StscEntry {
    first_chunk: u32,
    samples_per_chunk: u32,
    _description_idx: u32,
}

fn parse_stsc(payload: &[u8]) -> Result<Vec<StscEntry>> {
    let (_v, _f, body) = parse_full_box(payload)?;
    if body.len() < 4 {
        return Err(Error::InvalidData("avis: stsc truncated".to_string()));
    }
    let n = read_u32(body, 0)? as usize;
    let mut cursor = 4usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if cursor + 12 > body.len() {
            return Err(Error::InvalidData(
                "avis: stsc entries truncated".to_string(),
            ));
        }
        out.push(StscEntry {
            first_chunk: read_u32(body, cursor)?,
            samples_per_chunk: read_u32(body, cursor + 4)?,
            _description_idx: read_u32(body, cursor + 8)?,
        });
        cursor += 12;
    }
    Ok(out)
}

/// Returns `(sample_size, per_sample_sizes)`. When `sample_size != 0`
/// every sample shares that size and the per-sample vector is empty.
fn parse_stsz(payload: &[u8]) -> Result<(u32, Vec<u32>)> {
    let (_v, _f, body) = parse_full_box(payload)?;
    if body.len() < 8 {
        return Err(Error::InvalidData("avis: stsz truncated".to_string()));
    }
    let sample_size = read_u32(body, 0)?;
    let sample_count = read_u32(body, 4)? as usize;
    let mut sizes = Vec::new();
    if sample_size == 0 {
        let mut cursor = 8usize;
        for _ in 0..sample_count {
            if cursor + 4 > body.len() {
                return Err(Error::InvalidData("avis: stsz sizes truncated".to_string()));
            }
            sizes.push(read_u32(body, cursor)?);
            cursor += 4;
        }
    }
    Ok((sample_size, sizes))
}

fn parse_stco(payload: &[u8]) -> Result<Vec<u64>> {
    let (_v, _f, body) = parse_full_box(payload)?;
    if body.len() < 4 {
        return Err(Error::InvalidData("avis: stco truncated".to_string()));
    }
    let n = read_u32(body, 0)? as usize;
    let mut cursor = 4usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if cursor + 4 > body.len() {
            return Err(Error::InvalidData(
                "avis: stco entries truncated".to_string(),
            ));
        }
        out.push(read_u32(body, cursor)? as u64);
        cursor += 4;
    }
    Ok(out)
}

fn parse_co64(payload: &[u8]) -> Result<Vec<u64>> {
    let (_v, _f, body) = parse_full_box(payload)?;
    if body.len() < 4 {
        return Err(Error::InvalidData("avis: co64 truncated".to_string()));
    }
    let n = read_u32(body, 0)? as usize;
    let mut cursor = 4usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if cursor + 8 > body.len() {
            return Err(Error::InvalidData(
                "avis: co64 entries truncated".to_string(),
            ));
        }
        out.push(read_u64(body, cursor)?);
        cursor += 8;
    }
    Ok(out)
}

/// Parse stss and return a sorted vec of 1-based sample indices.
fn parse_stss(payload: &[u8]) -> Result<Vec<u32>> {
    let (_v, _f, body) = parse_full_box(payload)?;
    if body.len() < 4 {
        return Err(Error::InvalidData("avis: stss truncated".to_string()));
    }
    let n = read_u32(body, 0)? as usize;
    let mut cursor = 4usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if cursor + 4 > body.len() {
            return Err(Error::InvalidData(
                "avis: stss entries truncated".to_string(),
            ));
        }
        out.push(read_u32(body, cursor)?);
        cursor += 4;
    }
    // Spec says sorted ascending — enforce it so binary_search works.
    out.sort_unstable();
    Ok(out)
}

// ===========================================================================
// AV1 Image Sequence (`avis`) §3 compliance audit
// ===========================================================================
//
// av1-avif v1.2.0 §3 layers four `shall`-level constraints on top of a
// MIAF image-sequence track:
//
//   1. The track shall be a valid MIAF image sequence (audited
//      elsewhere — handler == `pict` is the local proxy here).
//   2. The track handler shall be `'pict'`.
//   3. The track shall have only one AV1 Sample description entry.
//   4. If multiple Sequence Header OBUs are present across the track
//      payload, they shall be identical.
//
// `audit_avis_sequence` walks a parsed `AvisMeta` plus the source
// file bytes once and emits a single `AvisSequenceCompliance` record
// that surfaces each `shall` independently — callers can either gate
// on the aggregated `is_compliant()` or report individual failures via
// `missing()`.

/// av1-avif v1.2.0 §3 AV1 Image Sequence compliance record.
///
/// Emitted by [`audit_avis_sequence`]. Each boolean field tracks one
/// normative `shall`; the spec-source mapping is on each field. The
/// record is a single-instance audit (one record per file): unlike
/// the per-item `'av01'` audits in [`crate::derived`], an AVIS file
/// has at most one image-sequence track.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AvisSequenceCompliance {
    /// `true` when the first track's `mdia/hdlr/handler_type` equals
    /// `'pict'`. Spec: av1-avif v1.2.0 §3 (handler `shall` be
    /// `'pict'`); ISO/IEC 14496-12 §8.4.3 (`hdlr` box layout).
    pub handler_is_pict: bool,
    /// `true` when `stbl/stsd` carries exactly one SampleEntry. Spec:
    /// av1-avif v1.2.0 §3 (sample description count `shall` be 1).
    pub single_sample_description: bool,
    /// `true` when the single SampleEntry's type is `'av01'`. Implied
    /// by §3 (the one entry shall be an AV1 sample entry). Distinct
    /// from [`Self::single_sample_description`] so the audit can
    /// report "right count, wrong type" separately from "wrong count".
    pub sample_description_is_av01: bool,
    /// `true` when every Sequence Header OBU encountered across the
    /// track's sample payloads is byte-identical to the first one.
    /// Vacuously `true` when zero or one Sequence Header OBUs are
    /// present. Spec: av1-avif v1.2.0 §3 (multiple SH OBUs `shall` be
    /// identical).
    pub sequence_headers_identical: bool,
    /// Diagnostic — actual four-CC found at `mdia/hdlr/handler_type`.
    /// `None` when no `hdlr` could be located.
    pub observed_handler: Option<BoxType>,
    /// Diagnostic — number of SampleEntries declared by `stsd`.
    pub sample_description_count: u32,
    /// Diagnostic — total Sequence Header OBUs encountered across
    /// every sample.
    pub sequence_header_obu_count: u32,
    /// Diagnostic — total samples whose byte range could not be
    /// resolved against `file` (offset/size out of range). Such
    /// samples are skipped for the SH-OBU walk and do not flip
    /// [`Self::sequence_headers_identical`].
    pub samples_out_of_range: u32,
}

impl AvisSequenceCompliance {
    /// `true` when every audited `shall` passes:
    /// handler is `'pict'`, sample description count is 1, the entry
    /// type is `'av01'`, and any Sequence Header OBUs encountered
    /// across samples are byte-identical.
    pub fn is_compliant(&self) -> bool {
        self.handler_is_pict
            && self.single_sample_description
            && self.sample_description_is_av01
            && self.sequence_headers_identical
    }

    /// Human-readable list of `shall`-level failures. Empty when
    /// [`Self::is_compliant`] returns `true`. Token shapes mirror
    /// other AVIF audits (`avis-…`).
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.handler_is_pict {
            out.push("avis-handler-not-pict");
        }
        if !self.single_sample_description {
            out.push("avis-sample-description-not-single");
        }
        if !self.sample_description_is_av01 {
            out.push("avis-sample-description-not-av01");
        }
        if !self.sequence_headers_identical {
            out.push("avis-sequence-header-obus-differ");
        }
        out
    }
}

/// Audit an [`AvisMeta`] + the source file bytes against the
/// av1-avif v1.2.0 §3 `shall`-level constraints on an AV1 Image
/// Sequence.
///
/// Walks every sample's OBU stream once. The function reads only
/// from the parsed metadata and the supplied `file` slice — no IO,
/// no decode. Sample payloads that fall outside the file bounds
/// are reported via [`AvisSequenceCompliance::samples_out_of_range`]
/// and skipped from the Sequence-Header-identity check.
///
/// OBU framing follows AV1 §5.3.1 / §5.3.2 / §4.10.5 — see the
/// implementation of [`crate::derived::audit_sequence_header_obu`]
/// for the parallel still-image walker.
pub fn audit_avis_sequence(meta: &AvisMeta, file: &[u8]) -> AvisSequenceCompliance {
    let handler_is_pict = meta.handler == Some(HANDLER_PICT);
    let single_sample_description = meta.sample_description_types.len() == 1;
    let sample_description_is_av01 = meta
        .sample_description_types
        .first()
        .map(|t| t == &AV01)
        .unwrap_or(false);

    let mut first_sh: Option<Vec<u8>> = None;
    let mut sh_total: u32 = 0;
    let mut sh_identical = true;
    let mut samples_out_of_range: u32 = 0;
    for s in &meta.samples {
        let payload = match sample_bytes(file, s) {
            Ok(p) => p,
            Err(_) => {
                samples_out_of_range = samples_out_of_range.saturating_add(1);
                continue;
            }
        };
        for sh in walk_sequence_header_obus(payload) {
            sh_total = sh_total.saturating_add(1);
            match &first_sh {
                None => first_sh = Some(sh),
                Some(canonical) => {
                    if canonical != &sh {
                        sh_identical = false;
                    }
                }
            }
        }
    }

    AvisSequenceCompliance {
        handler_is_pict,
        single_sample_description,
        sample_description_is_av01,
        sequence_headers_identical: sh_identical,
        observed_handler: meta.handler,
        sample_description_count: meta.sample_description_types.len() as u32,
        sequence_header_obu_count: sh_total,
        samples_out_of_range,
    }
}

// ===========================================================================
// AVIF Profile compliance for AV1 Image Sequences — av1-avif v1.2.0 §8.2 / §8.3
// ===========================================================================
//
// av1-avif v1.2.0 §8.2 (`MA1B` Baseline) and §8.3 (`MA1A` Advanced) bound the
// AV1 `seq_profile` and `seq_level_idx_0` of every coded image in the file.
// For an AV1 Image Sequence track those values live on the track's
// `AV1CodecConfigurationRecord` carried in `stbl/stsd/av01/av1C` — surfaced
// here via [`AvisMeta::av1_codec_config`]. The per-still-image audit at
// [`crate::derived::audit_avif_profile_compliance`] covers `iprp.ipco`;
// this audit covers the parallel sample-table carrier.

/// av1-avif v1.2.0 §8.2 / §8.3 AV1-Image-Sequence profile compliance record.
///
/// Emitted by [`audit_avis_profile_compliance`], one record per declared
/// AVIF profile brand (so a file claiming both `MA1B` and `MA1A` produces
/// two records, Baseline before Advanced). The audit operates entirely on
/// the track's `av1C` flag byte (byte 1, which packs `seq_profile (3) |
/// seq_level_idx_0 (5)` per av1-isobmff §2.3); no AV1 OBU decode is
/// performed.
///
/// The single-track shape of AVIS (each file carries one AV1 Image Sequence
/// track) means at most one `av1C` is inspected; the per-`(item, profile)`
/// fan-out from the still-image audit collapses to per-`(track, profile)`
/// here. AVIS files declaring neither `MA1B` nor `MA1A` skip the audit
/// entirely — [`audit_avis_profile_compliance`] returns an empty vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvisProfileCompliance {
    /// Which AVIF profile this record is checking the sequence track
    /// against — same enum as the still-image audit
    /// ([`crate::derived::AvifProfile`]).
    pub profile: crate::derived::AvifProfile,
    /// `seq_profile` decoded from the track `av1C[1]` high 3 bits, or
    /// `None` when the av1C is absent or truncated.
    pub seq_profile: Option<u8>,
    /// `seq_level_idx_0` decoded from the track `av1C[1]` low 5 bits, or
    /// `None` when the av1C is absent or truncated.
    pub seq_level_idx_0: Option<u8>,
    /// `true` when [`AvisMeta::av1_codec_config`] is `None` — no `av1C`
    /// could be located in the track's `stsd → av01` chain. (Distinct
    /// from a present-but-truncated `av1C`, which surfaces as both fields
    /// `None` without setting this flag.)
    pub missing_av1c: bool,
}

impl AvisProfileCompliance {
    /// True when the track's `(seq_profile, seq_level_idx_0)` pair
    /// satisfies the declared AVIF profile's `shall`-level constraints.
    ///
    /// Baseline (`MA1B`, §8.2): `seq_profile == 0` (AV1 Main) AND
    /// `seq_level_idx_0 <= 13` (level ≤ 5.1).
    ///
    /// Advanced (`MA1A`, §8.3): `seq_profile <= 1` (AV1 Main or High)
    /// AND `seq_level_idx_0 <= 16` (level ≤ 6.0). Per AV1 Annex A.2 a
    /// High-Profile decoder also accepts Main-Profile streams, so a
    /// `seq_profile == 0` track passes the Advanced check too.
    ///
    /// Returns `false` when `av1C` is missing or truncated.
    pub fn is_compliant(&self) -> bool {
        match (self.seq_profile, self.seq_level_idx_0) {
            (Some(p), Some(l)) => match self.profile {
                crate::derived::AvifProfile::Baseline => p == 0 && l <= 13,
                crate::derived::AvifProfile::Advanced => p <= 1 && l <= 16,
            },
            _ => false,
        }
    }

    /// Human-readable list of failed `shall`s. Empty when
    /// [`Self::is_compliant`] returns `true`. Tokens mirror the
    /// still-image audit but with an `avis-` prefix to disambiguate
    /// (`avis-track-missing-av1C`, `avis-track-av1C-truncated`,
    /// `avis-seq-profile-out-of-range`, `avis-seq-level-idx-out-of-range`).
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.missing_av1c {
            out.push("avis-track-missing-av1C");
            return out;
        }
        if self.seq_profile.is_none() || self.seq_level_idx_0.is_none() {
            out.push("avis-track-av1C-truncated");
            return out;
        }
        let p = self.seq_profile.unwrap();
        let l = self.seq_level_idx_0.unwrap();
        let (max_p, max_l) = match self.profile {
            crate::derived::AvifProfile::Baseline => (0u8, 13u8),
            crate::derived::AvifProfile::Advanced => (1u8, 16u8),
        };
        if p > max_p {
            out.push("avis-seq-profile-out-of-range");
        }
        if l > max_l {
            out.push("avis-seq-level-idx-out-of-range");
        }
        out
    }
}

/// Audit an AV1 Image Sequence track against the av1-avif v1.2.0 §8.2
/// (`MA1B`) and §8.3 (`MA1A`) profile `shall`-level constraints, gated
/// on the file's declared brands.
///
/// One [`AvisProfileCompliance`] record is emitted per declared profile
/// brand (so a file declaring both `MA1B` and `MA1A` produces two
/// records, Baseline before Advanced). The audit reads only the
/// track's `av1C` byte 1 (surfaced via [`AvisMeta::av1_codec_config`]);
/// it does not decode AV1 OBUs and does not walk per-sample payloads.
///
/// The returned vector is empty when the `brands` argument declares
/// neither `MA1B` nor `MA1A` — a file that doesn't claim a profile has
/// nothing to fail. Symmetric with the per-item audit at
/// [`crate::derived::audit_avif_profile_compliance`].
///
/// Spec sources:
/// * av1-avif v1.2.0 §8.2 — `MA1B` Baseline Profile constraints.
/// * av1-avif v1.2.0 §8.3 — `MA1A` Advanced Profile constraints.
/// * AV1 §A.2 — Profiles (Main / High / Professional).
/// * AV1 §A.3 — Levels (seq_level_idx_0 ↔ X.Y mapping; 13 = 5.1,
///   16 = 6.0, 31 = unconstrained).
/// * av1-isobmff §2.3 — `av1C` byte layout.
pub fn audit_avis_profile_compliance(
    meta: &AvisMeta,
    brands: &crate::parser::BrandClass,
) -> Vec<AvisProfileCompliance> {
    let mut out = Vec::new();
    if !brands.is_baseline_profile && !brands.is_advanced_profile {
        return out;
    }
    let (seq_profile, seq_level_idx_0, missing_av1c) = match meta.av1_codec_config.as_deref() {
        Some(bytes) => (
            crate::derived::decode_av1c_seq_profile(bytes),
            crate::derived::decode_av1c_seq_level_idx_0(bytes),
            false,
        ),
        None => (None, None, true),
    };
    if brands.is_baseline_profile {
        out.push(AvisProfileCompliance {
            profile: crate::derived::AvifProfile::Baseline,
            seq_profile,
            seq_level_idx_0,
            missing_av1c,
        });
    }
    if brands.is_advanced_profile {
        out.push(AvisProfileCompliance {
            profile: crate::derived::AvifProfile::Advanced,
            seq_profile,
            seq_level_idx_0,
            missing_av1c,
        });
    }
    out
}

// ===========================================================================
// Edit List (`edts/elst`) compliance — ISO/IEC 14496-12 §8.6.6.3
// ===========================================================================
//
// `elst` maps the AVIS track's presentation timeline onto its media
// timeline. The Edit Box is optional (§8.6.5); in its absence the
// mapping is implicit identity. When present, §8.6.6.3 layers two
// per-entry `shall`-level constraints:
//
//   1. "The last edit in a track shall never be an empty edit"
//      (i.e. the trailing entry's `media_time` shall not be `-1`).
//   2. `media_rate` shall be either `0` (dwell) or `1` (normal-rate);
//      no other `media_rate_integer` value is permitted.
//
// `audit_edit_list` walks the parsed entries on [`AvisMeta`] once and
// emits a single [`EditListCompliance`] record — mirroring the shape
// of `audit_avis_sequence` and `audit_avis_profile_compliance`. A
// file that ships no `edts` (or whose `edts` carries no entries)
// trivially passes the audit.

/// ISO/IEC 14496-12 §8.6.6.3 edit-list compliance record.
///
/// Emitted by [`audit_edit_list`]. Each boolean field tracks one
/// normative `shall`; tally fields surface diagnostic counts for
/// callers wanting a single-record summary. An AVIS file without an
/// `edts/elst` (the implicit-identity case) produces a record where
/// every `shall` passes vacuously.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EditListCompliance {
    /// `true` when no entry's `media_rate_integer` is outside the
    /// `{0, 1}` set sanctioned by §8.6.6.3 ("Otherwise this field
    /// shall contain the value 1"). Vacuously `true` when the
    /// edit list is empty.
    pub media_rate_in_range: bool,
    /// `true` when the trailing entry is not an empty edit. Spec
    /// §8.6.6.3: "The last edit in a track shall never be an empty
    /// edit." Vacuously `true` when the edit list is empty.
    pub last_entry_not_empty: bool,
    /// Diagnostic — total number of `elst` entries decoded. `0` when
    /// the track has no `edts` or the `elst` body parsed as zero
    /// entries.
    pub entry_count: u32,
    /// Diagnostic — number of entries whose `media_time == -1` (empty
    /// edits). At most one is normally expected (the leading offset),
    /// but the count is surfaced so callers can spot encoders that
    /// pack multiple empties.
    pub empty_edit_count: u32,
    /// Diagnostic — number of entries whose `media_rate_integer == 0`
    /// (dwells). §8.6.6.3 permits dwell entries explicitly.
    pub dwell_entry_count: u32,
    /// Diagnostic — number of entries flagged for
    /// [`Self::media_rate_in_range`]. `0` when every entry passes.
    pub out_of_range_rate_count: u32,
}

impl EditListCompliance {
    /// `true` when every audited §8.6.6.3 `shall` passes (trivially
    /// `true` for an empty edit list).
    pub fn is_compliant(&self) -> bool {
        self.media_rate_in_range && self.last_entry_not_empty
    }

    /// Human-readable list of `shall`-level failures. Empty when
    /// [`Self::is_compliant`] returns `true`. Token shapes mirror the
    /// other AVIS audits (`avis-…` prefix).
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.media_rate_in_range {
            out.push("avis-edit-list-media-rate-out-of-range");
        }
        if !self.last_entry_not_empty {
            out.push("avis-edit-list-last-entry-empty");
        }
        out
    }
}

/// Audit an [`AvisMeta`]'s `edit_list` against the ISO/IEC 14496-12
/// §8.6.6.3 `shall`-level constraints.
///
/// Returns a single record — an AVIS file carries at most one image
/// sequence track and therefore at most one `edts/elst`. The audit
/// reads only the parsed entries: no file IO, no decode, no
/// `mvhd`/`tkhd` cross-checks (those have their own §8 audits).
///
/// Empty input (no `edts`, no `elst`, or `entry_count == 0`)
/// trivially satisfies both `shall`s — the record's
/// [`EditListCompliance::is_compliant`] returns `true` and the
/// diagnostic counters are zero.
pub fn audit_edit_list(meta: &AvisMeta) -> EditListCompliance {
    let entry_count = meta.edit_list.len() as u32;
    let empty_edit_count = meta.edit_list.iter().filter(|e| e.is_empty_edit()).count() as u32;
    let dwell_entry_count = meta.edit_list.iter().filter(|e| e.is_dwell()).count() as u32;
    let out_of_range_rate_count = meta
        .edit_list
        .iter()
        .filter(|e| e.media_rate_integer != 0 && e.media_rate_integer != 1)
        .count() as u32;
    let media_rate_in_range = out_of_range_rate_count == 0;
    // §8.6.6.3 "The last edit in a track shall never be an empty
    // edit." Vacuously satisfied when the edit list is empty (a
    // file without an `edts` has no "last edit" to constrain).
    let last_entry_not_empty = meta
        .edit_list
        .last()
        .map(|e| !e.is_empty_edit())
        .unwrap_or(true);

    EditListCompliance {
        media_rate_in_range,
        last_entry_not_empty,
        entry_count,
        empty_edit_count,
        dwell_entry_count,
        out_of_range_rate_count,
    }
}

/// Walk one AV1 sample payload and return the raw byte slices of
/// every OBU whose `obu_type` equals `OBU_SEQUENCE_HEADER` (value
/// `1`, per AV1 §6.2.1). The returned slice for each SH OBU starts
/// at the OBU header byte and runs through the end of the OBU
/// payload — i.e. byte-equality on these slices is what
/// av1-avif §3 calls "identical".
///
/// Parsing follows AV1 §5.3.1 (general OBU framing), §5.3.2 (OBU
/// header byte layout: `obu_forbidden_bit(1) | obu_type(4) |
/// obu_extension_flag(1) | obu_has_size_field(1) |
/// obu_reserved_1bit(1)`), §5.3.3 (extension byte when
/// `obu_extension_flag == 1`), and §4.10.5 (`leb128()` for
/// `obu_size`). A malformed framing (truncated leb128, payload
/// running past EOF, or `obu_has_size_field == 0`) stops the walk
/// for that sample — every SH OBU successfully framed up to that
/// point is still returned.
fn walk_sequence_header_obus(payload: &[u8]) -> Vec<Vec<u8>> {
    const AV1_OBU_TYPE_SEQUENCE_HEADER: u8 = 1;
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < payload.len() {
        let obu_start = cursor;
        let header = payload[cursor];
        cursor += 1;
        let obu_type = (header >> 3) & 0x0f;
        let obu_extension_flag = (header >> 2) & 0x01;
        let obu_has_size_field = (header >> 1) & 0x01;
        if obu_extension_flag == 1 {
            if cursor >= payload.len() {
                return out;
            }
            cursor += 1;
        }
        if obu_has_size_field == 0 {
            // Without the size field, the next OBU's start is
            // undefined inside an item-framed container per AV1
            // §5.3.1 — bail. The header byte we already read is
            // surfaced only when this OBU is itself a SH OBU.
            if obu_type == AV1_OBU_TYPE_SEQUENCE_HEADER {
                out.push(payload[obu_start..].to_vec());
            }
            return out;
        }
        // leb128 obu_size, per AV1 §4.10.5.
        let mut size: u64 = 0;
        let mut leb_len = 0usize;
        let mut bad = false;
        for i in 0..8 {
            if cursor + i >= payload.len() {
                bad = true;
                break;
            }
            let b = payload[cursor + i];
            size |= u64::from(b & 0x7f) << (i * 7);
            if b & 0x80 == 0 {
                leb_len = i + 1;
                break;
            }
            if i == 7 {
                bad = true;
                break;
            }
        }
        if bad || size > u64::from(u32::MAX) {
            return out;
        }
        cursor += leb_len;
        let payload_end = match cursor.checked_add(size as usize) {
            Some(e) if e <= payload.len() => e,
            _ => {
                // Truncated OBU body — surface the SH header if this
                // was an SH OBU (matches the still-image audit's
                // "count it before bailing" behaviour) then stop.
                if obu_type == AV1_OBU_TYPE_SEQUENCE_HEADER {
                    out.push(payload[obu_start..].to_vec());
                }
                return out;
            }
        };
        if obu_type == AV1_OBU_TYPE_SEQUENCE_HEADER {
            out.push(payload[obu_start..payload_end].to_vec());
        }
        cursor = payload_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal stbl payload containing stts/stsc/stsz/stco/stss
    /// for a single-chunk, 3-sample layout with sizes [10,20,30] at
    /// chunk offset 100.
    fn minimal_stbl() -> Vec<u8> {
        fn full_box(v: u8, flags: u32, body: &[u8]) -> Vec<u8> {
            let mut out = vec![v, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
            out.extend_from_slice(body);
            out
        }
        fn wrap(btype: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let size = (8 + payload.len()) as u32;
            let mut out = size.to_be_bytes().to_vec();
            out.extend_from_slice(btype);
            out.extend_from_slice(payload);
            out
        }
        let stts_body = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&3u32.to_be_bytes()); // count = 3
            b.extend_from_slice(&100u32.to_be_bytes()); // delta = 100
            b
        };
        let stsc_body = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
            b.extend_from_slice(&3u32.to_be_bytes()); // samples_per_chunk
            b.extend_from_slice(&1u32.to_be_bytes()); // desc_idx
            b
        };
        let stsz_body = {
            let mut b = 0u32.to_be_bytes().to_vec(); // sample_size=0
            b.extend_from_slice(&3u32.to_be_bytes()); // sample_count=3
            b.extend_from_slice(&10u32.to_be_bytes());
            b.extend_from_slice(&20u32.to_be_bytes());
            b.extend_from_slice(&30u32.to_be_bytes());
            b
        };
        let stco_body = {
            let mut b = 1u32.to_be_bytes().to_vec(); // one chunk
            b.extend_from_slice(&100u32.to_be_bytes());
            b
        };
        let stss_body = {
            let mut b = 1u32.to_be_bytes().to_vec(); // one sync
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
        let mut out = Vec::new();
        out.extend_from_slice(&wrap(b"stts", &full_box(0, 0, &stts_body)));
        out.extend_from_slice(&wrap(b"stsc", &full_box(0, 0, &stsc_body)));
        out.extend_from_slice(&wrap(b"stsz", &full_box(0, 0, &stsz_body)));
        out.extend_from_slice(&wrap(b"stco", &full_box(0, 0, &stco_body)));
        out.extend_from_slice(&wrap(b"stss", &full_box(0, 0, &stss_body)));
        out
    }

    /// `sample_table` refuses an stsc whose declared `samples_per_chunk`
    /// would expand to more than the soft cap (`MAX_TOTAL_SAMPLES`).
    /// Without this guard the per-chunk loop spins on an adversarial
    /// `0xFFFF_FFFF` until the process OOMs.
    #[test]
    fn sample_table_rejects_oversized_stsc_expansion() {
        fn wrap(t: &[u8; 4], p: &[u8]) -> Vec<u8> {
            let size = (8 + p.len()) as u32;
            let mut out = size.to_be_bytes().to_vec();
            out.extend_from_slice(t);
            out.extend_from_slice(p);
            out
        }
        fn full_box(v: u8, flags: u32, body: &[u8]) -> Vec<u8> {
            let mut out = vec![v, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
            out.extend_from_slice(body);
            out
        }
        // stts: 1 entry, count=1, delta=1.
        let stts_body = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
        // stsc: 1 entry, first_chunk=1, samples_per_chunk=0xFFFF_FFFF.
        let stsc_body = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&u32::MAX.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
        // stsz: sample_size=1 sample_count=1 (so size lookup never runs out).
        let stsz_body = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
        // stco: 1 chunk @ offset 100.
        let stco_body = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&100u32.to_be_bytes());
            b
        };
        let mut stbl = Vec::new();
        stbl.extend_from_slice(&wrap(b"stts", &full_box(0, 0, &stts_body)));
        stbl.extend_from_slice(&wrap(b"stsc", &full_box(0, 0, &stsc_body)));
        stbl.extend_from_slice(&wrap(b"stsz", &full_box(0, 0, &stsz_body)));
        stbl.extend_from_slice(&wrap(b"stco", &full_box(0, 0, &stco_body)));
        let err = sample_table(&stbl).unwrap_err();
        match err {
            Error::InvalidData(s) => assert!(
                s.contains("soft cap") || s.contains("samples"),
                "expected DoS cap message, got: {s}"
            ),
            _ => panic!("expected InvalidData"),
        }
    }

    #[test]
    fn sample_table_three_samples() {
        let stbl = minimal_stbl();
        let samples = sample_table(&stbl).unwrap();
        assert_eq!(samples.len(), 3);
        assert_eq!(
            samples[0],
            Sample {
                offset: 100,
                size: 10,
                duration: 100,
                is_sync: true,
            }
        );
        assert_eq!(
            samples[1],
            Sample {
                offset: 110,
                size: 20,
                duration: 100,
                is_sync: false,
            }
        );
        assert_eq!(
            samples[2],
            Sample {
                offset: 130,
                size: 30,
                duration: 100,
                is_sync: false,
            }
        );
    }

    #[test]
    fn sample_table_missing_stts_errors() {
        // stbl with only stsc/stsz/stco — no stts.
        let mut stbl = Vec::new();
        let wrap = |t: &[u8; 4], p: &[u8]| {
            let size = (8 + p.len()) as u32;
            let mut out = size.to_be_bytes().to_vec();
            out.extend_from_slice(t);
            out.extend_from_slice(p);
            out
        };
        stbl.extend_from_slice(&wrap(b"stsc", &[0u8; 4]));
        stbl.extend_from_slice(&wrap(b"stsz", &[0u8; 8]));
        stbl.extend_from_slice(&wrap(b"stco", &[0u8; 4]));
        let err = sample_table(&stbl).unwrap_err();
        match err {
            Error::InvalidData(_) => {}
            _ => panic!("expected InvalidData"),
        }
    }

    #[test]
    fn sample_table_absent_stss_marks_all_sync() {
        // Take the minimal stbl but strip stss.
        let full = minimal_stbl();
        // stss is the last box; recompute total length minus the stss bytes.
        // Find stss by searching for the "stss" type tag.
        let idx = full
            .windows(4)
            .position(|w| w == b"stss")
            .expect("stss present");
        let stss_size_start = idx - 4;
        let stss_size = u32::from_be_bytes([
            full[stss_size_start],
            full[stss_size_start + 1],
            full[stss_size_start + 2],
            full[stss_size_start + 3],
        ]) as usize;
        let stss_end = stss_size_start + stss_size;
        let stbl_no_stss: Vec<u8> = full
            .iter()
            .take(stss_size_start)
            .chain(full.iter().skip(stss_end))
            .copied()
            .collect();
        let samples = sample_table(&stbl_no_stss).unwrap();
        assert!(samples.iter().all(|s| s.is_sync));
    }

    /// Synthesize a tiny stbl-with-stsd-with-av01-with-av1C box chain
    /// and confirm `find_av1c_in_stbl` extracts the av1C body. This
    /// guards the AVIS sequence decode path against silent regressions
    /// in the stsd → av01 → av1C walk (av1-avif §2.2.1).
    #[test]
    fn stsd_av01_av1c_extraction_round_trip() {
        // Build a minimal av1C body — 4 bytes is enough that
        // `find_av1c_in_stbl` returns it verbatim; full
        // AV1CodecConfigurationRecord parsing is exercised by
        // oxideav-av1's own tests.
        let av1c_body: &[u8] = &[0x81, 0x04, 0x0c, 0x00];

        // av1C box: size(4) + type(4) + body
        let mut av1c_box = Vec::new();
        av1c_box.extend_from_slice(&((8 + av1c_body.len()) as u32).to_be_bytes());
        av1c_box.extend_from_slice(b"av1C");
        av1c_box.extend_from_slice(av1c_body);

        // VisualSampleEntry header — 78 bytes of mostly-zero plus
        // data_reference_index = 1 (offset 6..8).
        let mut visual_header = vec![0u8; 78];
        visual_header[6] = 0;
        visual_header[7] = 1;

        // av01 SampleEntry box: size(4) + type(4) + visual_header(78) +
        // av1C box.
        let av01_payload_len = visual_header.len() + av1c_box.len();
        let mut av01_box = Vec::new();
        av01_box.extend_from_slice(&((8 + av01_payload_len) as u32).to_be_bytes());
        av01_box.extend_from_slice(b"av01");
        av01_box.extend_from_slice(&visual_header);
        av01_box.extend_from_slice(&av1c_box);

        // stsd FullBox: version(1) + flags(3) + entry_count(4) + N
        // SampleEntries. Box header: size(4) + type(4) + body.
        let mut stsd_body = Vec::new();
        stsd_body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
        stsd_body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        stsd_body.extend_from_slice(&av01_box);

        let mut stsd_box = Vec::new();
        stsd_box.extend_from_slice(&((8 + stsd_body.len()) as u32).to_be_bytes());
        stsd_box.extend_from_slice(b"stsd");
        stsd_box.extend_from_slice(&stsd_body);

        // Pretend stbl == stsd_box for this targeted unit test.
        let extracted = find_av1c_in_stbl(&stsd_box).expect("av1C must be extracted");
        assert_eq!(
            extracted, av1c_body,
            "extracted av1C body must match the synthesized payload byte-for-byte"
        );
    }

    /// `find_av1c_in_stbl` returns `None` when stsd has zero
    /// SampleEntries — the AVIS decoder must surface an explicit error
    /// instead of seeding the AV1 decoder with an empty extradata
    /// buffer.
    #[test]
    fn stsd_missing_av01_returns_none() {
        let mut stsd_body = Vec::new();
        stsd_body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
        stsd_body.extend_from_slice(&0u32.to_be_bytes()); // entry_count = 0
        let mut stsd_box = Vec::new();
        stsd_box.extend_from_slice(&((8 + stsd_body.len()) as u32).to_be_bytes());
        stsd_box.extend_from_slice(b"stsd");
        stsd_box.extend_from_slice(&stsd_body);

        assert!(
            find_av1c_in_stbl(&stsd_box).is_none(),
            "empty entry_count must produce no av1C"
        );
    }

    /// `find_av1c_in_stbl` returns `None` when the av01 SampleEntry
    /// payload is shorter than the 78-byte VisualSampleEntry fixed
    /// header — a malformed file must not panic on the
    /// VISUAL_HEADER_LEN bounds slice.
    #[test]
    fn stsd_truncated_av01_payload_returns_none() {
        // av01 box body is only 32 bytes — far less than 78.
        let mut av01_box = Vec::new();
        av01_box.extend_from_slice(&((8 + 32) as u32).to_be_bytes());
        av01_box.extend_from_slice(b"av01");
        av01_box.extend_from_slice(&[0u8; 32]);

        let mut stsd_body = Vec::new();
        stsd_body.extend_from_slice(&[0, 0, 0, 0]);
        stsd_body.extend_from_slice(&1u32.to_be_bytes());
        stsd_body.extend_from_slice(&av01_box);

        let mut stsd_box = Vec::new();
        stsd_box.extend_from_slice(&((8 + stsd_body.len()) as u32).to_be_bytes());
        stsd_box.extend_from_slice(b"stsd");
        stsd_box.extend_from_slice(&stsd_body);

        assert!(
            find_av1c_in_stbl(&stsd_box).is_none(),
            "truncated av01 payload must not panic and must not yield an av1C"
        );
    }

    /// `parse_avis` on the Netflix `alpha_video.avif` fixture surfaces
    /// the track's av1C so the AVIS decode pipeline can seed the AV1
    /// decoder with the sequence header. Empirical: the fixture's
    /// av1C is 4 bytes (marker + profile + flags), all defined by
    /// av1-avif §2.2.1.
    #[test]
    fn alpha_video_avis_exposes_av1c() {
        let bytes = include_bytes!("../tests/fixtures/alpha_video.avif");
        let meta = parse_avis(bytes).expect("parse_avis alpha_video");
        let av1c = meta
            .av1_codec_config
            .expect("alpha_video.avif must surface an av1C from stsd");
        assert!(
            av1c.len() >= 4,
            "av1C must carry at least the 4-byte AV1CodecConfigurationRecord prefix, got {} bytes",
            av1c.len()
        );
        // Top bit of byte 0 is `marker` and must be 1 (AV1-AVIF §2.2.1).
        assert_eq!(
            av1c[0] & 0x80,
            0x80,
            "av1C[0] marker bit must be set, got {:#04x}",
            av1c[0]
        );
    }

    // -----------------------------------------------------------------
    // av1-avif v1.2.0 §3 AvisSequenceCompliance audit
    // -----------------------------------------------------------------

    /// Build a one-byte-payload OBU whose header carries the given
    /// `obu_type` and `obu_has_size_field == 1`. Used by the audit
    /// unit tests to synthesize compliant + non-compliant SH streams
    /// without depending on a full AV1 fixture.
    fn obu_with_size(obu_type: u8, payload_byte: u8) -> Vec<u8> {
        // header byte: forbidden(0)|type(4)|ext(0)|has_size(1)|reserved(0)
        let header = (obu_type & 0x0f) << 3 | 0b0000_0010;
        // leb128(1) = 0x01
        vec![header, 0x01, payload_byte]
    }

    #[test]
    fn walk_sequence_header_obus_pulls_out_sh_obus_only() {
        // One non-SH OBU (type 6 = OBU_FRAME) followed by one SH OBU
        // (type 1) followed by one OBU_TEMPORAL_DELIMITER (type 2).
        let mut buf = Vec::new();
        buf.extend_from_slice(&obu_with_size(6, 0xaa));
        buf.extend_from_slice(&obu_with_size(1, 0xbb));
        buf.extend_from_slice(&obu_with_size(2, 0xcc));
        let shs = walk_sequence_header_obus(&buf);
        assert_eq!(shs.len(), 1, "exactly one SH OBU expected");
        // SH OBU header byte for has_size=1: (1<<3)|2 == 0x0a.
        assert_eq!(shs[0][0], 0x0a);
        assert_eq!(shs[0][2], 0xbb);
    }

    #[test]
    fn walk_sequence_header_obus_empty_input_returns_empty_vec() {
        assert!(walk_sequence_header_obus(&[]).is_empty());
    }

    #[test]
    fn walk_sequence_header_obus_truncated_size_stops_walk() {
        // SH header byte with has_size=1 but missing the leb128 size
        // byte entirely.
        let buf = vec![0x0a];
        let shs = walk_sequence_header_obus(&buf);
        // Truncated leb means the SH framing failed — we get nothing.
        assert!(shs.is_empty(), "truncated leb must skip the SH OBU");
    }

    #[test]
    fn walk_sequence_header_obus_truncated_body_still_surfaces_sh_header() {
        // SH header byte (has_size=1) + leb128(3) but only 1 byte of
        // body — body extends past EOF. The audit's truncated-body
        // branch still includes the SH header in the output so an
        // identical-SH check can spot mismatched SH headers even
        // when the encoder mis-sized one.
        let buf = vec![0x0a, 0x03, 0xff];
        let shs = walk_sequence_header_obus(&buf);
        assert_eq!(shs.len(), 1);
        assert_eq!(shs[0][0], 0x0a);
    }

    #[test]
    fn walk_sequence_header_obus_has_size_zero_stops_walk_after_sh() {
        // SH header byte with has_size=0 — walker can't continue but
        // still surfaces this OBU's bytes (header through EOF) since
        // its type is SH.
        let buf = vec![0x08, 0xff, 0xee]; // (1<<3)|0 = 0x08
        let shs = walk_sequence_header_obus(&buf);
        assert_eq!(shs.len(), 1);
        assert_eq!(shs[0], vec![0x08, 0xff, 0xee]);
    }

    /// `audit_avis_sequence` against a synthetic `AvisMeta` that
    /// satisfies every §3 `shall` reports `is_compliant() == true`
    /// with no `missing()` tokens.
    #[test]
    fn audit_avis_sequence_all_shalls_satisfied() {
        let sh_obu = obu_with_size(1, 0xab);
        let mut file = vec![0u8; 100];
        file.splice(50..50, sh_obu.iter().copied());
        // Re-truncate file to a known length.
        let _ = file;
        let mut file = vec![0u8; 50];
        file.extend_from_slice(&sh_obu);
        file.extend_from_slice(&sh_obu); // second sample with identical SH
        let sh_len = sh_obu.len();
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: Some((16, 16)),
            samples: vec![
                Sample {
                    offset: 50,
                    size: sh_len as u32,
                    duration: 33,
                    is_sync: true,
                },
                Sample {
                    offset: (50 + sh_len) as u64,
                    size: sh_len as u32,
                    duration: 33,
                    is_sync: false,
                },
            ],
            av1_codec_config: Some(vec![0x81, 0x04, 0x0c, 0x00]),
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &file);
        assert!(
            audit.is_compliant(),
            "compliance audit must pass: {audit:?}"
        );
        assert!(audit.missing().is_empty());
        assert_eq!(audit.sequence_header_obu_count, 2);
        assert!(audit.sequence_headers_identical);
        assert_eq!(audit.samples_out_of_range, 0);
    }

    #[test]
    fn audit_avis_sequence_handler_not_pict_flagged() {
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: Some(*b"vide"),
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &[]);
        assert!(!audit.is_compliant());
        assert_eq!(audit.observed_handler, Some(*b"vide"));
        assert!(audit.missing().contains(&"avis-handler-not-pict"));
        // The other two shalls still pass — the missing() list should
        // contain only the handler token.
        assert_eq!(audit.missing(), vec!["avis-handler-not-pict"]);
    }

    #[test]
    fn audit_avis_sequence_handler_missing_flagged_as_not_pict() {
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: None,
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &[]);
        assert!(!audit.handler_is_pict);
        assert_eq!(audit.observed_handler, None);
        assert!(audit.missing().contains(&"avis-handler-not-pict"));
    }

    #[test]
    fn audit_avis_sequence_multiple_sample_descriptions_flagged() {
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![AV01, AV01],
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &[]);
        assert!(!audit.is_compliant());
        assert_eq!(audit.sample_description_count, 2);
        assert!(audit
            .missing()
            .contains(&"avis-sample-description-not-single"));
        // Type-of-first still passes since first is av01.
        assert!(audit.sample_description_is_av01);
    }

    #[test]
    fn audit_avis_sequence_zero_sample_descriptions_flagged() {
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: Vec::new(),
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &[]);
        assert!(!audit.single_sample_description);
        assert!(!audit.sample_description_is_av01);
        assert!(audit
            .missing()
            .contains(&"avis-sample-description-not-single"));
        assert!(audit
            .missing()
            .contains(&"avis-sample-description-not-av01"));
    }

    #[test]
    fn audit_avis_sequence_non_av01_sample_description_flagged() {
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![*b"hvc1"],
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &[]);
        assert!(audit.single_sample_description);
        assert!(!audit.sample_description_is_av01);
        assert_eq!(
            audit.missing(),
            vec!["avis-sample-description-not-av01"],
            "right count, wrong type must surface only the av01 token"
        );
    }

    #[test]
    fn audit_avis_sequence_diverging_sequence_headers_flagged() {
        // Two samples each with a SH OBU; the second SH has a different
        // payload byte from the first — the audit must flag them as
        // diverging per av1-avif §3.
        let sh_a = obu_with_size(1, 0xaa);
        let sh_b = obu_with_size(1, 0xbb);
        let mut file = Vec::new();
        file.extend_from_slice(&sh_a);
        file.extend_from_slice(&sh_b);
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: vec![
                Sample {
                    offset: 0,
                    size: sh_a.len() as u32,
                    duration: 1,
                    is_sync: true,
                },
                Sample {
                    offset: sh_a.len() as u64,
                    size: sh_b.len() as u32,
                    duration: 1,
                    is_sync: false,
                },
            ],
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &file);
        assert_eq!(audit.sequence_header_obu_count, 2);
        assert!(!audit.sequence_headers_identical);
        assert!(audit
            .missing()
            .contains(&"avis-sequence-header-obus-differ"));
    }

    #[test]
    fn audit_avis_sequence_single_sequence_header_is_vacuously_identical() {
        let sh = obu_with_size(1, 0x42);
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: vec![Sample {
                offset: 0,
                size: sh.len() as u32,
                duration: 1,
                is_sync: true,
            }],
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &sh);
        assert!(audit.is_compliant());
        assert_eq!(audit.sequence_header_obu_count, 1);
        assert!(audit.sequence_headers_identical);
    }

    #[test]
    fn audit_avis_sequence_zero_sequence_headers_is_vacuously_identical() {
        // Samples that contain only non-SH OBUs (type 6 = OBU_FRAME).
        let frame = obu_with_size(6, 0xff);
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: vec![Sample {
                offset: 0,
                size: frame.len() as u32,
                duration: 1,
                is_sync: true,
            }],
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &frame);
        assert!(audit.is_compliant());
        assert_eq!(audit.sequence_header_obu_count, 0);
        assert!(audit.sequence_headers_identical);
    }

    #[test]
    fn audit_avis_sequence_out_of_range_samples_counted_and_skipped() {
        // Two samples; the first resolves to a valid SH OBU, the
        // second declares an offset beyond the file. The audit must
        // bump samples_out_of_range without flipping sequence_headers_
        // identical (the second sample is simply skipped).
        let sh = obu_with_size(1, 0x42);
        let meta = AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: vec![
                Sample {
                    offset: 0,
                    size: sh.len() as u32,
                    duration: 1,
                    is_sync: true,
                },
                Sample {
                    offset: 1_000_000,
                    size: 10,
                    duration: 1,
                    is_sync: false,
                },
            ],
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
        };
        let audit = audit_avis_sequence(&meta, &sh);
        assert_eq!(audit.sequence_header_obu_count, 1);
        assert!(audit.sequence_headers_identical);
        assert_eq!(audit.samples_out_of_range, 1);
        // Out-of-range samples don't flip a `shall` field — the
        // overall audit still passes if the resolvable samples were
        // self-consistent.
        assert!(audit.is_compliant());
    }

    /// `parse_avis` on the Netflix `alpha_video.avif` fixture
    /// populates the new fields with their declared values and the
    /// audit passes every §3 `shall`.
    #[test]
    fn alpha_video_avis_meets_section_3_compliance() {
        let bytes = include_bytes!("../tests/fixtures/alpha_video.avif");
        let meta = parse_avis(bytes).expect("parse_avis alpha_video");
        assert_eq!(meta.handler, Some(HANDLER_PICT));
        assert_eq!(meta.sample_description_types, vec![AV01]);
        let audit = audit_avis_sequence(&meta, bytes);
        assert!(
            audit.is_compliant(),
            "alpha_video.avif must satisfy av1-avif §3: {audit:?}"
        );
        assert_eq!(audit.observed_handler, Some(HANDLER_PICT));
        assert_eq!(audit.sample_description_count, 1);
        assert_eq!(audit.samples_out_of_range, 0);
        // Each AVIS sample carries a Temporal Delimiter + Frame
        // (no SH in non-first samples — the SH OBU lives only in
        // the very first sample under the still-image cadence) so
        // the total SH count is at least 1.
        assert!(audit.sequence_header_obu_count >= 1);
    }

    /// `sample_description_types_in_stbl` returns the declared
    /// SampleEntry types in order — three synthesized entries
    /// should round-trip verbatim.
    #[test]
    fn sample_description_types_round_trip() {
        let av1c_body: &[u8] = &[0x81, 0x04, 0x0c, 0x00];
        let mut av1c_box = Vec::new();
        av1c_box.extend_from_slice(&((8 + av1c_body.len()) as u32).to_be_bytes());
        av1c_box.extend_from_slice(b"av1C");
        av1c_box.extend_from_slice(av1c_body);
        let mut visual_header = vec![0u8; 78];
        visual_header[7] = 1;
        let mut av01_box = Vec::new();
        let av01_payload_len = visual_header.len() + av1c_box.len();
        av01_box.extend_from_slice(&((8 + av01_payload_len) as u32).to_be_bytes());
        av01_box.extend_from_slice(b"av01");
        av01_box.extend_from_slice(&visual_header);
        av01_box.extend_from_slice(&av1c_box);
        // A second entry of a foreign type to exercise the walker.
        let mut hvc1_box = Vec::new();
        hvc1_box.extend_from_slice(&8u32.to_be_bytes());
        hvc1_box.extend_from_slice(b"hvc1");
        let mut stsd_body = Vec::new();
        stsd_body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
        stsd_body.extend_from_slice(&2u32.to_be_bytes()); // entry_count
        stsd_body.extend_from_slice(&av01_box);
        stsd_body.extend_from_slice(&hvc1_box);
        let mut stsd_box = Vec::new();
        stsd_box.extend_from_slice(&((8 + stsd_body.len()) as u32).to_be_bytes());
        stsd_box.extend_from_slice(b"stsd");
        stsd_box.extend_from_slice(&stsd_body);
        let types = sample_description_types_in_stbl(&stsd_box);
        assert_eq!(types, vec![*b"av01", *b"hvc1"]);
    }

    /// `sample_description_types_in_stbl` returns an empty vector
    /// for a malformed (missing `stsd`) stbl rather than panicking.
    #[test]
    fn sample_description_types_missing_stsd_returns_empty() {
        let types = sample_description_types_in_stbl(&[]);
        assert!(types.is_empty());
    }

    // -----------------------------------------------------------------------
    // av1-avif v1.2.0 §8.2 / §8.3 — AVIS profile compliance audit
    // -----------------------------------------------------------------------

    use crate::derived::AvifProfile;

    /// Build a `BrandClass` declaring the requested profile brand(s).
    fn brands_with(baseline: bool, advanced: bool) -> crate::parser::BrandClass {
        crate::parser::BrandClass {
            is_image: true,
            is_miaf: true,
            is_baseline_profile: baseline,
            is_advanced_profile: advanced,
            ..crate::parser::BrandClass::default()
        }
    }

    /// `av1C` bytes (record header `0x81` then byte 1 = `(seq_profile <<
    /// 5) | seq_level_idx_0`, then two bytes of subsampling flags
    /// padded zero — enough to satisfy the byte-1 decode).
    fn av1c_with(seq_profile: u8, seq_level_idx_0: u8) -> Vec<u8> {
        let b1 = (seq_profile << 5) | (seq_level_idx_0 & 0x1F);
        vec![0x81, b1, 0x00, 0x00]
    }

    fn avis_meta_with_av1c(av1c: Option<Vec<u8>>) -> AvisMeta {
        AvisMeta {
            timescale: 1000,
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: av1c,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
        }
    }

    /// Empty-vector contract: a file that declares neither `MA1B` nor
    /// `MA1A` produces no audit records, even when an av1C is present.
    #[test]
    fn audit_avis_profile_no_brand_claim_short_circuits() {
        let meta = avis_meta_with_av1c(Some(av1c_with(0, 13)));
        let r = audit_avis_profile_compliance(&meta, &brands_with(false, false));
        assert!(r.is_empty());
    }

    /// Baseline track at the §8.2 edge: Main + level 5.1 (= 13). Passes.
    #[test]
    fn audit_avis_profile_baseline_main_level_5_1_compliant() {
        let meta = avis_meta_with_av1c(Some(av1c_with(0, 13)));
        let r = audit_avis_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].profile, AvifProfile::Baseline);
        assert_eq!(r[0].seq_profile, Some(0));
        assert_eq!(r[0].seq_level_idx_0, Some(13));
        assert!(r[0].is_compliant());
        assert!(r[0].missing().is_empty());
    }

    /// Baseline rejection: High Profile track under MA1B.
    #[test]
    fn audit_avis_profile_baseline_rejects_high_profile() {
        let meta = avis_meta_with_av1c(Some(av1c_with(1, 8)));
        let r = audit_avis_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert!(!r[0].is_compliant());
        assert!(r[0].missing().contains(&"avis-seq-profile-out-of-range"));
    }

    /// Baseline rejection: Main Profile but level 6.0 (= 16) under MA1B.
    #[test]
    fn audit_avis_profile_baseline_rejects_level_above_5_1() {
        let meta = avis_meta_with_av1c(Some(av1c_with(0, 16)));
        let r = audit_avis_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert!(!r[0].is_compliant());
        assert!(r[0].missing().contains(&"avis-seq-level-idx-out-of-range"));
    }

    /// Advanced at the §8.3 edge: High + level 6.0 (= 16). Passes.
    #[test]
    fn audit_avis_profile_advanced_high_level_6_0_compliant() {
        let meta = avis_meta_with_av1c(Some(av1c_with(1, 16)));
        let r = audit_avis_profile_compliance(&meta, &brands_with(false, true));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].profile, AvifProfile::Advanced);
        assert!(r[0].is_compliant());
    }

    /// Advanced accepts a Main-Profile track too — AV1 §A.2 makes Main
    /// a subset of High.
    #[test]
    fn audit_avis_profile_advanced_accepts_main_profile_track() {
        let meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        let r = audit_avis_profile_compliance(&meta, &brands_with(false, true));
        assert!(r[0].is_compliant());
    }

    /// Advanced rejects Professional Profile (`seq_profile == 2`).
    #[test]
    fn audit_avis_profile_advanced_rejects_professional_profile() {
        let meta = avis_meta_with_av1c(Some(av1c_with(2, 8)));
        let r = audit_avis_profile_compliance(&meta, &brands_with(false, true));
        assert!(!r[0].is_compliant());
        assert!(r[0].missing().contains(&"avis-seq-profile-out-of-range"));
    }

    /// Level 31 (AV1 §A.3 "Maximum parameters") is out of range for
    /// either profile since both profile clauses bound the level.
    #[test]
    fn audit_avis_profile_level_31_rejected_for_both_profiles() {
        let meta = avis_meta_with_av1c(Some(av1c_with(0, 31)));
        let r = audit_avis_profile_compliance(&meta, &brands_with(true, true));
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].profile, AvifProfile::Baseline);
        assert_eq!(r[1].profile, AvifProfile::Advanced);
        assert!(!r[0].is_compliant());
        assert!(!r[1].is_compliant());
        assert!(r[0].missing().contains(&"avis-seq-level-idx-out-of-range"));
        assert!(r[1].missing().contains(&"avis-seq-level-idx-out-of-range"));
    }

    /// Missing av1C produces a single token; the byte-decode fields
    /// are both `None` and `missing_av1c` is `true`.
    #[test]
    fn audit_avis_profile_missing_av1c_flagged_distinctly() {
        let meta = avis_meta_with_av1c(None);
        let r = audit_avis_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert!(r[0].missing_av1c);
        assert_eq!(r[0].seq_profile, None);
        assert_eq!(r[0].seq_level_idx_0, None);
        assert!(!r[0].is_compliant());
        assert_eq!(r[0].missing(), vec!["avis-track-missing-av1C"]);
    }

    /// Truncated av1C (less than 2 bytes) surfaces as both fields
    /// `None` but `missing_av1c == false` — distinct token.
    #[test]
    fn audit_avis_profile_truncated_av1c_flagged_distinctly() {
        let meta = avis_meta_with_av1c(Some(vec![0x81]));
        let r = audit_avis_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert!(!r[0].missing_av1c);
        assert_eq!(r[0].seq_profile, None);
        assert_eq!(r[0].seq_level_idx_0, None);
        assert!(!r[0].is_compliant());
        assert_eq!(r[0].missing(), vec!["avis-track-av1C-truncated"]);
    }

    /// File declaring both `MA1B` and `MA1A` yields two records (one
    /// per brand), in `Baseline`-then-`Advanced` declaration order.
    #[test]
    fn audit_avis_profile_dual_brand_emits_two_records_in_order() {
        let meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        let r = audit_avis_profile_compliance(&meta, &brands_with(true, true));
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].profile, AvifProfile::Baseline);
        assert_eq!(r[1].profile, AvifProfile::Advanced);
        // Both pass — Main + level ≤ 5.1.
        assert!(r[0].is_compliant());
        assert!(r[1].is_compliant());
    }

    // -----------------------------------------------------------------
    // Edit list (`edts/elst`) — ISO/IEC 14496-12 §8.6.6
    // -----------------------------------------------------------------

    fn full_box_bytes(version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut out = vec![
            version,
            (flags >> 16) as u8,
            (flags >> 8) as u8,
            flags as u8,
        ];
        out.extend_from_slice(body);
        out
    }

    fn wrap_box(btype: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut out = size.to_be_bytes().to_vec();
        out.extend_from_slice(btype);
        out.extend_from_slice(payload);
        out
    }

    /// Build a v0 `elst` payload from `(segment_duration, media_time,
    /// media_rate_integer, media_rate_fraction)` tuples.
    fn build_elst_v0(entries: &[(u32, i32, i16, i16)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for &(seg, mt, ri, rf) in entries {
            body.extend_from_slice(&seg.to_be_bytes());
            body.extend_from_slice(&mt.to_be_bytes());
            body.extend_from_slice(&ri.to_be_bytes());
            body.extend_from_slice(&rf.to_be_bytes());
        }
        wrap_box(b"elst", &full_box_bytes(0, 0, &body))
    }

    /// Build a v1 `elst` payload from `(segment_duration, media_time,
    /// media_rate_integer, media_rate_fraction)` tuples.
    fn build_elst_v1(entries: &[(u64, i64, i16, i16)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for &(seg, mt, ri, rf) in entries {
            body.extend_from_slice(&seg.to_be_bytes());
            body.extend_from_slice(&mt.to_be_bytes());
            body.extend_from_slice(&ri.to_be_bytes());
            body.extend_from_slice(&rf.to_be_bytes());
        }
        wrap_box(b"elst", &full_box_bytes(1, 0, &body))
    }

    /// Wrap a sequence of boxes as a `trak` containing an `edts`
    /// wrapping the given `elst`, then a `moov` wrapping that `trak`.
    fn wrap_moov_with_edts(elst_box: &[u8]) -> Vec<u8> {
        let edts = wrap_box(b"edts", elst_box);
        let trak = wrap_box(b"trak", &edts);
        wrap_box(b"moov", &trak)
    }

    /// A v0 single-entry normal edit round-trips into one
    /// `EditListEntry` with the same numeric fields, widened.
    #[test]
    fn parse_edit_list_v0_single_normal_entry_round_trips() {
        let elst = build_elst_v0(&[(1000, 200, 1, 0)]);
        let entries = parse_edit_list_box(&elst[8..]).expect("parse v0");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].segment_duration, 1000);
        assert_eq!(entries[0].media_time, 200);
        assert_eq!(entries[0].media_rate_integer, 1);
        assert_eq!(entries[0].media_rate_fraction, 0);
        assert!(!entries[0].is_empty_edit());
        assert!(!entries[0].is_dwell());
    }

    /// A v0 `media_time == -1` decodes to a sign-extended `-1` as the
    /// signed 64-bit field — the empty-edit predicate flips.
    #[test]
    fn parse_edit_list_v0_empty_edit_sign_extends_to_minus_one() {
        let elst = build_elst_v0(&[(500, -1, 1, 0)]);
        let entries = parse_edit_list_box(&elst[8..]).expect("parse v0");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].media_time, -1);
        assert!(entries[0].is_empty_edit());
    }

    /// A v1 entry's 64-bit `media_time` survives the round trip
    /// unaltered, including a value that cannot fit in v0's 32-bit
    /// field.
    #[test]
    fn parse_edit_list_v1_large_media_time_round_trips() {
        let big = 0x1_0000_0000i64; // > i32::MAX, only representable in v1
        let elst = build_elst_v1(&[(0xFFFF_FFFFu64 + 1, big, 1, 0)]);
        let entries = parse_edit_list_box(&elst[8..]).expect("parse v1");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].segment_duration, 0xFFFF_FFFFu64 + 1);
        assert_eq!(entries[0].media_time, big);
    }

    /// A truncated entry table stops the walk cleanly: every
    /// well-formed entry up to the truncation point is returned.
    #[test]
    fn parse_edit_list_truncated_entry_table_stops_walk() {
        // Build 3 entries on the wire, then truncate the payload after
        // entry 2.
        let full = build_elst_v0(&[(10, 0, 1, 0), (20, 5, 1, 0), (30, 9, 1, 0)]);
        // Drop the trailing 6 bytes — entry 3 becomes truncated.
        let truncated = &full[..full.len() - 6];
        let entries = parse_edit_list_box(&truncated[8..]).expect("parse v0");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].segment_duration, 10);
        assert_eq!(entries[1].segment_duration, 20);
    }

    /// `parse_avis` plumbs the edit list through a synthetic file:
    /// `moov[trak[edts[elst]] + trak2[stbl…]]` — but the AVIS parse
    /// requires the first track to also have an `stbl`, so synthesize a
    /// `trak` carrying both `edts` and an `mdia/minf/stbl`.
    #[test]
    fn find_first_track_edit_list_reads_v0_elst() {
        let elst = build_elst_v0(&[(0, -1, 1, 0), (200, 0, 1, 0)]);
        let moov = wrap_moov_with_edts(&elst);
        // `find_first_track_edit_list` takes a `moov_payload`
        // (interior of the `moov` box).
        let entries = find_first_track_edit_list(&moov[8..]);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_empty_edit());
        assert_eq!(entries[1].segment_duration, 200);
    }

    /// A track without `edts` produces an empty edit list.
    #[test]
    fn find_first_track_edit_list_absent_edts_returns_empty() {
        let trak = wrap_box(b"trak", &[]);
        let moov = wrap_box(b"moov", &trak);
        let entries = find_first_track_edit_list(&moov[8..]);
        assert!(entries.is_empty());
    }

    /// A future-version (v2+) `elst` is silently ignored — we surface
    /// no entries rather than reject the file. The §8.6.6.3 audit
    /// trivially passes for the resulting empty edit list (a forward-
    /// compatible reader can fall back to identity mapping).
    #[test]
    fn parse_edit_list_unknown_version_returns_empty() {
        let body = vec![0u8, 0, 0, 1, 0, 0, 0, 0]; // entry_count = 1, garbage entry
        let elst = wrap_box(b"elst", &full_box_bytes(2, 0, &body));
        let entries = parse_edit_list_box(&elst[8..]).expect("parse box");
        assert!(entries.is_empty());
    }

    /// `audit_edit_list` against a meta with an empty `edit_list` (no
    /// `edts` in the file) reports both `shall`s as vacuously
    /// satisfied — `is_compliant()` is `true`, `missing()` is empty.
    #[test]
    fn audit_edit_list_empty_is_vacuously_compliant() {
        let meta = avis_meta_with_av1c(None);
        let r = audit_edit_list(&meta);
        assert!(r.is_compliant());
        assert!(r.missing().is_empty());
        assert_eq!(r.entry_count, 0);
        assert_eq!(r.empty_edit_count, 0);
        assert_eq!(r.dwell_entry_count, 0);
    }

    /// A two-entry edit list with a leading empty edit followed by a
    /// normal segment (the canonical "offset the start by N units"
    /// shape from §8.6.6.1) satisfies both audited `shall`s.
    #[test]
    fn audit_edit_list_leading_empty_then_normal_is_compliant() {
        let mut meta = avis_meta_with_av1c(None);
        meta.edit_list = vec![
            EditListEntry {
                segment_duration: 1000,
                media_time: -1,
                media_rate_integer: 1,
                media_rate_fraction: 0,
            },
            EditListEntry {
                segment_duration: 5000,
                media_time: 0,
                media_rate_integer: 1,
                media_rate_fraction: 0,
            },
        ];
        let r = audit_edit_list(&meta);
        assert!(r.is_compliant());
        assert_eq!(r.entry_count, 2);
        assert_eq!(r.empty_edit_count, 1);
        assert_eq!(r.dwell_entry_count, 0);
    }

    /// A trailing empty edit trips §8.6.6.3 ("The last edit in a
    /// track shall never be an empty edit").
    #[test]
    fn audit_edit_list_trailing_empty_flagged() {
        let mut meta = avis_meta_with_av1c(None);
        meta.edit_list = vec![
            EditListEntry {
                segment_duration: 500,
                media_time: 0,
                media_rate_integer: 1,
                media_rate_fraction: 0,
            },
            EditListEntry {
                segment_duration: 100,
                media_time: -1,
                media_rate_integer: 1,
                media_rate_fraction: 0,
            },
        ];
        let r = audit_edit_list(&meta);
        assert!(!r.is_compliant());
        assert_eq!(r.empty_edit_count, 1);
        assert_eq!(r.missing(), vec!["avis-edit-list-last-entry-empty"]);
    }

    /// A dwell entry (`media_rate_integer == 0`) is permitted by
    /// §8.6.6.3 and increments `dwell_entry_count` without flipping
    /// `media_rate_in_range`.
    #[test]
    fn audit_edit_list_dwell_entry_is_compliant_and_counted() {
        let mut meta = avis_meta_with_av1c(None);
        meta.edit_list = vec![EditListEntry {
            segment_duration: 250,
            media_time: 33,
            media_rate_integer: 0,
            media_rate_fraction: 0,
        }];
        let r = audit_edit_list(&meta);
        assert!(r.is_compliant());
        assert_eq!(r.dwell_entry_count, 1);
        assert_eq!(r.out_of_range_rate_count, 0);
    }

    /// A `media_rate_integer` other than `0` or `1` (here, `2`)
    /// trips the §8.6.6.3 rate `shall`.
    #[test]
    fn audit_edit_list_out_of_range_media_rate_flagged() {
        let mut meta = avis_meta_with_av1c(None);
        meta.edit_list = vec![EditListEntry {
            segment_duration: 100,
            media_time: 0,
            media_rate_integer: 2,
            media_rate_fraction: 0,
        }];
        let r = audit_edit_list(&meta);
        assert!(!r.is_compliant());
        assert_eq!(r.out_of_range_rate_count, 1);
        assert_eq!(r.missing(), vec!["avis-edit-list-media-rate-out-of-range"]);
    }

    /// A negative `media_rate_integer` also fails the §8.6.6.3 set
    /// constraint — the audit doesn't carve out negative values.
    #[test]
    fn audit_edit_list_negative_media_rate_flagged() {
        let mut meta = avis_meta_with_av1c(None);
        meta.edit_list = vec![EditListEntry {
            segment_duration: 100,
            media_time: 0,
            media_rate_integer: -1,
            media_rate_fraction: 0,
        }];
        let r = audit_edit_list(&meta);
        assert!(!r.is_compliant());
        assert_eq!(r.out_of_range_rate_count, 1);
    }

    /// Both `shall`s can fail simultaneously — a trailing empty edit
    /// with an out-of-range rate flips both flags + emits both
    /// diagnostic tokens.
    #[test]
    fn audit_edit_list_both_shalls_fail_simultaneously() {
        let mut meta = avis_meta_with_av1c(None);
        meta.edit_list = vec![EditListEntry {
            segment_duration: 100,
            media_time: -1,
            media_rate_integer: 7,
            media_rate_fraction: 0,
        }];
        let r = audit_edit_list(&meta);
        assert!(!r.is_compliant());
        let m = r.missing();
        assert!(m.contains(&"avis-edit-list-media-rate-out-of-range"));
        assert!(m.contains(&"avis-edit-list-last-entry-empty"));
    }
}
