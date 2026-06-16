//! Sample-to-group parsing for AVIS image-sequence tracks.
//!
//! The ISO/IEC 14496-12 sample-grouping family maps each sample in a
//! track to a *group description index* so that a sibling
//! [`SampleGroupDescriptionBox`] (`sgpd`) can attach per-group
//! characteristics. Two on-wire shapes carry the mapping:
//!
//! * **`sbgp`** — [`SampleToGroupBox`], ISO/IEC 14496-12:2015 §8.9.2.
//!   A run-length table: each entry is a `(sample_count,
//!   group_description_index)` pair covering a run of consecutive
//!   samples. v0 carries only `grouping_type`; v1 adds
//!   `grouping_type_parameter`.
//! * **`csgp`** — `CompactSampleToGroupBox`, ISO/IEC 14496-12:2020
//!   §8.9.5 (box layout staged in
//!   `docs/container/isobmff/post-2015-additions.md`). The compact
//!   form factors the per-sample indices into a small set of *patterns*
//!   that repeat across the track, with each field width selected by a
//!   2-bit code packed into the `FullBox.flags`.
//!
//! Both shapes resolve to the same logical result: an ordered list of
//! `(sample_count, group_description_index)` runs, which this module
//! exposes as [`SampleToGroup`]. The per-sample index for a given
//! 0-based sample number is recovered with
//! [`SampleToGroup::group_index_for_sample`].
//!
//! This is container-side metadata only: the module does not decode
//! AV1 OBUs and does not interpret what any particular `grouping_type`
//! *means* — that semantics lives in the `sgpd` entries (whose generic
//! `grouping_type` + `default_group_description_index` header is
//! surfaced by [`SampleGroupDescription`]).

use crate::box_parser::{b, iter_boxes, parse_full_box, read_u32, BoxType};
use crate::error::{AvifError as Error, Result};

const SBGP: BoxType = b(b"sbgp");
const CSGP: BoxType = b(b"csgp");
const SGPD: BoxType = b(b"sgpd");

/// Soft cap on the number of `(sample_count, index)` runs we expand
/// from a single `sbgp`/`csgp`. Adversarial files can declare
/// `entry_count`/`pattern_count == 0xFFFF_FFFF`; the cap bounds the
/// `Vec` without affecting any real AVIS track (which carries one run
/// per group boundary — typically a handful).
const MAX_RUNS: usize = 1 << 20;

/// One `(sample_count, group_description_index)` run from a
/// [`SampleToGroup`] table.
///
/// `group_description_index` follows ISO/IEC 14496-12:2015 §8.9.2.3:
/// an index in `1..=N` into the matching `sgpd`'s entries, or `0` to
/// indicate the run's samples are members of *no* group of this type.
///
/// For `csgp` carried inside a `traf`, the most-significant bit of the
/// index may flag a fragment-local vs global description
/// (post-2015-additions.md §"Fragment-local vs global indices").
/// [`SampleToGroupRun`] preserves the raw index verbatim; the
/// fragment-local bit is decoded on demand via
/// [`SampleToGroupRun::is_fragment_local`] /
/// [`SampleToGroupRun::description_index`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleToGroupRun {
    /// Number of consecutive samples covered by this run.
    pub sample_count: u32,
    /// Raw `group_description_index` as carried on the wire. For
    /// `sbgp` this is the field verbatim; for `csgp` it may carry a
    /// fragment-local flag in its msb — see
    /// [`Self::is_fragment_local`].
    pub group_description_index: u32,
}

impl SampleToGroupRun {
    /// `true` when this run's index has its most-significant bit set,
    /// which a `csgp` inside a `traf` uses to flag a *fragment-local*
    /// group description (defined in the same `traf`'s `sgpd`) rather
    /// than a global one (post-2015-additions.md). For an `sbgp` run,
    /// or any index with the msb clear, this is `false`.
    ///
    /// `bits` is the on-wire width of the index field for the box this
    /// run came from (`32` for `sbgp`; `4 << index_size_code` for
    /// `csgp`). The msb is bit `bits - 1`.
    pub fn is_fragment_local(&self, bits: u32) -> bool {
        if bits == 0 || bits > 32 {
            return false;
        }
        let msb = 1u32 << (bits - 1);
        self.group_description_index & msb != 0
    }

