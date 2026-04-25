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
//! [`oxideav_av1::Av1Decoder`] to decode frames end-to-end.

use oxideav_core::{Error, Result};

use crate::box_parser::{b, find_box, iter_boxes, parse_full_box, read_u32, read_u64, BoxType};

const MOOV: BoxType = b(b"moov");
const MVHD: BoxType = b(b"mvhd");
const TRAK: BoxType = b(b"trak");
const TKHD: BoxType = b(b"tkhd");
const MDIA: BoxType = b(b"mdia");
const MINF: BoxType = b(b"minf");
const STBL: BoxType = b(b"stbl");
const STTS: BoxType = b(b"stts");
const STSC: BoxType = b(b"stsc");
const STSZ: BoxType = b(b"stsz");
const STCO: BoxType = b(b"stco");
const CO64: BoxType = b(b"co64");
const STSS: BoxType = b(b"stss");

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

    // Locate the first track's stbl — AVIS carries a single image track.
    let stbl = find_first_track_stbl(moov_payload)
        .ok_or_else(|| Error::InvalidData("avis: missing trak/mdia/minf/stbl".to_string()))?;
    let samples = sample_table(stbl)?;
    Ok(AvisMeta {
        timescale,
        display_dims,
        samples,
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
}
