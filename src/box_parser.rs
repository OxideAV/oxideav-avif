//! Generic ISO Base Media File Format box-header walker, just enough to
//! reach the AVIF meta hierarchy. Spec: ISO/IEC 14496-12 §4.2 (box
//! structure), §4.2.2 (FullBox).
//!
//! A box is `size(4) + type(4)` optionally followed by `largesize(8)`
//! when `size == 1` or the box extending to end-of-file when `size == 0`.
//! `FullBox` additionally prefixes its payload with `version(1) +
//! flags(3)`.

use crate::error::{AvifError as Error, Result};

/// 4-character box type, compared as a big-endian `u32`.
pub type BoxType = [u8; 4];

/// Convert a 4-byte ASCII literal to a `BoxType` at compile time.
pub const fn b(s: &[u8; 4]) -> BoxType {
    *s
}

/// Readable rendering for error messages.
pub fn type_str(t: &BoxType) -> String {
    String::from_utf8_lossy(t).into_owned()
}

/// One parsed box header + the payload range inside the parent buffer.
#[derive(Clone, Debug)]
pub struct BoxHeader {
    pub box_type: BoxType,
    /// Offset of the box payload (right after size/type/largesize) in the
    /// parent slice.
    pub payload_start: usize,
    /// Length of the payload (excluding size/type/largesize bytes).
    pub payload_len: usize,
    /// Full byte span occupied by the box (header + payload) in the
    /// parent slice.
    pub total_len: usize,
}

impl BoxHeader {
    pub fn end(&self) -> usize {
        self.payload_start + self.payload_len
    }
}

/// Iterate the boxes packed contiguously inside `buf`, starting from
/// `start`. Stops cleanly at end of buffer; surfaces an error on any
/// truncated size field.
pub fn iter_boxes(buf: &[u8]) -> BoxIter<'_> {
    BoxIter { buf, cursor: 0 }
}

pub struct BoxIter<'a> {
    buf: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = Result<BoxHeader>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.buf.len() {
            return None;
        }
        match parse_box_header(self.buf, self.cursor) {
            Ok(h) => {
                self.cursor = h
                    .payload_start
                    .checked_add(h.payload_len)
                    .unwrap_or(self.buf.len());
                Some(Ok(h))
            }
            Err(e) => {
                // Force iteration to stop on the next call.
                self.cursor = self.buf.len();
                Some(Err(e))
            }
        }
    }
}

/// Parse a single box header at `start`.
pub fn parse_box_header(buf: &[u8], start: usize) -> Result<BoxHeader> {
    // Defensive arithmetic: `start` is caller-supplied so overflow checks
    // come first. Subsequent slice indexing relies on these bounds.
    let header_end = start
        .checked_add(8)
        .ok_or_else(|| Error::invalid("avif: box header offset overflow"))?;
    if header_end > buf.len() {
        return Err(Error::invalid("avif: truncated box header"));
    }
    let size = read_u32(buf, start)?;
    let mut box_type = [0u8; 4];
    box_type.copy_from_slice(&buf[start + 4..start + 8]);
    let remaining = buf.len() - start;
    let (payload_start, total_len) = if size == 1 {
        let largesize_end = start
            .checked_add(16)
            .ok_or_else(|| Error::invalid("avif: largesize header offset overflow"))?;
        if largesize_end > buf.len() {
            return Err(Error::invalid("avif: truncated largesize box"));
        }
        let ls = read_u64(buf, start + 8)?;
        if ls < 16 || ls > remaining as u64 {
            return Err(Error::invalid(format!(
                "avif: box '{}' largesize {ls} out of range",
                type_str(&box_type)
            )));
        }
        (start + 16, ls as usize)
    } else if size == 0 {
        // Box extends to end of file.
        (start + 8, remaining)
    } else {
        let s = size as usize;
        if s < 8 || s > remaining {
            return Err(Error::invalid(format!(
                "avif: box '{}' size {s} out of range",
                type_str(&box_type)
            )));
        }
        (start + 8, s)
    };
    let payload_len = total_len
        .checked_sub(payload_start - start)
        .ok_or_else(|| {
            Error::invalid(format!(
                "avif: box '{}' header longer than total",
                type_str(&box_type)
            ))
        })?;
    Ok(BoxHeader {
        box_type,
        payload_start,
        payload_len,
        total_len,
    })
}

/// Parse a FullBox prefix `version(1) + flags(3)` and return
/// `(version, flags, remaining)`.
pub fn parse_full_box(payload: &[u8]) -> Result<(u8, u32, &[u8])> {
    if payload.len() < 4 {
        return Err(Error::invalid("avif: truncated FullBox"));
    }
    let version = payload[0];
    let flags = ((payload[1] as u32) << 16) | ((payload[2] as u32) << 8) | (payload[3] as u32);
    Ok((version, flags, &payload[4..]))
}