    /// The group description index with any fragment-local msb (for the
    /// supplied field `bits`) masked off, i.e. the actual index into
    /// the `sgpd` entry list. Returns the raw value unchanged when the
    /// msb is clear or `bits` is out of range.
    pub fn description_index(&self, bits: u32) -> u32 {
        if bits == 0 || bits > 32 {
            return self.group_description_index;
        }
        let msb = 1u32 << (bits - 1);
        self.group_description_index & !msb
    }
}

/// Which on-wire box a [`SampleToGroup`] was decoded from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleToGroupKind {
    /// ISO/IEC 14496-12:2015 §8.9.2 `sbgp` (run-length table).
    Sbgp,
    /// ISO/IEC 14496-12:2020 §8.9.5 `csgp` (compact pattern form).
    Csgp,
}

/// A decoded sample-to-group mapping — the normalised result of either
/// an `sbgp` or a `csgp` box.
///
/// Both shapes resolve to an ordered list of
/// [`SampleToGroupRun`]s covering the samples in declaration order. The
/// per-sample group index is recovered with
/// [`Self::group_index_for_sample`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleToGroup {
    /// Which box produced this mapping.
    pub kind: SampleToGroupKind,
    /// `grouping_type` four-CC linking this mapping to its sibling
    /// `sgpd` (ISO/IEC 14496-12:2015 §8.9.2.3).
    pub grouping_type: BoxType,
    /// `grouping_type_parameter`, present for `sbgp` version 1 and for
    /// `csgp` when its `grouping_type_parameter_present` flag bit is
    /// set. `None` otherwise.
    pub grouping_type_parameter: Option<u32>,
    /// On-wire width in bits of each `group_description_index` field —
    /// `32` for `sbgp`, `4 << index_size_code` for `csgp`. Callers
    /// pass this to [`SampleToGroupRun::is_fragment_local`] /
    /// [`SampleToGroupRun::description_index`].
    pub index_bits: u32,
    /// The ordered `(sample_count, index)` runs.
    pub runs: Vec<SampleToGroupRun>,
}

impl SampleToGroup {
    /// Total number of samples covered by every run, saturating on
    /// overflow. Per §8.9.2.3 this may be less than the track's total
    /// sample count (uncovered samples fall to the `sgpd` default group
    /// or to no group).
    pub fn covered_sample_count(&self) -> u64 {
        self.runs
            .iter()
            .map(|r| u64::from(r.sample_count))
            .fold(0u64, u64::saturating_add)
    }

    /// Resolve the *raw* `group_description_index` for a 0-based
    /// `sample` number by walking the runs in order. Returns `None`
    /// when `sample` falls beyond the last covered sample — per
    /// §8.9.2.3 such a sample has no explicit association and the
    /// caller should fall back to the `sgpd` default group.
    ///
    /// The returned value is the raw index (it may carry a `csgp`
    /// fragment-local msb); mask it with
    /// [`SampleToGroupRun::description_index`] using [`Self::index_bits`]
    /// for the real `sgpd` index.
    pub fn group_index_for_sample(&self, sample: u32) -> Option<u32> {
        let mut base: u64 = 0;
        for run in &self.runs {
            let next = base.saturating_add(u64::from(run.sample_count));
            if u64::from(sample) < next {
                return Some(run.group_description_index);
            }
            base = next;
        }
        None
    }
}

