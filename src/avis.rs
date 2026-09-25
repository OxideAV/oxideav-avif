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
//! `oxideav_av1`'s registry decoder to decode frames end-to-end. The
//! decoder needs the track's `AV1CodecConfigurationRecord` to seed its
//! sequence header — that record is extracted from `stsd` → `av01` →
//! `av1C` and surfaced as [`AvisMeta::av1_codec_config`].

use crate::error::{AvifError as Error, Result};

use crate::box_parser::{b, find_box, iter_boxes, parse_full_box, read_u32, read_u64, BoxType};
use oxideav_heif::{sequence, HeifFile};

const MOOV: BoxType = b(b"moov");
const TRAK: BoxType = b(b"trak");
const STBL: BoxType = b(b"stbl");
const AV01: BoxType = b(b"av01");
const PRFT: BoxType = b(b"prft");
const SSIX: BoxType = b(b"ssix");

/// NTP-epoch offset relative to the Unix epoch, in seconds. The NTP
/// timescale counts seconds since 1900-01-01T00:00:00Z; the Unix epoch
/// is 1970-01-01T00:00:00Z, which is 70 years (17 leap days included)
/// = 2_208_988_800 seconds later. Used to convert a `prft`
/// `ntp_timestamp` to a Unix instant. Spec: RFC 5905 §6.
pub const NTP_UNIX_EPOCH_OFFSET_SECONDS: u64 = 2_208_988_800;

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

    /// Convert this entry's [`Self::media_time`] (in media-timescale
    /// units, per ISO/IEC 14496-12 §8.6.6) to seconds using the
    /// supplied `media_timescale` (`mdhd::timescale`, §8.4.2.2).
    ///
    /// Returns `None` when:
    ///
    /// * `media_timescale == 0` — the conversion is undefined.
    /// * `self.is_empty_edit()` — the entry signals "no media is
    ///   presented" rather than a position on the media timeline, so
    ///   "seconds into the media" is meaningless. Callers wanting the
    ///   raw `-1` sentinel use [`Self::media_time`] directly.
    pub fn media_time_seconds(&self, media_timescale: u32) -> Option<f64> {
        if media_timescale == 0 || self.is_empty_edit() {
            None
        } else {
            Some(self.media_time as f64 / media_timescale as f64)
        }
    }

    /// Convert this entry's [`Self::segment_duration`] (in
    /// movie-timescale units, per ISO/IEC 14496-12 §8.6.6) to
    /// seconds using the supplied `movie_timescale`
    /// (`mvhd::timescale`, §8.2.2). Returns `None` when
    /// `movie_timescale == 0` (the conversion is undefined).
    pub fn segment_duration_seconds(&self, movie_timescale: u32) -> Option<f64> {
        if movie_timescale == 0 {
            None
        } else {
            Some(self.segment_duration as f64 / movie_timescale as f64)
        }
    }
}

/// One `prft` ProducerReferenceTimeBox (ISO/IEC 14496-12 §8.16.5).
///
/// `prft` is a top-level (`File`-container) box that supplies a
/// wall-clock instant at which a movie fragment / segment was produced.
/// It relates to the **next** `moof` in bitstream order, binding an NTP
/// UTC time to a media time on one reference track so a real-time
/// consumer can pace itself against the producer. AVIS sequences carried
/// as fragmented / segmented files (`moof`-based) may carry one or more
/// of these before each fragment; they sit alongside `styp` / `sidx`,
/// not inside `moov`.
///
/// All fields are widened to their largest version shape (`u64`
/// `media_time`) so callers stay version-agnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProducerReferenceTime {
    /// FullBox `version` (`0` or `1`). v0 carries a 32-bit `media_time`;
    /// v1 widens it to 64-bit. Other versions are rejected at parse.
    pub version: u8,
    /// FullBox `flags`. The 2015 edition sets this to `0`; later
    /// editions define named bits annotating what the NTP time
    /// represents (encoder input/output, finalization, file-write,
    /// arbitrary association). Preserved verbatim for those readers.
    pub flags: u32,
    /// `track_ID` of the reference track this time is associated with
    /// (ISO/IEC 14496-12 §8.16.5.3).
    pub reference_track_id: u32,
    /// UTC instant in NTP 64-bit format: high 32 bits are seconds since
    /// 1900-01-01, low 32 bits are fractional seconds (RFC 5905).
    pub ntp_timestamp: u64,
    /// The same instant expressed in the reference track's media
    /// timescale (`mdhd::timescale`). v0 sources this from a 32-bit
    /// field; v1 from a 64-bit field.
    pub media_time: u64,
}

impl ProducerReferenceTime {
    /// Whether the §8.16.5 `flags` mark the NTP time as the frame's
    /// encoder input/output time (`0x000001`, later-edition bit).
    pub fn is_encoder_input_output(&self) -> bool {
        self.flags & 0x000001 != 0
    }

    /// Whether the §8.16.5 `flags` mark the NTP time as the segment
    /// finalization (complete) time (`0x000002`, later-edition bit).
    pub fn is_finalization_time(&self) -> bool {
        self.flags & 0x000002 != 0
    }

    /// Whether the §8.16.5 `flags` mark the NTP time as the file /
    /// segment write time (`0x000004`, later-edition bit).
    pub fn is_file_write_time(&self) -> bool {
        self.flags & 0x000004 != 0
    }

    /// Integer NTP seconds-since-1900 (high 32 bits of
    /// [`Self::ntp_timestamp`]).
    pub fn ntp_seconds(&self) -> u32 {
        (self.ntp_timestamp >> 32) as u32
    }

    /// Fractional NTP seconds (low 32 bits of [`Self::ntp_timestamp`]),
    /// in units of 1/2^32 second.
    pub fn ntp_fraction(&self) -> u32 {
        (self.ntp_timestamp & 0xFFFF_FFFF) as u32
    }

    /// Convert [`Self::ntp_timestamp`] to floating-point seconds since
    /// the Unix epoch (1970-01-01). Returns `None` when the NTP integer
    /// part predates the Unix epoch (i.e. before 1970), which an AVIS
    /// producer time should never be.
    pub fn unix_seconds(&self) -> Option<f64> {
        let secs = self.ntp_seconds() as u64;
        let unix_secs = secs.checked_sub(NTP_UNIX_EPOCH_OFFSET_SECONDS)?;
        let frac = self.ntp_fraction() as f64 / u64::from(u32::MAX).wrapping_add(1) as f64;
        Some(unix_secs as f64 + frac)
    }
}

/// Parse a single `prft` box payload (the bytes after the
/// size/type header, including the FullBox `version`/`flags`).
///
/// Body layout (ISO/IEC 14496-12 §8.16.5.2): `reference_track_ID(32)`,
/// `ntp_timestamp(64)`, then `media_time` — 32-bit for v0, 64-bit for
/// v1. Total body 16 bytes (v0) or 20 bytes (v1) after the 4-byte
/// FullBox prefix. Rejects versions other than 0/1 and truncated bodies.
pub fn parse_prft(payload: &[u8]) -> Result<ProducerReferenceTime> {
    let (version, flags, rest) = parse_full_box(payload)?;
    if version > 1 {
        return Err(Error::InvalidData(format!(
            "avis: prft version {version} unsupported (expected 0 or 1)"
        )));
    }
    let reference_track_id = read_u32(rest, 0)?;
    let ntp_timestamp = read_u64(rest, 4)?;
    let media_time = if version == 0 {
        u64::from(read_u32(rest, 12)?)
    } else {
        read_u64(rest, 12)?
    };
    Ok(ProducerReferenceTime {
        version,
        flags,
        reference_track_id,
        ntp_timestamp,
        media_time,
    })
}

/// Collect every top-level `prft` ProducerReferenceTimeBox in the file,
/// in bitstream order (ISO/IEC 14496-12 §8.16.5). `prft` is a
/// `File`-container box that precedes the movie fragment it documents,
/// so it is found by walking the file's top-level boxes — not inside
/// `moov`. Returns an empty vector for a non-fragmented AVIS (the common
/// case). A malformed `prft` is skipped rather than failing the whole
/// walk so a still-image / non-fragmented file's other boxes stay
/// reachable.
pub fn parse_producer_reference_times(file: &[u8]) -> Vec<ProducerReferenceTime> {
    let mut out = Vec::new();
    for hdr in iter_boxes(file) {
        let Ok(hdr) = hdr else { break };
        if hdr.box_type != PRFT {
            continue;
        }
        let payload = &file[hdr.payload_start..hdr.end()];
        if let Ok(prft) = parse_prft(payload) {
            out.push(prft);
        }
    }
    out
}

/// One `(level, range_size)` pair inside an `ssix` subsegment
/// (ISO/IEC 14496-12 §8.16.4 SubsegmentIndexBox).
///
/// A subsegment is partitioned into contiguous byte ranges, one per
/// `leva` Level Assignment Box level. `level` names the level (matching
/// a `leva` entry); `range_size` is the byte length of this level's
/// slice of the subsegment. Ranges within a subsegment are contiguous
/// and together cover every byte of it, so a client can fetch a partial
/// subsegment (e.g. just the base layer) by summing leading ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubsegmentRange {
    /// Level (per the `leva` Level Assignment Box) to which this partial
    /// subsegment is assigned. `unsigned int(8)` on the wire.
    pub level: u8,
    /// Size in bytes of this level's contiguous byte range within the
    /// subsegment. `unsigned int(24)` on the wire (max 16 MiB − 1).
    pub range_size: u32,
}