/// Find the first box whose type matches `target` among the contiguous
/// boxes in `buf`. Returns `Ok(None)` when not present, `Err` on a
/// parse failure.
pub fn find_box<'a>(buf: &'a [u8], target: &BoxType) -> Result<Option<(&'a [u8], BoxHeader)>> {
    for h in iter_boxes(buf) {
        let h = h?;
        if &h.box_type == target {
            let payload = &buf[h.payload_start..h.end()];
            return Ok(Some((payload, h)));
        }
    }
    Ok(None)
}

// Internal byte-cursor plumbing behind the box walker — not part of the
// crate's stable API surface (the walker itself — `iter_boxes` / `find_box`
// / `parse_box_header` / `parse_full_box` — stays public per the lib.rs
// standalone-surface contract).
#[doc(hidden)]
pub fn read_u16(buf: &[u8], at: usize) -> Result<u16> {
    let end = at
        .checked_add(2)
        .ok_or_else(|| Error::invalid("avif: u16 offset overflow"))?;
    if end > buf.len() {
        return Err(Error::invalid("avif: truncated u16 read"));
    }
    Ok(u16::from_be_bytes([buf[at], buf[at + 1]]))
}

// Internal byte-cursor plumbing — see `read_u16`.
#[doc(hidden)]
pub fn read_u32(buf: &[u8], at: usize) -> Result<u32> {
    let end = at
        .checked_add(4)
        .ok_or_else(|| Error::invalid("avif: u32 offset overflow"))?;
    if end > buf.len() {
        return Err(Error::invalid("avif: truncated u32 read"));
    }
    Ok(u32::from_be_bytes([
        buf[at],
        buf[at + 1],
        buf[at + 2],
        buf[at + 3],
    ]))
}

// Internal byte-cursor plumbing — see `read_u16`.
#[doc(hidden)]
pub fn read_u64(buf: &[u8], at: usize) -> Result<u64> {
    let end = at
        .checked_add(8)
        .ok_or_else(|| Error::invalid("avif: u64 offset overflow"))?;
    if end > buf.len() {
        return Err(Error::invalid("avif: truncated u64 read"));
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[at..at + 8]);
    Ok(u64::from_be_bytes(b))
}

/// Read a variable-width big-endian unsigned integer of `width_bytes`
/// bytes starting at `at`. `width_bytes` may be 0, 4, or 8 per
/// ISO/IEC 14496-12 §8.11.3 (`iloc`).
// Internal byte-cursor plumbing — see `read_u16`.
#[doc(hidden)]
pub fn read_var_uint(buf: &[u8], at: usize, width_bytes: usize) -> Result<u64> {
    match width_bytes {
        0 => Ok(0),
        4 => read_u32(buf, at).map(|v| v as u64),
        8 => read_u64(buf, at),
        _ => Err(Error::invalid(format!(
            "avif: unsupported iloc field width {width_bytes}"
        ))),
    }
}

/// Null-terminated ASCII string starting at `at`, advancing the caller's
/// cursor past the terminator. Returns `(string, new_offset)`.
// Internal byte-cursor plumbing — see `read_u16`.
#[doc(hidden)]
pub fn read_cstr(buf: &[u8], at: usize) -> Result<(String, usize)> {
    let mut i = at;
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    if i >= buf.len() {
        return Err(Error::invalid("avif: unterminated C string"));
    }
    let s = String::from_utf8_lossy(&buf[at..i]).into_owned();
    Ok((s, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_ftyp_then_meta() {
        // ftyp(32) + meta(8, empty) trailing 0-size box.
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x20u32.to_be_bytes());
        buf.extend_from_slice(b"ftyp");
        buf.extend_from_slice(&[0u8; 0x18]);
        buf.extend_from_slice(&0x08u32.to_be_bytes());
        buf.extend_from_slice(b"meta");
        let headers: Vec<_> = iter_boxes(&buf).collect::<Result<_>>().unwrap();
        assert_eq!(headers.len(), 2);
        assert_eq!(&headers[0].box_type, b"ftyp");
        assert_eq!(headers[0].total_len, 0x20);
        assert_eq!(&headers[1].box_type, b"meta");
        assert_eq!(headers[1].payload_len, 0);
    }

    #[test]
    fn rejects_truncated() {
        let buf = [0, 0, 0, 0x20, b'f', b't', b'y', b'p', 0, 0, 0]; // 20 advertised, only 11 present
        let err = parse_box_header(&buf, 0).unwrap_err();
        assert!(format!("{err}").contains("out of range"));
    }

    /// `parse_box_header` rejects an offset close to `usize::MAX` instead
    /// of overflowing the `start + 8` computation — that path used to
    /// debug-panic before the saturating arithmetic was added.
    #[test]
    fn rejects_offset_overflow() {
        let buf = [0u8; 16];
        let err = parse_box_header(&buf, usize::MAX - 3).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("overflow") || msg.contains("truncated"),
            "expected overflow / truncated, got: {msg}"
        );
    }

    /// `read_u32` at an out-of-range offset (close to `usize::MAX`) must
    /// surface the `u32 offset overflow` error rather than wrap silently.
    #[test]
    fn read_u32_rejects_overflow_offset() {
        let buf = [0u8; 4];
        let err = read_u32(&buf, usize::MAX - 1).unwrap_err();
        assert!(format!("{err}").contains("overflow"));
    }
}