/// Generic header of a `SampleGroupDescriptionBox` (`sgpd`), ISO/IEC
/// 14496-12:2015 §8.9.3.
///
/// The per-entry descriptive payloads are `grouping_type`-specific and
/// not interpreted here (their meaning is defined by whatever codec or
/// extension owns the grouping type). What *is* generic — and what a
/// sample-to-group resolver needs — is the box's `grouping_type`, its
/// `default_length` (v1), and its `default_group_description_index`
/// (v2): the latter is the index assigned to any sample not explicitly
/// covered by an `sbgp`/`csgp` run (§8.9.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleGroupDescription {
    /// FullBox version (0, 1, or 2 in the staged editions).
    pub version: u8,
    /// `grouping_type` four-CC — must match a sibling `sbgp`/`csgp`'s
    /// `grouping_type` for the mapping to apply (§8.9.3.3).
    pub grouping_type: BoxType,
    /// `default_length` from v1 (the fixed byte length of each entry
    /// when non-zero; `0` means each entry carries its own
    /// `description_length`). `None` for v0 (no such field).
    pub default_length: Option<u32>,
    /// `default_group_description_index` from v2 — the group index for
    /// samples not covered by any `sbgp`/`csgp` run (§8.9.3.3). `None`
    /// for v0/v1 (the default is then "no group").
    pub default_group_description_index: Option<u32>,
    /// `entry_count` declared by the box.
    pub entry_count: u32,
}

/// Decode an `sbgp` (`SampleToGroupBox`) payload — the FullBox body
/// after the version/flags header.
///
/// ISO/IEC 14496-12:2015 §8.9.2.2:
///
/// ```text
/// unsigned int(32) grouping_type;
/// if (version == 1) { unsigned int(32) grouping_type_parameter; }
/// unsigned int(32) entry_count;
/// for (i = 1; i <= entry_count; i++) {
///     unsigned int(32) sample_count;
///     unsigned int(32) group_description_index;
/// }
/// ```
///
/// A truncated entry table bounds the recognised runs (every
/// well-formed run up to the truncation point is returned); a
/// malformed FullBox header or a header too short to hold
/// `grouping_type` + `entry_count` returns `Err(InvalidData)`.
pub fn parse_sbgp(payload: &[u8]) -> Result<SampleToGroup> {
    let (version, _flags, body) = parse_full_box(payload)?;
    let mut cursor = 0usize;
    let grouping_type = read_fourcc(body, cursor)
        .ok_or_else(|| Error::InvalidData("sbgp: truncated grouping_type".to_string()))?;
    cursor += 4;
    let grouping_type_parameter = if version == 1 {
        let p = read_u32(body, cursor).map_err(|_| {
            Error::InvalidData("sbgp: truncated grouping_type_parameter".to_string())
        })?;
        cursor += 4;
        Some(p)
    } else {
        None
    };
    let entry_count = read_u32(body, cursor)
        .map_err(|_| Error::InvalidData("sbgp: truncated entry_count".to_string()))?
        as usize;
    cursor += 4;
    let mut runs = Vec::with_capacity(entry_count.min(64));
    for _ in 0..entry_count.min(MAX_RUNS) {
        if cursor + 8 > body.len() {
            break;
        }
        let sample_count = read_u32(body, cursor)?;
        let group_description_index = read_u32(body, cursor + 4)?;
        cursor += 8;
        runs.push(SampleToGroupRun {
            sample_count,
            group_description_index,
        });
    }
    Ok(SampleToGroup {
        kind: SampleToGroupKind::Sbgp,
        grouping_type,
        grouping_type_parameter,
        index_bits: 32,
        runs,
    })
}