/// One `ssix` SubsegmentIndexBox (ISO/IEC 14496-12 §8.16.4).
///
/// `ssix` is a top-level (`File`-container) box that sits immediately
/// after the `sidx` Segment Index Box it documents and maps each
/// indexed subsegment to a list of level→byte-range partitions. With
/// the companion `leva` box it lets a client fetch byte-range subsets of
/// a subsegment (partial-subsegment access), e.g. a temporal base layer
/// without higher enhancement layers. Fragmented / segmented AVIS
/// sequences (DASH-style `moof`-based delivery) may carry these
/// alongside `styp` / `sidx` / `prft`; a non-fragmented still-image
/// AVIF never does.
///
/// `subsegment_count` shall equal the `reference_count` of the preceding
/// `sidx`; each subsegment's `range_count` shall be ≥ 2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubsegmentIndex {
    /// One entry per indexed subsegment, in bitstream order. Each is the
    /// ordered list of `(level, range_size)` partitions covering that
    /// subsegment. The outer length is the box's `subsegment_count`.
    pub subsegments: Vec<Vec<SubsegmentRange>>,
}

/// Parse a single `ssix` box payload (the bytes after the size/type
/// header, including the FullBox `version`/`flags`).
///
/// Body layout (ISO/IEC 14496-12 §8.16.4.2): `subsegment_count(32)`,
/// then for each subsegment `range_count(32)` followed by `range_count`
/// `(level: u8, range_size: u24)` pairs (4 bytes each). The box is
/// `FullBox('ssix', 0, 0)`; versions other than 0 and truncated bodies
/// are rejected. All integers big-endian.
pub fn parse_ssix(payload: &[u8]) -> Result<SubsegmentIndex> {
    let (version, _flags, rest) = parse_full_box(payload)?;
    if version != 0 {
        return Err(Error::InvalidData(format!(
            "avis: ssix version {version} unsupported (expected 0)"
        )));
    }
    let subsegment_count = read_u32(rest, 0)?;
    let mut subsegments = Vec::with_capacity(subsegment_count.min(1024) as usize);
    let mut at = 4usize;
    for _ in 0..subsegment_count {
        let range_count = read_u32(rest, at)?;
        at = at
            .checked_add(4)
            .ok_or_else(|| Error::InvalidData("avis: ssix offset overflow".to_string()))?;
        let mut ranges = Vec::with_capacity(range_count.min(1024) as usize);
        for _ in 0..range_count {
            let level = *rest
                .get(at)
                .ok_or_else(|| Error::InvalidData("avis: ssix truncated range".to_string()))?;
            // `range_size` is 24-bit big-endian: bytes at+1..=at+3.
            let b0 = u32::from(
                *rest
                    .get(at + 1)
                    .ok_or_else(|| Error::InvalidData("avis: ssix truncated range".to_string()))?,
            );
            let b1 = u32::from(
                *rest
                    .get(at + 2)
                    .ok_or_else(|| Error::InvalidData("avis: ssix truncated range".to_string()))?,
            );
            let b2 = u32::from(
                *rest
                    .get(at + 3)
                    .ok_or_else(|| Error::InvalidData("avis: ssix truncated range".to_string()))?,
            );
            let range_size = (b0 << 16) | (b1 << 8) | b2;
            ranges.push(SubsegmentRange { level, range_size });
            at = at
                .checked_add(4)
                .ok_or_else(|| Error::InvalidData("avis: ssix offset overflow".to_string()))?;
        }
        subsegments.push(ranges);
    }
    Ok(SubsegmentIndex { subsegments })
}

/// Collect every top-level `ssix` SubsegmentIndexBox in the file, in
/// bitstream order (ISO/IEC 14496-12 §8.16.4). `ssix` is a
/// `File`-container box that follows the `sidx` it documents, so it is
/// found by walking the file's top-level boxes — not inside `moov`.
/// Returns an empty vector for a non-fragmented AVIS (the common case).
/// A malformed `ssix` is skipped rather than failing the whole walk so a
/// still-image / non-fragmented file's other boxes stay reachable.
pub fn parse_subsegment_indexes(file: &[u8]) -> Vec<SubsegmentIndex> {
    let mut out = Vec::new();
    for hdr in iter_boxes(file) {
        let Ok(hdr) = hdr else { break };
        if hdr.box_type != SSIX {
            continue;
        }
        let payload = &file[hdr.payload_start..hdr.end()];
        if let Ok(ssix) = parse_ssix(payload) {
            out.push(ssix);
        }
    }
    out
}

/// Container-side description of an AVIS image sequence.
#[derive(Clone, Debug)]
pub struct AvisMeta {
    /// Movie timescale from `mvhd`. A duration of `timescale` == 1s.
    pub timescale: u32,
    /// Media timescale from the first track's `mdia/mdhd`
    /// (ISO/IEC 14496-12 §8.4.2.2). Clock-ticks per second for the
    /// media timeline (the timeline `media_time` and sample
    /// `delta`s live on). `None` when `mdhd` is missing or
    /// malformed — callers can either fall back to assuming
    /// `media_timescale == timescale` (a common encoder default
    /// per §8.6.5) or surface the absence to the caller. Distinct
    /// from `timescale` (which is movie-level via `mvhd`): an
    /// `EditListBox` entry's `segment_duration` is in
    /// movie-timescale units, but its `media_time` and every
    /// sample table delta are in media-timescale units.
    pub media_timescale: Option<u32>,
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
    /// Sample-to-group mappings decoded from the first track's `stbl`
    /// (`sbgp` ISO/IEC 14496-12:2015 §8.9.2 and/or `csgp`
    /// :2020 §8.9.5). One entry per `grouping_type` present. Empty when
    /// the track carries no sample-grouping boxes. Each entry's
    /// per-sample group index is recovered via
    /// [`crate::sample_group::SampleToGroup::group_index_for_sample`].
    pub sample_to_groups: Vec<crate::sample_group::SampleToGroup>,
    /// Sample-group description headers from the first track's `stbl`
    /// (`sgpd` §8.9.3). One entry per `grouping_type`; pair each with
    /// the matching [`Self::sample_to_groups`] entry by `grouping_type`.
    /// Only the generic header (grouping type, default index, entry
    /// count) is decoded — per-entry payloads are grouping-type
    /// specific and not interpreted here.
    pub sample_group_descriptions: Vec<crate::sample_group::SampleGroupDescription>,
    /// Top-level `prft` ProducerReferenceTimeBox entries in bitstream
    /// order (ISO/IEC 14496-12 §8.16.5). Each binds an NTP UTC instant
    /// to a media time on its `reference_track_id`, documenting when a
    /// fragment / segment was produced. Empty for a non-fragmented AVIS
    /// (the common case) — `prft` only appears in `moof`-based
    /// fragmented / segmented files.
    pub producer_reference_times: Vec<ProducerReferenceTime>,
    /// Top-level `ssix` SubsegmentIndexBox entries in bitstream order
    /// (ISO/IEC 14496-12 §8.16.4). Each maps a `sidx`-indexed
    /// subsegment to its `leva`-level byte-range partitions, enabling
    /// partial-subsegment (byte-range) access. Empty for a
    /// non-fragmented AVIS (the common case) — `ssix` only appears in
    /// `sidx`-indexed fragmented / segmented files.
    pub subsegment_indexes: Vec<SubsegmentIndex>,
}