/// Decode a `csgp` (`CompactSampleToGroupBox`) payload.
///
/// Box layout per `docs/container/isobmff/post-2015-additions.md`
/// (ISO/IEC 14496-12:2020 §8.9.5). The `FullBox.flags` field encodes
/// four sub-fields:
///
/// | Field | flags bits | meaning |
/// |-------|-----------:|---------|
/// | `index_size_code` | `[0..1]` | width selector of each index |
/// | `count_size_code` | `[2..3]` | width selector of `sample_count` |
/// | `pattern_size_code` | `[4..5]` | width selector of `pattern_length` |
/// | `grouping_type_parameter_present` | `[6]` | optional param present |
///
/// Each 2-bit width code maps to `4 << code` bits (4/8/16/32). The
/// body is then:
///
/// ```text
/// unsigned int(32) grouping_type;
/// if (grouping_type_parameter_present) unsigned int(32) grouping_type_parameter;
/// unsigned int(32) pattern_count;
/// for (i = 1..=pattern_count) {
///     unsigned int(f(pattern_size_code)) pattern_length[i];
///     unsigned int(f(count_size_code))   sample_count[i];
/// }
/// for (j = 1..=pattern_count)
///     for (k = 1..=pattern_length[j])
///         unsigned int(f(index_size_code)) sample_group_description_index[j][k];
/// ```
///
/// Each pattern `j` is expanded into the run-length form: it contributes
/// `pattern_length[j]` runs, where run `k` covers `sample_count[j] /
/// pattern_length[j]` samples carrying index `[j][k]` — replicated so
/// the pattern repeats across the `sample_count[j]` samples it governs.
/// (Equivalently: the pattern of `pattern_length[j]` indices repeats to
/// fill `sample_count[j]` samples.) A truncated body bounds the decode.
pub fn parse_csgp(payload: &[u8]) -> Result<SampleToGroup> {
    let (version, flags, body) = parse_full_box(payload)?;
    let _ = version; // csgp is version 0 in the staged catalogue.
    let index_size_code = flags & 0b11;
    let count_size_code = (flags >> 2) & 0b11;
    let pattern_size_code = (flags >> 4) & 0b11;
    let grouping_type_parameter_present = (flags >> 6) & 0b1 == 1;
    let index_bits = 4u32 << index_size_code;
    let count_bits = 4u32 << count_size_code;
    let pattern_bits = 4u32 << pattern_size_code;

    let mut reader = BitReader::new(body);
    let grouping_type = {
        let v = reader
            .read(32)
            .ok_or_else(|| Error::InvalidData("csgp: truncated grouping_type".to_string()))?;
        (v as u32).to_be_bytes()
    };
    let grouping_type_parameter = if grouping_type_parameter_present {
        Some(reader.read(32).ok_or_else(|| {
            Error::InvalidData("csgp: truncated grouping_type_parameter".to_string())
        })? as u32)
    } else {
        None
    };
    let pattern_count = reader
        .read(32)
        .ok_or_else(|| Error::InvalidData("csgp: truncated pattern_count".to_string()))?
        as usize;
    let pattern_count = pattern_count.min(MAX_RUNS);

    // First loop: (pattern_length, sample_count) per pattern.
    let mut patterns: Vec<(u32, u32)> = Vec::with_capacity(pattern_count.min(64));
    for _ in 0..pattern_count {
        let Some(pattern_length) = reader.read(pattern_bits) else {
            break;
        };
        let Some(sample_count) = reader.read(count_bits) else {
            break;
        };
        patterns.push((pattern_length as u32, sample_count as u32));
    }

    // Second loop: indices, then expand each pattern into runs.
    let mut runs: Vec<SampleToGroupRun> = Vec::new();
    'patterns: for (pattern_length, sample_count) in patterns {
        // Each pattern of `pattern_length` indices repeats to fill
        // `sample_count` samples. Distribute the samples across the
        // pattern positions: position k covers ceil/floor of
        // sample_count / pattern_length samples. The canonical reading
        // (post-2015-additions.md, mirrored from sbgp run semantics)
        // gives the first `sample_count % pattern_length` positions one
        // extra sample so the run lengths sum exactly to `sample_count`.
        let plen = pattern_length;
        if plen == 0 {
            continue;
        }
        let mut indices = Vec::with_capacity((plen as usize).min(256));
        for _ in 0..plen {
            let Some(idx) = reader.read(index_bits) else {
                break 'patterns;
            };
            indices.push(idx as u32);
        }
        if indices.len() < plen as usize {
            break;
        }
        let base = sample_count / plen;
        let rem = sample_count % plen;
        if runs.len() + plen as usize > MAX_RUNS {
            break;
        }
        for (k, &idx) in indices.iter().enumerate() {
            let extra = if (k as u32) < rem { 1 } else { 0 };
            let run_samples = base + extra;
            runs.push(SampleToGroupRun {
                sample_count: run_samples,
                group_description_index: idx,
            });
        }
    }

    Ok(SampleToGroup {
        kind: SampleToGroupKind::Csgp,
        grouping_type,
        grouping_type_parameter,
        index_bits,
        runs,
    })
}