/// Walk the container and build a sample table. The input buffer must
/// contain the full file — sample offsets are absolute.
///
/// The `moov` / `trak` / `stbl` walk is the container crate's
/// ([`oxideav_heif::sequence::parse_movie`]): the first track's sample
/// table (`stts` / `stsc` / `stsz` / `stco` + `co64` / `stss`, bounded
/// expansion, every sample range checked against the file), its
/// `stsd` entries (`av01` + `av1C`), `tkhd` extents, `mdhd` timescale,
/// `hdlr` and `elst` edit list. This crate adds what the container
/// does not model: the sample-group boxes (`sbgp` / `csgp` / `sgpd`)
/// of that `stbl`, and the top-level `prft` / `ssix` boxes.
///
/// Returns `Error::InvalidData` when a required box is missing or
/// inconsistent (ISO/IEC 14496-12 makes `mvhd`, `tkhd`, `mdhd`, `hdlr`
/// and `stts` mandatory). AVIS files lacking a `moov` produce an error.
pub fn parse_avis(file: &[u8]) -> Result<AvisMeta> {
    if find_box(file, &MOOV)?.is_none() {
        return Err(Error::InvalidData("avis: missing moov".to_string()));
    }
    let heif = HeifFile::parse(file)?;
    let movie = sequence::parse_movie(&heif)?
        .ok_or_else(|| Error::InvalidData("avis: missing moov".to_string()))?;
    let track = movie
        .tracks
        .first()
        .ok_or_else(|| Error::InvalidData("avis: missing trak/mdia/minf/stbl".to_string()))?;
    let samples = track
        .samples
        .iter()
        .map(|s| Sample {
            offset: s.offset,
            size: s.size,
            duration: s.duration,
            is_sync: s.is_sync,
        })
        .collect();
    let av1_codec_config = track
        .sample_entries
        .iter()
        .find(|e| e.entry_type == AV01)
        .and_then(|e| e.av1c.as_ref().map(|c| c.raw.clone()));
    let sample_description_types = track.sample_entries.iter().map(|e| e.entry_type).collect();
    let edit_list = track
        .edits
        .iter()
        .map(|e| EditListEntry {
            segment_duration: e.segment_duration,
            media_time: e.media_time,
            media_rate_integer: (e.media_rate >> 16) as i16,
            media_rate_fraction: (e.media_rate & 0xffff) as i16,
        })
        .collect();
    let (sample_to_groups, sample_group_descriptions) = match first_track_stbl(&heif, file) {
        Some(stbl) => (
            crate::sample_group::parse_sample_to_groups(stbl),
            crate::sample_group::parse_sample_group_descriptions(stbl),
        ),
        None => (Vec::new(), Vec::new()),
    };
    let producer_reference_times = parse_producer_reference_times(file);
    let subsegment_indexes = parse_subsegment_indexes(file);
    Ok(AvisMeta {
        timescale: movie.timescale,
        media_timescale: Some(track.timescale),
        display_dims: Some((track.width, track.height)),
        samples,
        av1_codec_config,
        handler: Some(track.handler),
        sample_description_types,
        edit_list,
        sample_to_groups,
        sample_group_descriptions,
        producer_reference_times,
        subsegment_indexes,
    })
}

/// The raw `stbl` payload of the first track, located through the
/// container's flattened box walk (`moov` → `trak` → `mdia` → `minf`
/// → `stbl`), for the sample-group boxes this crate parses itself.
fn first_track_stbl<'a>(heif: &HeifFile, file: &'a [u8]) -> Option<&'a [u8]> {
    let walk = heif.box_walk().ok()?;
    let mut in_first_trak = false;
    for entry in &walk {
        let t = &entry.header.box_type;
        if entry.depth == 1 && t == &TRAK {
            if in_first_trak {
                break;
            }
            in_first_trak = true;
            continue;
        }
        if in_first_trak && t == &STBL {
            let h = &entry.header;
            return file.get(h.payload_start..h.end());
        }
    }
    None
}

/// Duration of one sample as a `(numerator, denominator)` pair of
/// seconds; a zero timescale yields `(duration, 1)` rather than a
/// division by zero.
pub fn sample_duration_seconds(duration: u32, timescale: u32) -> (u32, u32) {
    if timescale == 0 {
        (duration, 1)
    } else {
        (duration, timescale)
    }
}

/// Resolve a sample's byte slice inside the source file.
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

// ===========================================================================
// AVIS aggregator — `inspect_avis` / `AvisInfo`
// ===========================================================================
//
// Mirror of the still-image [`crate::inspect::AvifInfo`] one-call builder
// for AVIS files. A single call to [`inspect_avis`] parses the `ftyp` +
// `moov` chain once and fans every available container-side audit
// (`audit_avis_sequence`, `audit_avis_profile_compliance`, `audit_edit_list`)
// into a single record so callers do not have to thread `parse_avis`
// + `classify_brands` + each audit by hand. The shape parallels the
// repeated "Followup: the AVIS path's `AvifInfo` does not yet surface
// the audit the way `AvifInfo::avif_profile_compliance` does for items"
// notes in the per-round README sections (r201 / r206 / r212).
//
// The aggregator does not introduce new `shall`-level normative
// material — every audited rule is forwarded verbatim from the
// existing per-audit walkers; the value is one-call ergonomics +
// a `Vec`-free record shape `AvifInfo` already exposes downstream.

/// High-level container-side description of an AVIS (AV1 Image
/// Sequence) file — the AVIS counterpart to the still-image
/// [`crate::inspect::AvifInfo`].
///
/// Built by [`inspect_avis`]: one call walks `ftyp` + `moov` + the
/// first track's `stbl` once, then runs every container-side audit
/// the crate publishes. Callers that want a single record summarising
/// "what does this AVIS look like, and which AVIS-level `shall`s does
/// it satisfy?" can replace a hand-rolled
/// `parse_header` + `classify_brands` + `parse_avis` + N audits with
/// this one record.
///
/// The struct deliberately omits AVIS-internal byte-level material
/// (sample byte offsets, full `av1C` payload, full edit-list entries)
/// — those remain accessible via [`parse_avis`] when needed. The
/// fields surfaced here are summary signals + the three compliance
/// records.
#[derive(Clone, Debug)]
pub struct AvisInfo {
    /// `mvhd::timescale` — clock-ticks per second for the movie
    /// timeline. ISO/IEC 14496-12 §8.2.2.
    pub timescale: u32,
    /// `mdhd::timescale` from the first track's `mdia/mdhd`
    /// (ISO/IEC 14496-12 §8.4.2.2) — clock-ticks per second for the
    /// media timeline. `None` when `mdhd` is missing or malformed.
    /// `EditListEntry::media_time` and every `stts` per-sample
    /// `delta` live on this timeline. Distinct from
    /// [`Self::timescale`] (movie-level via `mvhd`); the two often
    /// agree but are independent fields per the spec.
    pub media_timescale: Option<u32>,
    /// `tkhd` width / height. `None` when `tkhd` is missing.
    pub display_dims: Option<(u32, u32)>,
    /// Number of samples (frames) walked from the first track's
    /// `stbl`. `0` when the track has no samples — a degenerate but
    /// parseable case.
    pub sample_count: u32,
    /// Sum of every sample's `stts` delta in movie-timescale units.
    /// This is the track media duration as carried by the sample
    /// table; `tkhd::duration` may differ (it is reported in movie
    /// timescale via `mvhd`). Callers can convert to seconds via
    /// `(total_sample_duration as f64) / (timescale as f64)`.
    pub total_sample_duration: u64,
    /// `true` when the track's `stsd → av01 → av1C` walk produced a
    /// valid configuration record. `false` when the AVIS track is
    /// not AV1-coded or the config record is missing — the file is
    /// then not decodable as an AV1 Image Sequence regardless of
    /// brand claims.
    pub has_av1_codec_config: bool,
    /// `mdia/hdlr/handler_type` four-CC. av1-avif v1.2.0 §3 mandates
    /// [`HANDLER_PICT`] (`'pict'`). `None` when no `hdlr` could be
    /// located.
    pub handler: Option<BoxType>,
    /// `stbl/stsd` SampleEntry types in declaration order. For a
    /// compliant AVIS this is `['av01']`.
    pub sample_description_types: Vec<BoxType>,
    /// Brand classification from `ftyp` (av1-avif §6 / §8, ISO/IEC
    /// 23000-22 §7). Useful for callers deciding whether the file
    /// claims a specific profile gate.
    pub brands: crate::parser::BrandClass,
    /// `true` when the first track carries an `edts/elst` with at
    /// least one entry (the §8.6.5 implicit-identity case is `false`).
    pub has_edit_list: bool,
    /// Number of sample-to-group mappings (`sbgp`/`csgp`) decoded from
    /// the first track's `stbl` — one per `grouping_type`. `0` when the
    /// track carries no sample-grouping boxes. The full mappings are on
    /// [`AvisMeta::sample_to_groups`] via [`parse_avis`].
    pub sample_to_group_count: u32,
    /// av1-avif v1.2.0 §3 `shall`-level audit (track handler `'pict'`,
    /// single `'av01'` SampleEntry, identical Sequence Header OBUs
    /// across samples). One record per AVIS file (each AVIS carries
    /// one image-sequence track).
    pub sequence_compliance: AvisSequenceCompliance,
    /// av1-avif v1.2.0 §8.2 / §8.3 profile `shall`-level audit, one
    /// entry per declared AVIF profile brand. Empty when the file
    /// declares neither `MA1B` nor `MA1A`. Spec: §8.2 (`MA1B`),
    /// §8.3 (`MA1A`).
    pub profile_compliance: Vec<AvisProfileCompliance>,
    /// ISO/IEC 14496-12 §8.6.6.3 edit-list `shall`-level audit. A
    /// file without an `edts` produces a vacuously-compliant record
    /// (both `shall`s trivially satisfied).
    pub edit_list_compliance: EditListCompliance,
}

impl AvisInfo {
    /// Number of samples (frames) in the track. Shorthand for
    /// [`Self::sample_count`].
    pub fn frame_count(&self) -> u32 {
        self.sample_count
    }

    /// Total track media duration in seconds, computed from
    /// [`Self::total_sample_duration`] and [`Self::timescale`].
    /// Returns `None` when `timescale == 0` (the conversion is
    /// undefined).
    pub fn duration_seconds(&self) -> Option<f64> {
        if self.timescale == 0 {
            None
        } else {
            Some(self.total_sample_duration as f64 / self.timescale as f64)
        }
    }