/// Decode the generic header of an `sgpd`
/// (`SampleGroupDescriptionBox`) payload, ISO/IEC 14496-12:2015
/// §8.9.3.2. The per-entry descriptive payloads are `grouping_type`-
/// specific and skipped; only the version, grouping type, `v1`
/// `default_length`, `v2` `default_group_description_index`, and
/// `entry_count` are surfaced.
///
/// ```text
/// aligned(8) class SampleGroupDescriptionBox(unsigned int(32) handler_type)
///   extends FullBox('sgpd', version, 0) {
///   unsigned int(32) grouping_type;
///   if (version == 1) unsigned int(32) default_length;
///   if (version >= 2) unsigned int(32) default_group_description_index;
///   unsigned int(32) entry_count;
///   // ... per-entry payloads ...
/// }
/// ```
pub fn parse_sgpd(payload: &[u8]) -> Result<SampleGroupDescription> {
    let (version, _flags, body) = parse_full_box(payload)?;
    let mut cursor = 0usize;
    let grouping_type = read_fourcc(body, cursor)
        .ok_or_else(|| Error::InvalidData("sgpd: truncated grouping_type".to_string()))?;
    cursor += 4;
    let default_length = if version == 1 {
        let v = read_u32(body, cursor)
            .map_err(|_| Error::InvalidData("sgpd: truncated default_length".to_string()))?;
        cursor += 4;
        Some(v)
    } else {
        None
    };
    let default_group_description_index = if version >= 2 {
        let v = read_u32(body, cursor).map_err(|_| {
            Error::InvalidData("sgpd: truncated default_group_description_index".to_string())
        })?;
        cursor += 4;
        Some(v)
    } else {
        None
    };
    let entry_count = read_u32(body, cursor)
        .map_err(|_| Error::InvalidData("sgpd: truncated entry_count".to_string()))?;
    Ok(SampleGroupDescription {
        version,
        grouping_type,
        default_length,
        default_group_description_index,
        entry_count,
    })
}

/// Walk an `stbl` payload and decode every sample-to-group box
/// (`sbgp` and `csgp`) it carries, in declaration order. A track may
/// carry one mapping per `grouping_type` (§8.9.2.1). Boxes whose body
/// fails to parse are skipped rather than aborting the walk.
pub fn parse_sample_to_groups(stbl: &[u8]) -> Vec<SampleToGroup> {
    let mut out = Vec::new();
    for hdr in iter_boxes(stbl).flatten() {
        let p = &stbl[hdr.payload_start..hdr.end()];
        let parsed = if hdr.box_type == SBGP {
            parse_sbgp(p)
        } else if hdr.box_type == CSGP {
            parse_csgp(p)
        } else {
            continue;
        };
        if let Ok(stg) = parsed {
            out.push(stg);
        }
    }
    out
}

/// Walk an `stbl` payload and decode every `sgpd`
/// (`SampleGroupDescriptionBox`) header it carries, in declaration
/// order. Skips boxes whose header fails to parse.
pub fn parse_sample_group_descriptions(stbl: &[u8]) -> Vec<SampleGroupDescription> {
    let mut out = Vec::new();
    for hdr in iter_boxes(stbl).flatten() {
        if hdr.box_type != SGPD {
            continue;
        }
        let p = &stbl[hdr.payload_start..hdr.end()];
        if let Ok(sgpd) = parse_sgpd(p) {
            out.push(sgpd);
        }
    }
    out
}