    /// Total track media duration in seconds, computed from
    /// [`Self::total_sample_duration`] (the sum of every sample's
    /// `stts` delta) and [`Self::media_timescale`]. This is the
    /// spec-correct conversion: `stts` per-sample `delta` values are
    /// in media-timescale units per ISO/IEC 14496-12 §8.6.1.2.
    ///
    /// Returns `None` when [`Self::media_timescale`] is `None`
    /// (`mdhd` missing) or `0` (the conversion is undefined).
    /// Callers that lack a media timescale can either:
    ///
    /// * Fall back to [`Self::duration_seconds`] when the encoder
    ///   sets `mvhd::timescale == mdhd::timescale` (a common
    ///   default), or
    /// * Surface the absence to the caller (no presentation-duration
    ///   answer is available without an `mdhd`).
    pub fn media_duration_seconds(&self) -> Option<f64> {
        match self.media_timescale {
            Some(ts) if ts != 0 => Some(self.total_sample_duration as f64 / ts as f64),
            _ => None,
        }
    }

    /// `true` when every audited `shall` across §3, §8.2/§8.3, and
    /// ISO/IEC 14496-12 §8.6.6.3 passes for this AVIS file.
    ///
    /// Trivially `true` for the empty edit list (the implicit-identity
    /// case) and for a file that declares no AVIF profile brand
    /// (`profile_compliance` empty). Callers that want a "compliance
    /// AND a profile claim was made" gate should combine this with
    /// `brands.is_baseline_profile || brands.is_advanced_profile`.
    pub fn is_compliant_all(&self) -> bool {
        self.sequence_compliance.is_compliant()
            && self.profile_compliance.iter().all(|c| c.is_compliant())
            && self.edit_list_compliance.is_compliant()
    }

    /// Aggregated list of every audited `shall` that failed across
    /// the three AVIS audits, in deterministic order: §3 sequence
    /// tokens (`avis-*`) first, then §8.2/§8.3 profile tokens (one
    /// group per record, in `Baseline`-then-`Advanced` order), then
    /// §8.6.6.3 edit-list tokens.
    ///
    /// Empty when [`Self::is_compliant_all`] returns `true`.
    pub fn missing_all(&self) -> Vec<&'static str> {
        let mut out = self.sequence_compliance.missing();
        for rec in &self.profile_compliance {
            out.extend(rec.missing());
        }
        out.extend(self.edit_list_compliance.missing());
        out
    }

    /// `true` when the file's `ftyp` declares the `avis` brand —
    /// i.e. the AVIS classification was claimed by the encoder, not
    /// just inferred from a `moov` presence.
    pub fn is_avis_brand(&self) -> bool {
        self.brands.is_sequence
    }
}

/// One-call AVIS inspector — parses the `ftyp` + `moov` chain once
/// and folds every container-side audit into a single
/// [`AvisInfo`] record.
///
/// Returns an error when:
///
/// * `ftyp` is missing or its brands cannot be classified (per
///   [`crate::parser::classify_brands`]).
/// * `moov` is missing or the first track lacks an `stbl` — both
///   surface as `Error::InvalidData` from [`parse_avis`].
///
/// Mirrors the still-image [`crate::inspect::inspect`] entry point.
/// The AVIS path does not need the still-image grid / alpha
/// composition logic because every AVIS frame is a single AV1 sample
/// and the per-frame structure is owned by the AV1 decoder.
pub fn inspect_avis(file: &[u8]) -> Result<AvisInfo> {
    let hdr = crate::parser::parse_header(file)?;
    let brands = crate::parser::classify_brands(&hdr.major_brand, &hdr.compatible_brands)?;
    let meta = parse_avis(file)?;
    Ok(build_avis_info(meta, brands, file))
}

/// Internal aggregator: fold a parsed [`AvisMeta`] + classified
/// [`crate::parser::BrandClass`] + the source file bytes into a single
/// [`AvisInfo`] record by running every container-side AVIS audit.
///
/// Exposed at module scope so the test harness can drive it on
/// synthetic `AvisMeta` values without round-tripping through
/// `parse_avis` — every behaviour walker in here is also covered by
/// the per-audit unit tests above; `AvisInfo`'s role is to surface
/// the three audit records together with a small number of summary
/// fields.
fn build_avis_info(meta: AvisMeta, brands: crate::parser::BrandClass, file: &[u8]) -> AvisInfo {
    let total_sample_duration: u64 = meta
        .samples
        .iter()
        .map(|s| u64::from(s.duration))
        .sum::<u64>();
    let sample_count = meta.samples.len() as u32;

    let sequence_compliance = audit_avis_sequence(&meta, file);
    let profile_compliance = audit_avis_profile_compliance(&meta, &brands);
    let edit_list_compliance = audit_edit_list(&meta);

    AvisInfo {
        timescale: meta.timescale,
        media_timescale: meta.media_timescale,
        display_dims: meta.display_dims,
        sample_count,
        total_sample_duration,
        has_av1_codec_config: meta.av1_codec_config.is_some(),
        handler: meta.handler,
        sample_description_types: meta.sample_description_types,
        brands,
        has_edit_list: !meta.edit_list.is_empty(),
        sample_to_group_count: meta.sample_to_groups.len() as u32,
        sequence_compliance,
        profile_compliance,
        edit_list_compliance,
    }
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

    /// One `av01` visual sample entry (78-byte header) carrying `av1c`.
    fn av01_entry_box(av1c_body: &[u8]) -> Vec<u8> {
        let mut visual_header = vec![0u8; 78];
        visual_header[7] = 1; // data_reference_index
        let mut payload = visual_header;
        payload.extend_from_slice(&wrap_box(b"av1C", av1c_body));
        wrap_box(b"av01", &payload)
    }

    /// An `stsd` box around the given entry boxes.
    fn stsd_box(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut body = (entries.len() as u32).to_be_bytes().to_vec();
        for e in entries {
            body.extend_from_slice(e);
        }
        wrap_box(b"stsd", &full_box_bytes(0, 0, &body))
    }

    /// A complete AVIS file around a sample table: `ftyp` (`avis`) +
    /// `moov { mvhd, trak { tkhd, [edts], mdia { [mdhd], hdlr, minf {
    /// stbl } } } }` + a `free` box so sample offsets up to `pad` bytes
    /// lie inside the file. `stbl` is the raw `stbl` payload; when it
    /// carries no `stsd` a one-entry `av01` `stsd` is prepended.
    struct SyntheticMovie {
        stbl: Vec<u8>,
        elst_box: Option<Vec<u8>>,
        mdhd_box: Option<Vec<u8>>,
        pad: usize,
    }

    impl SyntheticMovie {
        fn new(stbl: Vec<u8>) -> Self {
            Self {
                stbl,
                elst_box: None,
                mdhd_box: Some(build_mdhd_v0(600)),
                pad: 512,
            }
        }

        fn file(&self) -> Vec<u8> {
            let mut ftyp = b"avis".to_vec();
            ftyp.extend_from_slice(&0u32.to_be_bytes());
            ftyp.extend_from_slice(b"avismsf1miaf");
            let ftyp = wrap_box(b"ftyp", &ftyp);
            let has_stsd = self.stbl.windows(4).any(|w| w == b"stsd");
            let mut stbl = Vec::new();
            if !has_stsd {
                stbl.extend_from_slice(&stsd_box(&[av01_entry_box(&[0x81, 0x00, 0x0c, 0x00])]));
            }
            stbl.extend_from_slice(&self.stbl);
            let mut mdia = Vec::new();
            if let Some(m) = &self.mdhd_box {
                mdia.extend_from_slice(m);
            }
            let mut hdlr = 0u32.to_be_bytes().to_vec();
            hdlr.extend_from_slice(b"pict");
            hdlr.extend_from_slice(&[0u8; 12]);
            hdlr.push(0);
            mdia.extend_from_slice(&wrap_box(b"hdlr", &full_box_bytes(0, 0, &hdlr)));
            mdia.extend_from_slice(&wrap_box(b"minf", &wrap_box(b"stbl", &stbl)));
            let mut tkhd = vec![0u8; 8]; // creation_time, modification_time
            tkhd.extend_from_slice(&1u32.to_be_bytes()); // track_ID
            tkhd.extend_from_slice(&[0u8; 4]); // reserved
            tkhd.extend_from_slice(&6u32.to_be_bytes()); // duration
            tkhd.extend_from_slice(&[0u8; 16]);
            for m in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
                tkhd.extend_from_slice(&m.to_be_bytes());
            }
            tkhd.extend_from_slice(&(4u32 << 16).to_be_bytes());
            tkhd.extend_from_slice(&(4u32 << 16).to_be_bytes());
            let mut trak = wrap_box(b"tkhd", &full_box_bytes(0, 3, &tkhd));
            if let Some(e) = &self.elst_box {
                trak.extend_from_slice(&wrap_box(b"edts", e));
            }
            trak.extend_from_slice(&wrap_box(b"mdia", &mdia));
            let mut mvhd = vec![0u8; 8];
            mvhd.extend_from_slice(&1000u32.to_be_bytes());
            mvhd.extend_from_slice(&6u32.to_be_bytes());
            mvhd.extend_from_slice(&[0u8; 80]);
            let mut moov = wrap_box(b"mvhd", &full_box_bytes(0, 0, &mvhd));
            moov.extend_from_slice(&wrap_box(b"trak", &trak));
            let mut out = ftyp;
            out.extend_from_slice(&wrap_box(b"moov", &moov));
            if out.len() < self.pad {
                let fill = self.pad - out.len();
                out.extend_from_slice(&wrap_box(b"free", &vec![0u8; fill.saturating_sub(8)]));
            }
            out
        }
    }

    /// Build a v0 `mdhd` payload (FullBox) with the given timescale.
    /// Layout: creation(4) + modification(4) + timescale(4) +
    /// duration(4) + language(2) + pre_defined(2).
    fn build_mdhd_v0(timescale: u32) -> Vec<u8> {
        let mut body = vec![0u8; 20];
        body[8..12].copy_from_slice(&timescale.to_be_bytes());
        wrap_box(b"mdhd", &full_box_bytes(0, 0, &body))
    }

    /// An `stsc` claiming `u32::MAX` samples in one chunk must never
    /// drive an allocation of that size: the container either refuses
    /// the table or bounds the expansion to what `stsz` declares (one
    /// sample here).
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
        let stts_body = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
        let stsc_body = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&u32::MAX.to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
        let stsz_body = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&1u32.to_be_bytes());
            b
        };
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
        match parse_avis(&SyntheticMovie::new(stbl).file()) {
            Ok(meta) => assert_eq!(meta.samples.len(), 1, "expansion bounded by stsz"),
            Err(Error::InvalidData(s)) => assert!(
                s.contains("sample"),
                "expected the sample-count bound message, got: {s}"
            ),
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn sample_table_three_samples() {
        let file = SyntheticMovie::new(minimal_stbl()).file();
        let samples = parse_avis(&file).unwrap().samples;
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
        let mut stbl = Vec::new();
        let wrap = |t: &[u8; 4], p: &[u8]| {
            let size = (8 + p.len()) as u32;
            let mut out = size.to_be_bytes().to_vec();
            out.extend_from_slice(t);
            out.extend_from_slice(p);
            out
        };
        stbl.extend_from_slice(&wrap(b"stsc", &[0u8; 8]));
        stbl.extend_from_slice(&wrap(b"stsz", &[0u8; 12]));
        stbl.extend_from_slice(&wrap(b"stco", &[0u8; 8]));
        let err = parse_avis(&SyntheticMovie::new(stbl).file()).unwrap_err();
        match err {
            Error::InvalidData(_) => {}
            _ => panic!("expected InvalidData"),
        }
    }

    #[test]
    fn sample_table_absent_stss_marks_all_sync() {
        let full = minimal_stbl();
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
        let samples = parse_avis(&SyntheticMovie::new(stbl_no_stss).file())
            .unwrap()
            .samples;
        assert_eq!(samples.len(), 3);
        assert!(samples.iter().all(|s| s.is_sync));
    }

    #[test]
    fn stsd_av01_av1c_extraction_round_trip() {
        let av1c_body: &[u8] = &[0x81, 0x04, 0x0c, 0x00];
        let mut stbl = stsd_box(&[av01_entry_box(av1c_body)]);
        stbl.extend_from_slice(&minimal_stbl());
        let meta = parse_avis(&SyntheticMovie::new(stbl).file()).expect("parse");
        assert_eq!(
            meta.av1_codec_config.as_deref(),
            Some(av1c_body),
            "extracted av1C body must match the synthesized payload byte-for-byte"
        );
    }

    #[test]
    fn stsd_missing_av01_returns_none() {
        // entry_count = 0: no sample description at all — the track is
        // either refused or carries no av1C, never a stale one.
        let mut stbl = stsd_box(&[]);
        stbl.extend_from_slice(&minimal_stbl());
        match parse_avis(&SyntheticMovie::new(stbl).file()) {
            Ok(meta) => {
                assert!(
                    meta.av1_codec_config.is_none(),
                    "empty entry_count must produce no av1C"
                );
                assert!(meta.sample_description_types.is_empty());
            }
            Err(Error::InvalidData(_)) => {}
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn stsd_truncated_av01_payload_returns_none() {
        // An av01 entry shorter than its 78-byte visual header: refused
        // as malformed, or read with no av1C — never a panic.
        let mut stbl = stsd_box(&[wrap_box(b"av01", &[0u8; 32])]);
        stbl.extend_from_slice(&minimal_stbl());
        match parse_avis(&SyntheticMovie::new(stbl).file()) {
            Ok(meta) => assert!(
                meta.av1_codec_config.is_none(),
                "truncated av01 payload must not yield an av1C"
            ),
            Err(Error::InvalidData(_)) => {}
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
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
            media_timescale: Some(1000),
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
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
            media_timescale: Some(1000),
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: Some(*b"vide"),
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
            media_timescale: Some(1000),
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: None,
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
            media_timescale: Some(1000),
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![AV01, AV01],
            edit_list: Vec::new(),
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
            media_timescale: Some(1000),
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: Vec::new(),
            edit_list: Vec::new(),
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
            media_timescale: Some(1000),
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: None,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![*b"hvc1"],
            edit_list: Vec::new(),
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
            media_timescale: Some(1000),
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
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
            media_timescale: Some(1000),
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
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
            media_timescale: Some(1000),
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
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
            media_timescale: Some(1000),
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
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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

    #[test]
    fn sample_description_types_round_trip() {
        let av1c_body: &[u8] = &[0x81, 0x04, 0x0c, 0x00];
        let mut hvc1_header = vec![0u8; 78];
        hvc1_header[7] = 1;
        let mut stbl = stsd_box(&[av01_entry_box(av1c_body), wrap_box(b"hvc1", &hvc1_header)]);
        stbl.extend_from_slice(&minimal_stbl());
        let meta = parse_avis(&SyntheticMovie::new(stbl).file()).expect("parse");
        assert_eq!(meta.sample_description_types, vec![*b"av01", *b"hvc1"]);
    }

    #[test]
    fn sample_description_types_missing_stsd_returns_empty() {
        // A stbl without stsd (entry_count 0 stands in: the walk has no
        // entries to type): no description types are reported.
        let mut stbl = stsd_box(&[]);
        stbl.extend_from_slice(&minimal_stbl());
        let types = parse_avis(&SyntheticMovie::new(stbl).file())
            .map(|m| m.sample_description_types)
            .unwrap_or_default();
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
            media_timescale: Some(1000),
            display_dims: None,
            samples: Vec::new(),
            av1_codec_config: av1c,
            handler: Some(HANDLER_PICT),
            sample_description_types: vec![AV01],
            edit_list: Vec::new(),
            sample_to_groups: Vec::new(),
            sample_group_descriptions: Vec::new(),
            producer_reference_times: Vec::new(),
            subsegment_indexes: Vec::new(),
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
    fn edit_list_of(elst_box: Vec<u8>) -> Result<Vec<EditListEntry>> {
        let mut m = SyntheticMovie::new(minimal_stbl());
        m.elst_box = Some(elst_box);
        parse_avis(&m.file()).map(|m| m.edit_list)
    }

    #[test]
    fn parse_edit_list_v0_single_normal_entry_round_trips() {
        let entries = edit_list_of(build_elst_v0(&[(1000, 200, 1, 0)])).expect("parse v0");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].segment_duration, 1000);
        assert_eq!(entries[0].media_time, 200);
        assert_eq!(entries[0].media_rate_integer, 1);
        assert_eq!(entries[0].media_rate_fraction, 0);
        assert!(!entries[0].is_empty_edit());
        assert!(!entries[0].is_dwell());
    }

    #[test]
    fn parse_edit_list_v0_empty_edit_sign_extends_to_minus_one() {
        let entries = edit_list_of(build_elst_v0(&[(500, -1, 1, 0)])).expect("parse v0");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].media_time, -1);
        assert!(entries[0].is_empty_edit());
    }

    #[test]
    fn parse_edit_list_v1_large_media_time_round_trips() {
        let big = 0x1_0000_0000i64; // > i32::MAX, only representable in v1
        let entries =
            edit_list_of(build_elst_v1(&[(0xFFFF_FFFFu64 + 1, big, 1, 0)])).expect("parse v1");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].segment_duration, 0xFFFF_FFFFu64 + 1);
        assert_eq!(entries[0].media_time, big);
    }

    /// An `elst` whose entry table ends mid-entry is malformed: the
    /// file is refused, or at most the complete entries are read —
    /// never a partial entry.
    #[test]
    fn parse_edit_list_truncated_entry_table_stops_walk() {
        let full = build_elst_v0(&[(10, 0, 1, 0), (20, 5, 1, 0), (30, 9, 1, 0)]);
        let mut truncated = full[..full.len() - 6].to_vec();
        let size = truncated.len() as u32;
        truncated[..4].copy_from_slice(&size.to_be_bytes());
        match edit_list_of(truncated) {
            Ok(entries) => {
                assert!(entries.len() <= 2);
                for (e, seg) in entries.iter().zip([10u64, 20]) {
                    assert_eq!(e.segment_duration, seg);
                }
            }
            Err(Error::InvalidData(_)) => {}
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn find_first_track_edit_list_reads_v0_elst() {
        let entries = edit_list_of(build_elst_v0(&[(0, -1, 1, 0), (200, 0, 1, 0)])).expect("parse");
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_empty_edit());
        assert_eq!(entries[1].segment_duration, 200);
    }

    #[test]
    fn find_first_track_edit_list_absent_edts_returns_empty() {
        let meta = parse_avis(&SyntheticMovie::new(minimal_stbl()).file()).expect("parse");
        assert!(meta.edit_list.is_empty());
    }

    // -----------------------------------------------------------------
    // `mdhd` media-timescale plumb (ISO/IEC 14496-12 §8.4.2.2)
    // -----------------------------------------------------------------

    /// Build a v1 `mdhd` payload (FullBox) with the given timescale.
    /// Layout: creation(8) + modification(8) + timescale(4) +
    /// duration(8) + language(2) + pre_defined(2).
    fn build_mdhd_v1(timescale: u32) -> Vec<u8> {
        let mut body = vec![0u8; 36];
        body[16..20].copy_from_slice(&timescale.to_be_bytes());
        wrap_box(b"mdhd", &full_box_bytes(1, 0, &body))
    }

    /// Wrap an `mdhd` box inside `mdia` inside `trak` inside `moov`.
    fn media_timescale_of(mdhd_box: Option<Vec<u8>>) -> Result<Option<u32>> {
        let mut m = SyntheticMovie::new(minimal_stbl());
        m.mdhd_box = mdhd_box;
        parse_avis(&m.file()).map(|m| m.media_timescale)
    }

    #[test]
    fn find_first_track_media_timescale_v0_reads_timescale() {
        let ts = media_timescale_of(Some(build_mdhd_v0(24_000))).expect("parse");
        assert_eq!(ts, Some(24_000));
    }

    #[test]
    fn find_first_track_media_timescale_v1_reads_timescale() {
        let ts = media_timescale_of(Some(build_mdhd_v1(90_000))).expect("parse");
        assert_eq!(ts, Some(90_000));
    }

    /// `mdhd` is mandatory (ISO/IEC 14496-12 §8.4.2): a track without
    /// one is refused rather than read with an unknown timescale.
    #[test]
    fn find_first_track_media_timescale_absent_mdhd_returns_none() {
        assert!(matches!(
            media_timescale_of(None),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn find_first_track_media_timescale_unknown_version_returns_none() {
        // An mdhd of an unknown version has no defined timescale field:
        // the file is refused, or no usable (non-zero) timescale comes
        // out of it.
        let mdhd = wrap_box(b"mdhd", &full_box_bytes(2, 0, &[0u8; 36]));
        match media_timescale_of(Some(mdhd)) {
            Ok(ts) => assert_eq!(ts.unwrap_or(0), 0),
            Err(Error::InvalidData(_)) => {}
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn find_first_track_media_timescale_truncated_v0_returns_none() {
        let mdhd = wrap_box(b"mdhd", &full_box_bytes(0, 0, &[0u8; 8]));
        match media_timescale_of(Some(mdhd)) {
            Ok(ts) => assert_eq!(ts, None),
            Err(Error::InvalidData(_)) => {}
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
    }

    /// `EditListEntry::media_time_seconds` divides `media_time` by
    /// the supplied media-timescale.
    #[test]
    fn edit_list_entry_media_time_seconds_v0_normal_entry() {
        let e = EditListEntry {
            segment_duration: 1000,
            media_time: 9000,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        };
        // 9000 ticks / 90_000 ticks-per-second = 0.1 s
        let s = e.media_time_seconds(90_000).expect("seconds");
        assert!((s - 0.1).abs() < 1e-9);
    }

    /// An empty edit (`media_time == -1`) reports `None` rather than
    /// `-1/timescale` — the sentinel is not a position on the media
    /// timeline.
    #[test]
    fn edit_list_entry_media_time_seconds_empty_edit_returns_none() {
        let e = EditListEntry {
            segment_duration: 1000,
            media_time: -1,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        };
        assert!(e.media_time_seconds(90_000).is_none());
    }

    /// A media-timescale of zero is the undefined-conversion case and
    /// surfaces `None`.
    #[test]
    fn edit_list_entry_media_time_seconds_zero_timescale_returns_none() {
        let e = EditListEntry {
            segment_duration: 1000,
            media_time: 50,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        };
        assert!(e.media_time_seconds(0).is_none());
    }

    /// `segment_duration_seconds` divides `segment_duration` by the
    /// supplied movie-timescale.
    #[test]
    fn edit_list_entry_segment_duration_seconds_typical_case() {
        let e = EditListEntry {
            segment_duration: 12_000,
            media_time: 0,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        };
        // 12_000 ticks / 24_000 movie ticks-per-second = 0.5 s
        let s = e.segment_duration_seconds(24_000).expect("seconds");
        assert!((s - 0.5).abs() < 1e-9);
    }

    /// Zero movie-timescale is the undefined-conversion case.
    #[test]
    fn edit_list_entry_segment_duration_seconds_zero_timescale_returns_none() {
        let e = EditListEntry {
            segment_duration: 12_000,
            media_time: 0,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        };
        assert!(e.segment_duration_seconds(0).is_none());
    }

    /// An `elst` of an unknown version carries no readable entries:
    /// refused, or read as an empty edit list.
    #[test]
    fn parse_edit_list_unknown_version_returns_empty() {
        let body = vec![0u8, 0, 0, 1, 0, 0, 0, 0]; // entry_count = 1, garbage entry
        let elst = wrap_box(b"elst", &full_box_bytes(2, 0, &body));
        match edit_list_of(elst) {
            Ok(entries) => assert!(entries.is_empty()),
            Err(Error::InvalidData(_)) => {}
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
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

    // -----------------------------------------------------------------
    // AVIS aggregator (`inspect_avis` / `AvisInfo` / `build_avis_info`)
    // -----------------------------------------------------------------

    /// A fully-compliant synthetic AVIS aggregates to every audit
    /// passing + the four summary fields agreeing with the underlying
    /// `AvisMeta`. The §3 sequence audit's SH-identity check is
    /// vacuously satisfied (no samples → zero SH OBUs walked, which
    /// trivially passes the §3 byte-identity `shall`).
    #[test]
    fn build_avis_info_aggregates_a_clean_avis() {
        let mut meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        meta.timescale = 24_000;
        meta.display_dims = Some((640, 480));
        meta.samples = vec![
            Sample {
                offset: 0,
                size: 0,
                duration: 1000,
                is_sync: true,
            },
            Sample {
                offset: 0,
                size: 0,
                duration: 1000,
                is_sync: false,
            },
        ];

        let info = build_avis_info(meta, brands_with(true, false), &[]);
        assert_eq!(info.timescale, 24_000);
        assert_eq!(info.display_dims, Some((640, 480)));
        assert_eq!(info.sample_count, 2);
        assert_eq!(info.total_sample_duration, 2_000);
        assert_eq!(info.frame_count(), 2);
        assert!(info.has_av1_codec_config);
        assert_eq!(info.handler, Some(HANDLER_PICT));
        assert_eq!(info.sample_description_types, vec![AV01]);
        assert!(info.brands.is_baseline_profile);
        assert!(!info.has_edit_list);
        assert!(info.sequence_compliance.is_compliant());
        assert_eq!(info.profile_compliance.len(), 1);
        assert_eq!(info.profile_compliance[0].profile, AvifProfile::Baseline);
        assert!(info.profile_compliance[0].is_compliant());
        assert!(info.edit_list_compliance.is_compliant());
        assert!(info.is_compliant_all());
        assert!(info.missing_all().is_empty());
        // 2 samples / 24_000 ticks-per-second = 1/12 s.
        let d = info.duration_seconds().unwrap();
        assert!((d - (2000.0 / 24_000.0)).abs() < 1e-9);
    }

    /// `duration_seconds()` returns `None` for the degenerate
    /// `timescale == 0` case (division undefined).
    #[test]
    fn build_avis_info_duration_seconds_undefined_when_timescale_zero() {
        let mut meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        meta.timescale = 0;
        let info = build_avis_info(meta, brands_with(false, false), &[]);
        assert!(info.duration_seconds().is_none());
    }

    /// A file declaring neither `MA1B` nor `MA1A` produces an empty
    /// `profile_compliance` vector, and `is_compliant_all()` reflects
    /// the §3 + §8.6.6.3 outcome alone. With the synth meta being
    /// otherwise clean, both pass — the empty profile vector is
    /// vacuously compliant.
    #[test]
    fn build_avis_info_no_brand_claim_empty_profile_records() {
        let meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        let info = build_avis_info(meta, brands_with(false, false), &[]);
        assert!(info.profile_compliance.is_empty());
        assert!(info.is_compliant_all());
        assert!(!info.is_avis_brand());
    }

    /// A file declaring both `MA1B` and `MA1A` produces two profile
    /// records in `Baseline`-then-`Advanced` order. The aggregator
    /// preserves the per-audit ordering.
    #[test]
    fn build_avis_info_dual_brand_records_in_order() {
        let meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        let info = build_avis_info(meta, brands_with(true, true), &[]);
        assert_eq!(info.profile_compliance.len(), 2);
        assert_eq!(info.profile_compliance[0].profile, AvifProfile::Baseline);
        assert_eq!(info.profile_compliance[1].profile, AvifProfile::Advanced);
    }

    /// `is_avis_brand()` reflects the `BrandClass::is_sequence` flag
    /// — i.e. did `ftyp` actually list the `avis` brand. A file with a
    /// `moov` but no `avis` claim still parses but is not classified
    /// as AVIS by the encoder.
    #[test]
    fn build_avis_info_is_avis_brand_tracks_sequence_flag() {
        let meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        let mut brands = brands_with(false, false);
        brands.is_sequence = true;
        let info = build_avis_info(meta, brands, &[]);
        assert!(info.is_avis_brand());
    }

    /// A track failing both §3 (handler mismatch) and §8.6.6.3
    /// (trailing empty edit) flips `is_compliant_all()` to false and
    /// emits both groups of tokens in `missing_all()`, deterministically
    /// ordered: §3 tokens first, then §8.6.6.3 tokens.
    #[test]
    fn build_avis_info_missing_all_concatenates_in_audit_order() {
        let mut meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        meta.handler = Some(*b"vide"); // not 'pict' → §3 failure
        meta.edit_list = vec![EditListEntry {
            segment_duration: 100,
            media_time: -1, // empty edit at the trailing position
            media_rate_integer: 1,
            media_rate_fraction: 0,
        }];

        let info = build_avis_info(meta, brands_with(true, false), &[]);
        assert!(!info.is_compliant_all());
        let m = info.missing_all();
        assert!(
            m.contains(&"avis-handler-not-pict"),
            "§3 token must be present: {m:?}"
        );
        assert!(
            m.contains(&"avis-edit-list-last-entry-empty"),
            "§8.6.6.3 token must be present: {m:?}"
        );
        // Ordering invariant: §3 tokens precede §8.6.6.3 tokens.
        let i_seq = m
            .iter()
            .position(|t| *t == "avis-handler-not-pict")
            .unwrap();
        let i_elst = m
            .iter()
            .position(|t| *t == "avis-edit-list-last-entry-empty")
            .unwrap();
        assert!(
            i_seq < i_elst,
            "§3 tokens must precede §8.6.6.3 tokens: {m:?}"
        );
    }

    /// `total_sample_duration` is the sum of every sample's `duration`
    /// widened to u64 — guards against accidental `u32` overflow when
    /// the track ships many large-duration samples.
    #[test]
    fn build_avis_info_total_sample_duration_widens_to_u64() {
        let mut meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        meta.samples = vec![
            Sample {
                offset: 0,
                size: 0,
                duration: u32::MAX,
                is_sync: true,
            },
            Sample {
                offset: 0,
                size: 0,
                duration: u32::MAX,
                is_sync: false,
            },
        ];
        let info = build_avis_info(meta, brands_with(false, false), &[]);
        assert_eq!(info.total_sample_duration, 2 * u64::from(u32::MAX));
    }

    /// `has_edit_list` is `true` when the parsed edit list has any
    /// entries — the §8.6.5 implicit-identity case (empty edit list)
    /// reports `false`.
    #[test]
    fn build_avis_info_has_edit_list_reflects_entry_presence() {
        let empty = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        let info_empty = build_avis_info(empty, brands_with(false, false), &[]);
        assert!(!info_empty.has_edit_list);

        let mut with_elst = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        with_elst.edit_list = vec![EditListEntry {
            segment_duration: 100,
            media_time: 0,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        }];
        let info_present = build_avis_info(with_elst, brands_with(false, false), &[]);
        assert!(info_present.has_edit_list);
    }

    /// `has_av1_codec_config` is `false` when `av1C` could not be
    /// located in the track's `stsd → av01` chain — the AVIS is then
    /// not decodable even if every brand claim is otherwise sound.
    #[test]
    fn build_avis_info_has_av1_codec_config_tracks_av1c_presence() {
        let meta = avis_meta_with_av1c(None);
        let info = build_avis_info(meta, brands_with(false, false), &[]);
        assert!(!info.has_av1_codec_config);
    }

    /// `AvisInfo::media_timescale` carries forward the `AvisMeta`
    /// field set by `parse_avis` from the first track's `mdia/mdhd`.
    #[test]
    fn build_avis_info_media_timescale_carries_forward() {
        let mut meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        meta.media_timescale = Some(90_000);
        let info = build_avis_info(meta, brands_with(false, false), &[]);
        assert_eq!(info.media_timescale, Some(90_000));
    }

    /// `AvisInfo::media_timescale` is `None` when the underlying
    /// `mdhd` could not be located — distinguishes "no mdhd" from
    /// "explicit 0" (the latter also returns None from the helper but
    /// is a separate signal callers can branch on if they walk the
    /// raw meta).
    #[test]
    fn build_avis_info_media_timescale_none_when_mdhd_absent() {
        let mut meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        meta.media_timescale = None;
        let info = build_avis_info(meta, brands_with(false, false), &[]);
        assert_eq!(info.media_timescale, None);
        // media_duration_seconds inherits the None — there's no
        // timescale to convert against.
        assert!(info.media_duration_seconds().is_none());
    }

    /// `media_duration_seconds` divides `total_sample_duration` (the
    /// sum of `stts` per-sample deltas, in media-timescale units) by
    /// the media timescale — distinct from `duration_seconds` (which
    /// uses the movie timescale). When the two timescales agree the
    /// two helpers report the same number; when they differ this
    /// helper is the spec-correct one.
    #[test]
    fn build_avis_info_media_duration_seconds_uses_media_timescale() {
        let mut meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        // mvhd timescale: 1000 (movie ticks/sec).
        // mdhd timescale: 90_000 (media ticks/sec — 90 kHz, a common
        // MPEG-TS-style choice independent of the movie timeline).
        meta.timescale = 1000;
        meta.media_timescale = Some(90_000);
        meta.samples = vec![
            Sample {
                offset: 0,
                size: 0,
                duration: 3000,
                is_sync: true,
            },
            Sample {
                offset: 0,
                size: 0,
                duration: 3000,
                is_sync: false,
            },
        ];
        let info = build_avis_info(meta, brands_with(false, false), &[]);
        // total = 6000 media ticks; / 90_000 = 1/15 s.
        let media_s = info.media_duration_seconds().expect("media seconds");
        assert!((media_s - 6000.0 / 90_000.0).abs() < 1e-12);
        // duration_seconds divides by the movie timescale (1000) — same
        // accumulator interpreted under a different unit. Documents
        // why this round added the explicit media-timescale helper.
        let movie_s = info.duration_seconds().expect("movie seconds");
        assert!((movie_s - 6.0).abs() < 1e-9);
        assert!((media_s - movie_s).abs() > 1e-3);
    }

    /// `media_duration_seconds` returns `None` when the underlying
    /// `media_timescale` is `Some(0)` (degenerate) — division would
    /// be undefined. Distinct from the `None` case (mdhd absent),
    /// which also short-circuits to `None` via the helper.
    #[test]
    fn build_avis_info_media_duration_seconds_zero_media_timescale_none() {
        let mut meta = avis_meta_with_av1c(Some(av1c_with(0, 8)));
        meta.media_timescale = Some(0);
        meta.samples = vec![Sample {
            offset: 0,
            size: 0,
            duration: 50,
            is_sync: true,
        }];
        let info = build_avis_info(meta, brands_with(false, false), &[]);
        assert!(info.media_duration_seconds().is_none());
    }

    // ----- prft ProducerReferenceTimeBox (ISO/IEC 14496-12 §8.16.5) -----

    /// Build a `prft` body (FullBox prefix + fields) for the given
    /// version. `media_time` is truncated to 32 bits for v0.
    fn prft_body(version: u8, flags: u32, track_id: u32, ntp: u64, media_time: u64) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&track_id.to_be_bytes());
        body.extend_from_slice(&ntp.to_be_bytes());
        if version == 0 {
            body.extend_from_slice(&(media_time as u32).to_be_bytes());
        } else {
            body.extend_from_slice(&media_time.to_be_bytes());
        }
        full_box_bytes(version, flags, &body)
    }

    #[test]
    fn parse_prft_v0_reads_32bit_media_time() {
        let ntp = (3_900_000_000u64 << 32) | 0x8000_0000;
        let payload = prft_body(0, 0, 7, ntp, 123_456);
        let p = parse_prft(&payload).unwrap();
        assert_eq!(p.version, 0);
        assert_eq!(p.flags, 0);
        assert_eq!(p.reference_track_id, 7);
        assert_eq!(p.ntp_timestamp, ntp);
        assert_eq!(p.media_time, 123_456);
        assert_eq!(p.ntp_seconds(), 3_900_000_000);
        assert_eq!(p.ntp_fraction(), 0x8000_0000);
    }

    #[test]
    fn parse_prft_v1_reads_64bit_media_time() {
        let big = 0x1_0000_0001u64; // exceeds u32 — must not truncate
        let payload = prft_body(1, 0, 1, 0, big);
        let p = parse_prft(&payload).unwrap();
        assert_eq!(p.version, 1);
        assert_eq!(p.media_time, big);
    }

    #[test]
    fn parse_prft_rejects_unknown_version() {
        let payload = prft_body(1, 0, 1, 0, 0);
        let mut bad = payload.clone();
        bad[0] = 2; // version 2
        assert!(parse_prft(&bad).is_err());
    }

    #[test]
    fn parse_prft_rejects_truncated_v0_body() {
        // 4-byte FullBox prefix + 12 bytes (missing the 4-byte media_time).
        let payload = &prft_body(0, 0, 1, 0, 0)[..16];
        assert!(parse_prft(payload).is_err());
    }

    #[test]
    fn parse_prft_rejects_truncated_v1_body() {
        // v1 needs 20 body bytes after the prefix; cut to 16.
        let payload = &prft_body(1, 0, 1, 0, 0)[..16];
        assert!(parse_prft(payload).is_err());
    }

    #[test]
    fn prft_flag_bits_decode() {
        let p = parse_prft(&prft_body(0, 0x000001, 1, 0, 0)).unwrap();
        assert!(p.is_encoder_input_output());
        assert!(!p.is_finalization_time());
        let p = parse_prft(&prft_body(0, 0x000002, 1, 0, 0)).unwrap();
        assert!(p.is_finalization_time());
        let p = parse_prft(&prft_body(0, 0x000004, 1, 0, 0)).unwrap();
        assert!(p.is_file_write_time());
    }

    #[test]
    fn prft_unix_seconds_converts_ntp_epoch() {
        // NTP seconds == NTP_UNIX_EPOCH_OFFSET → Unix instant 0.0.
        let ntp = NTP_UNIX_EPOCH_OFFSET_SECONDS << 32;
        let p = parse_prft(&prft_body(0, 0, 1, ntp, 0)).unwrap();
        let unix = p.unix_seconds().unwrap();
        assert!((unix - 0.0).abs() < 1e-6, "got {unix}");

        // Add 10 seconds and half a fractional second.
        let ntp = ((NTP_UNIX_EPOCH_OFFSET_SECONDS + 10) << 32) | 0x8000_0000;
        let p = parse_prft(&prft_body(0, 0, 1, ntp, 0)).unwrap();
        let unix = p.unix_seconds().unwrap();
        assert!((unix - 10.5).abs() < 1e-6, "got {unix}");
    }

    #[test]
    fn prft_unix_seconds_none_before_unix_epoch() {
        // NTP seconds < the 1970 offset → predates Unix epoch.
        let ntp = (1000u64) << 32;
        let p = parse_prft(&prft_body(0, 0, 1, ntp, 0)).unwrap();
        assert!(p.unix_seconds().is_none());
    }

    #[test]
    fn parse_producer_reference_times_walks_top_level_in_order() {
        // styp + two prft + a (dummy) moof at the top level: order preserved,
        // moof ignored, both prft collected.
        let mut file = Vec::new();
        file.extend_from_slice(&wrap_box(b"styp", &[0u8; 8]));
        file.extend_from_slice(&wrap_box(b"prft", &prft_body(0, 0, 1, 100 << 32, 10)));
        file.extend_from_slice(&wrap_box(b"prft", &prft_body(1, 0, 2, 200 << 32, 20)));
        file.extend_from_slice(&wrap_box(b"moof", &[0u8; 4]));
        let prfts = parse_producer_reference_times(&file);
        assert_eq!(prfts.len(), 2);
        assert_eq!(prfts[0].reference_track_id, 1);
        assert_eq!(prfts[0].ntp_seconds(), 100);
        assert_eq!(prfts[1].reference_track_id, 2);
        assert_eq!(prfts[1].version, 1);
    }

    #[test]
    fn parse_producer_reference_times_empty_when_absent() {
        let file = wrap_box(b"moov", &[0u8; 4]);
        assert!(parse_producer_reference_times(&file).is_empty());
    }

    #[test]
    fn parse_producer_reference_times_skips_malformed() {
        // A truncated prft body is skipped; the valid one still lands.
        let mut file = Vec::new();
        file.extend_from_slice(&wrap_box(b"prft", &prft_body(0, 0, 1, 100 << 32, 10)[..10]));
        file.extend_from_slice(&wrap_box(b"prft", &prft_body(0, 0, 9, 300 << 32, 30)));
        let prfts = parse_producer_reference_times(&file);
        assert_eq!(prfts.len(), 1);
        assert_eq!(prfts[0].reference_track_id, 9);
    }

    /// Build an `ssix` body (FullBox v0/flags0 prefix + payload) from a
    /// list of subsegments, each a slice of `(level, range_size)` pairs.
    /// `range_size` is emitted as 24-bit big-endian.
    fn ssix_body(subsegments: &[&[(u8, u32)]]) -> Vec<u8> {
        let mut body = vec![0u8, 0, 0, 0]; // version 0, flags 0
        body.extend_from_slice(&(subsegments.len() as u32).to_be_bytes());
        for ss in subsegments {
            body.extend_from_slice(&(ss.len() as u32).to_be_bytes());
            for &(level, range_size) in *ss {
                body.push(level);
                body.push((range_size >> 16) as u8);
                body.push((range_size >> 8) as u8);
                body.push(range_size as u8);
            }
        }
        body
    }

    #[test]
    fn parse_ssix_reads_levels_and_24bit_ranges() {
        let body = ssix_body(&[
            &[(1, 0x10_0000), (2, 0x20_0000)],
            &[(1, 100), (2, 200), (3, 300)],
        ]);
        let ssix = parse_ssix(&body).unwrap();
        assert_eq!(ssix.subsegments.len(), 2);
        assert_eq!(ssix.subsegments[0].len(), 2);
        assert_eq!(
            ssix.subsegments[0][0],
            SubsegmentRange {
                level: 1,
                range_size: 0x10_0000
            }
        );
        assert_eq!(
            ssix.subsegments[0][1],
            SubsegmentRange {
                level: 2,
                range_size: 0x20_0000
            }
        );
        assert_eq!(ssix.subsegments[1].len(), 3);
        assert_eq!(ssix.subsegments[1][2].level, 3);
        assert_eq!(ssix.subsegments[1][2].range_size, 300);
    }

    #[test]
    fn parse_ssix_max_24bit_range_size() {
        // range_size is unsigned int(24): top value is 0xFFFFFF.
        let body = ssix_body(&[&[(0, 0xFF_FFFF), (1, 0)]]);
        let ssix = parse_ssix(&body).unwrap();
        assert_eq!(ssix.subsegments[0][0].range_size, 0xFF_FFFF);
        assert_eq!(ssix.subsegments[0][1].range_size, 0);
    }

    #[test]
    fn parse_ssix_empty_subsegment_count() {
        let body = ssix_body(&[]);
        let ssix = parse_ssix(&body).unwrap();
        assert!(ssix.subsegments.is_empty());
    }

    #[test]
    fn parse_ssix_rejects_unknown_version() {
        let mut body = ssix_body(&[&[(1, 1), (2, 2)]]);
        body[0] = 1; // version 1
        assert!(parse_ssix(&body).is_err());
    }

    #[test]
    fn parse_ssix_rejects_truncated_range() {
        // Declares one subsegment with one range but cuts the last byte.
        let mut body = ssix_body(&[&[(1, 0x123456)]]);
        body.pop();
        assert!(parse_ssix(&body).is_err());
    }

    #[test]
    fn parse_ssix_rejects_truncated_range_count() {
        // subsegment_count = 1 but no range_count follows.
        let body = vec![0u8, 0, 0, 0, 0, 0, 0, 1];
        assert!(parse_ssix(&body).is_err());
    }

    #[test]
    fn parse_subsegment_indexes_walks_top_level_in_order() {
        // styp + sidx + two ssix + a (dummy) moof: order preserved,
        // non-ssix ignored, both ssix collected.
        let mut file = Vec::new();
        file.extend_from_slice(&wrap_box(b"styp", &[0u8; 8]));
        file.extend_from_slice(&wrap_box(b"sidx", &[0u8; 12]));
        file.extend_from_slice(&wrap_box(b"ssix", &ssix_body(&[&[(1, 10), (2, 20)]])));
        file.extend_from_slice(&wrap_box(b"ssix", &ssix_body(&[&[(1, 30), (2, 40)]])));
        file.extend_from_slice(&wrap_box(b"moof", &[0u8; 4]));
        let idx = parse_subsegment_indexes(&file);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[0].subsegments[0][0].range_size, 10);
        assert_eq!(idx[1].subsegments[0][1].range_size, 40);
    }

    #[test]
    fn parse_subsegment_indexes_empty_when_absent() {
        let file = wrap_box(b"moov", &[0u8; 4]);
        assert!(parse_subsegment_indexes(&file).is_empty());
    }

    #[test]
    fn parse_subsegment_indexes_skips_malformed() {
        // A truncated ssix is skipped; the valid one still lands.
        let truncated = {
            let mut b = ssix_body(&[&[(1, 0x123456)]]);
            b.pop();
            b
        };
        let mut file = Vec::new();
        file.extend_from_slice(&wrap_box(b"ssix", &truncated));
        file.extend_from_slice(&wrap_box(b"ssix", &ssix_body(&[&[(7, 77), (8, 88)]])));
        let idx = parse_subsegment_indexes(&file);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx[0].subsegments[0][0].level, 7);
    }
}