/// Read a four-CC at `at` from `buf`, or `None` when out of range.
fn read_fourcc(buf: &[u8], at: usize) -> Option<BoxType> {
    if at + 4 > buf.len() {
        return None;
    }
    Some([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

/// Minimal big-endian, MSB-first bit reader for the `csgp` sub-byte
/// field widths (4 / 8 / 16 / 32 bits). The `csgp` syntax packs fields
/// of `4 << code` bits consecutively without byte alignment between
/// them, so a bit-granular reader is required for the 4-bit case.
struct BitReader<'a> {
    buf: &'a [u8],
    /// Absolute bit position from the start of `buf` (MSB-first).
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        BitReader { buf, bit_pos: 0 }
    }

    /// Read `n` bits (1..=32) MSB-first as a big-endian unsigned value.
    /// Returns `None` when fewer than `n` bits remain.
    fn read(&mut self, n: u32) -> Option<u64> {
        if n == 0 {
            return Some(0);
        }
        let total_bits = self.buf.len().checked_mul(8)?;
        if self.bit_pos.checked_add(n as usize)? > total_bits {
            return None;
        }
        let mut value: u64 = 0;
        for _ in 0..n {
            let byte = self.buf[self.bit_pos / 8];
            let bit = (byte >> (7 - (self.bit_pos % 8))) & 1;
            value = (value << 1) | u64::from(bit);
            self.bit_pos += 1;
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_box(version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut out = vec![
            version,
            (flags >> 16) as u8,
            (flags >> 8) as u8,
            flags as u8,
        ];
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

    // ---- sbgp ----

    #[test]
    fn sbgp_v0_runs_and_per_sample_lookup() {
        // grouping_type 'roll', 2 entries: (3 samples -> idx 1),
        // (2 samples -> idx 2).
        let mut body = Vec::new();
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&2u32.to_be_bytes()); // entry_count
        body.extend_from_slice(&3u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&1u32.to_be_bytes()); // index
        body.extend_from_slice(&2u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&2u32.to_be_bytes()); // index
        let payload = full_box(0, 0, &body);

        let stg = parse_sbgp(&payload).unwrap();
        assert_eq!(stg.kind, SampleToGroupKind::Sbgp);
        assert_eq!(&stg.grouping_type, b"roll");
        assert_eq!(stg.grouping_type_parameter, None);
        assert_eq!(stg.index_bits, 32);
        assert_eq!(stg.runs.len(), 2);
        assert_eq!(stg.covered_sample_count(), 5);
        // samples 0,1,2 -> idx 1; samples 3,4 -> idx 2; sample 5 -> None
        assert_eq!(stg.group_index_for_sample(0), Some(1));
        assert_eq!(stg.group_index_for_sample(2), Some(1));
        assert_eq!(stg.group_index_for_sample(3), Some(2));
        assert_eq!(stg.group_index_for_sample(4), Some(2));
        assert_eq!(stg.group_index_for_sample(5), None);
    }

    #[test]
    fn sbgp_v1_carries_grouping_type_parameter() {
        let mut body = Vec::new();
        body.extend_from_slice(b"sync");
        body.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // grouping_type_parameter
        body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        body.extend_from_slice(&4u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&0u32.to_be_bytes()); // index 0 = no group
        let payload = full_box(1, 0, &body);
        let stg = parse_sbgp(&payload).unwrap();
        assert_eq!(stg.grouping_type_parameter, Some(0xDEAD_BEEF));
        assert_eq!(stg.runs.len(), 1);
        assert_eq!(stg.group_index_for_sample(0), Some(0));
    }

    #[test]
    fn sbgp_truncated_entries_bounded() {
        // entry_count says 4 but only one full entry follows.
        let mut body = Vec::new();
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&4u32.to_be_bytes());
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&7u32.to_be_bytes()); // dangling half-entry
        let payload = full_box(0, 0, &body);
        let stg = parse_sbgp(&payload).unwrap();
        assert_eq!(stg.runs.len(), 1);
    }

    #[test]
    fn sbgp_too_short_header_errors() {
        let payload = full_box(0, 0, b"ro"); // not even a four-CC
        assert!(parse_sbgp(&payload).is_err());
    }

    // ---- csgp ----

    #[test]
    fn csgp_all_8bit_single_pattern() {
        // codes: index=1 (8b), count=1 (8b), pattern=1 (8b),
        // grouping_type_parameter_present = 0.
        // flags = index(0..1)=01 | count(2..3)=01<<2 | pattern(4..5)=01<<4
        let flags = 0b01 | (0b01 << 2) | (0b01 << 4);
        let mut body = Vec::new();
        body.extend_from_slice(b"roll"); // grouping_type (32, byte-aligned)
        body.extend_from_slice(&1u32.to_be_bytes()); // pattern_count (32)
                                                     // pattern 0: pattern_length=2 (8b), sample_count=5 (8b)
        body.push(2);
        body.push(5);
        // indices for pattern 0: [3, 4] (8b each)
        body.push(3);
        body.push(4);
        let payload = full_box(0, flags, &body);

        let stg = parse_csgp(&payload).unwrap();
        assert_eq!(stg.kind, SampleToGroupKind::Csgp);
        assert_eq!(&stg.grouping_type, b"roll");
        assert_eq!(stg.index_bits, 8);
        // pattern_length=2 over 5 samples: base=2, rem=1 -> runs [3 (3 samples), 4 (2 samples)]
        assert_eq!(stg.runs.len(), 2);
        assert_eq!(stg.runs[0].sample_count, 3);
        assert_eq!(stg.runs[0].group_description_index, 3);
        assert_eq!(stg.runs[1].sample_count, 2);
        assert_eq!(stg.runs[1].group_description_index, 4);
        assert_eq!(stg.covered_sample_count(), 5);
        assert_eq!(stg.group_index_for_sample(0), Some(3));
        assert_eq!(stg.group_index_for_sample(2), Some(3));
        assert_eq!(stg.group_index_for_sample(3), Some(4));
        assert_eq!(stg.group_index_for_sample(4), Some(4));
        assert_eq!(stg.group_index_for_sample(5), None);
    }

    #[test]
    fn csgp_4bit_indices_packed() {
        // index_size_code = 0 -> 4-bit indices; count/pattern = 8-bit.
        let flags = (0b01 << 2) | (0b01 << 4);
        let mut body = Vec::new();
        body.extend_from_slice(b"abcd");
        body.extend_from_slice(&1u32.to_be_bytes()); // pattern_count
        body.push(3); // pattern_length = 3 (8b)
        body.push(3); // sample_count = 3 (8b)
                      // three 4-bit indices: 1, 2, 15 -> bytes 0x12, 0xF0 (last nibble padding)
        body.push(0x12);
        body.push(0xF0);
        let payload = full_box(0, flags, &body);
        let stg = parse_csgp(&payload).unwrap();
        assert_eq!(stg.index_bits, 4);
        assert_eq!(stg.runs.len(), 3);
        assert_eq!(stg.runs[0].group_description_index, 1);
        assert_eq!(stg.runs[1].group_description_index, 2);
        assert_eq!(stg.runs[2].group_description_index, 15);
        // base=1, rem=0 -> each run is 1 sample.
        assert_eq!(stg.runs.iter().map(|r| r.sample_count).sum::<u32>(), 3);
        assert_eq!(stg.group_index_for_sample(0), Some(1));
        assert_eq!(stg.group_index_for_sample(1), Some(2));
        assert_eq!(stg.group_index_for_sample(2), Some(15));
    }

    #[test]
    fn csgp_grouping_type_parameter_present() {
        let flags = (0b01) | (0b01 << 2) | (0b01 << 4) | (1 << 6);
        let mut body = Vec::new();
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&0x1234_5678u32.to_be_bytes()); // grouping_type_parameter
        body.extend_from_slice(&1u32.to_be_bytes()); // pattern_count
        body.push(1); // pattern_length
        body.push(2); // sample_count
        body.push(7); // single index
        let payload = full_box(0, flags, &body);
        let stg = parse_csgp(&payload).unwrap();
        assert_eq!(stg.grouping_type_parameter, Some(0x1234_5678));
        assert_eq!(stg.runs.len(), 1);
        assert_eq!(stg.runs[0].sample_count, 2);
        assert_eq!(stg.runs[0].group_description_index, 7);
    }

    #[test]
    fn csgp_fragment_local_msb() {
        // 8-bit index with msb set -> fragment-local at bits=8.
        let run = SampleToGroupRun {
            sample_count: 1,
            group_description_index: 0x83, // msb of 8-bit set, low = 3
        };
        assert!(run.is_fragment_local(8));
        assert_eq!(run.description_index(8), 3);
        // at 32-bit width the same value's msb is clear.
        assert!(!run.is_fragment_local(32));
        assert_eq!(run.description_index(32), 0x83);
    }

    // ---- sgpd ----

    #[test]
    fn sgpd_v2_default_index() {
        let mut body = Vec::new();
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&2u32.to_be_bytes()); // default_group_description_index
        body.extend_from_slice(&0u32.to_be_bytes()); // entry_count
        let payload = full_box(2, 0, &body);
        let sgpd = parse_sgpd(&payload).unwrap();
        assert_eq!(sgpd.version, 2);
        assert_eq!(&sgpd.grouping_type, b"roll");
        assert_eq!(sgpd.default_group_description_index, Some(2));
        assert_eq!(sgpd.default_length, None);
        assert_eq!(sgpd.entry_count, 0);
    }

    #[test]
    fn sgpd_v1_default_length() {
        let mut body = Vec::new();
        body.extend_from_slice(b"sync");
        body.extend_from_slice(&8u32.to_be_bytes()); // default_length
        body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        let payload = full_box(1, 0, &body);
        let sgpd = parse_sgpd(&payload).unwrap();
        assert_eq!(sgpd.default_length, Some(8));
        assert_eq!(sgpd.default_group_description_index, None);
        assert_eq!(sgpd.entry_count, 1);
    }

    // ---- stbl walkers ----

    #[test]
    fn walk_stbl_collects_sbgp_and_sgpd() {
        let mut sbgp_body = Vec::new();
        sbgp_body.extend_from_slice(b"roll");
        sbgp_body.extend_from_slice(&1u32.to_be_bytes());
        sbgp_body.extend_from_slice(&3u32.to_be_bytes());
        sbgp_body.extend_from_slice(&1u32.to_be_bytes());
        let mut sgpd_body = Vec::new();
        sgpd_body.extend_from_slice(b"roll");
        sgpd_body.extend_from_slice(&0u32.to_be_bytes()); // entry_count (v0)

        let mut stbl = Vec::new();
        stbl.extend_from_slice(&wrap(b"sbgp", &full_box(0, 0, &sbgp_body)));
        stbl.extend_from_slice(&wrap(b"sgpd", &full_box(0, 0, &sgpd_body)));

        let stgs = parse_sample_to_groups(&stbl);
        assert_eq!(stgs.len(), 1);
        assert_eq!(&stgs[0].grouping_type, b"roll");
        let sgpds = parse_sample_group_descriptions(&stbl);
        assert_eq!(sgpds.len(), 1);
        assert_eq!(&sgpds[0].grouping_type, b"roll");
    }

    #[test]
    fn walk_stbl_collects_csgp() {
        let flags = 0b01 | (0b01 << 2) | (0b01 << 4);
        let mut csgp_body = Vec::new();
        csgp_body.extend_from_slice(b"roll");
        csgp_body.extend_from_slice(&1u32.to_be_bytes());
        csgp_body.push(1);
        csgp_body.push(2);
        csgp_body.push(5);
        let mut stbl = Vec::new();
        stbl.extend_from_slice(&wrap(b"csgp", &full_box(0, flags, &csgp_body)));
        let stgs = parse_sample_to_groups(&stbl);
        assert_eq!(stgs.len(), 1);
        assert_eq!(stgs[0].kind, SampleToGroupKind::Csgp);
        assert_eq!(stgs[0].runs[0].group_description_index, 5);
    }

    #[test]
    fn bit_reader_crosses_byte_boundary() {
        // bytes 0b1010_0110, 0b1100_0000
        let buf = [0b1010_0110u8, 0b1100_0000u8];
        let mut r = BitReader::new(&buf);
        assert_eq!(r.read(4), Some(0b1010));
        assert_eq!(r.read(4), Some(0b0110));
        assert_eq!(r.read(2), Some(0b11));
        assert_eq!(r.read(8), None); // only 6 bits left
    }
}
