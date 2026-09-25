//! ISO Base Media File Format box-header access (ISO/IEC 14496-12 §4.2,
//! §4.2.2 FullBox), served by the container crate.
//!
//! The box reader itself lives in [`oxideav_heif::boxes`]; this module
//! is the thin surface the AVIF profile layer uses — the same
//! [`BoxHeader`] / [`iter_boxes`] / [`find_box`] / [`parse_full_box`]
//! names as before, returning the crate-local [`Result`] — plus the
//! bounds-checked scalar readers the AVIF-specific box parsers
//! (`prft`, `ssix`, sample groups, entity groups, gain-map payloads)
//! consume.

use crate::error::{AvifError as Error, Result};

pub use oxideav_heif::boxes::BoxHeader;

/// 4-character box type, compared bytewise. Identical to
/// [`oxideav_heif::boxes::FourCc`].
pub type BoxType = oxideav_heif::boxes::FourCc;

/// Convert a 4-byte ASCII literal to a `BoxType` at compile time.
pub const fn b(s: &[u8; 4]) -> BoxType {
    *s
}

/// Readable rendering for error messages.
pub fn type_str(t: &BoxType) -> String {
    String::from_utf8_lossy(t).into_owned()
}

/// Iterate the boxes packed contiguously inside `buf`. Stops cleanly at
/// end of buffer; surfaces an error on any truncated size field.
pub fn iter_boxes(buf: &[u8]) -> BoxIter<'_> {
    BoxIter {
        inner: oxideav_heif::boxes::iter_boxes(buf),
    }
}

/// Iterator returned by [`iter_boxes`].
pub struct BoxIter<'a> {
    inner: oxideav_heif::boxes::BoxIter<'a>,
}

impl Iterator for BoxIter<'_> {
    type Item = Result<BoxHeader>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|r| r.map_err(Error::from))
    }
}

/// Parse a single box header at `start`.
pub fn parse_box_header(buf: &[u8], start: usize) -> Result<BoxHeader> {
    oxideav_heif::boxes::parse_box_header(buf, start).map_err(Error::from)
}

/// Parse a FullBox prefix `version(1) + flags(3)` and return
/// `(version, flags, remaining)`.
pub fn parse_full_box(payload: &[u8]) -> Result<(u8, u32, &[u8])> {
    oxideav_heif::boxes::parse_full_box(payload).map_err(Error::from)
}

/// Find the first box whose type matches `target` among the contiguous
/// boxes in `buf`. Returns `Ok(None)` when not present, `Err` on a
/// parse failure.
pub fn find_box<'a>(buf: &'a [u8], target: &BoxType) -> Result<Option<(&'a [u8], BoxHeader)>> {
    Ok(oxideav_heif::boxes::find_box(buf, target)?.map(|(h, p)| (p, h)))
}

fn reader_at<'a>(
    buf: &'a [u8],
    at: usize,
    width: usize,
    what: &str,
) -> Result<oxideav_heif::boxes::Reader<'a>> {
    let end = at
        .checked_add(width)
        .ok_or_else(|| Error::invalid(format!("avif: {what} offset overflow")))?;
    if end > buf.len() {
        return Err(Error::invalid(format!("avif: truncated {what} read")));
    }
    Ok(oxideav_heif::boxes::Reader::new(&buf[at..]))
}

#[doc(hidden)]
pub fn read_u16(buf: &[u8], at: usize) -> Result<u16> {
    Ok(reader_at(buf, at, 2, "u16")?.u16("u16")?)
}

#[doc(hidden)]
pub fn read_u32(buf: &[u8], at: usize) -> Result<u32> {
    Ok(reader_at(buf, at, 4, "u32")?.u32("u32")?)
}

#[doc(hidden)]
pub fn read_u64(buf: &[u8], at: usize) -> Result<u64> {
    Ok(reader_at(buf, at, 8, "u64")?.u64("u64")?)
}

/// Read a variable-width big-endian unsigned integer of `width_bytes`
/// bytes starting at `at`. `width_bytes` may be 0, 4, or 8 per
/// ISO/IEC 14496-12 §8.11.3 (`iloc`).
#[doc(hidden)]
pub fn read_var_uint(buf: &[u8], at: usize, width_bytes: usize) -> Result<u64> {
    match width_bytes {
        0 => Ok(0),
        4 | 8 => {
            Ok(reader_at(buf, at, width_bytes, "iloc field")?.uint(width_bytes, "iloc field")?)
        }
        _ => Err(Error::invalid(format!(
            "avif: unsupported iloc field width {width_bytes}"
        ))),
    }
}

/// Null-terminated string starting at `at`, advancing the caller's
/// cursor past the terminator. Returns `(string, new_offset)`.
#[doc(hidden)]
pub fn read_cstr(buf: &[u8], at: usize) -> Result<(String, usize)> {
    let tail = buf
        .get(at..)
        .ok_or_else(|| Error::invalid("avif: unterminated C string"))?;
    let nul = tail
        .iter()
        .position(|&c| c == 0)
        .ok_or_else(|| Error::invalid("avif: unterminated C string"))?;
    let mut r = oxideav_heif::boxes::Reader::new(&tail[..=nul]);
    let s = r.cstr("string")?;
    Ok((s, at + nul + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_ftyp_then_meta() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x20u32.to_be_bytes());
        buf.extend_from_slice(b"ftyp");
        buf.extend_from_slice(&[0u8; 0x18]);
        buf.extend_from_slice(&0x08u32.to_be_bytes());
        buf.extend_from_slice(b"meta");
        let headers: Vec<_> = iter_boxes(&buf).collect::<Result<_>>().unwrap();
        assert_eq!(headers.len(), 2);
        assert_eq!(&headers[0].box_type, b"ftyp");
        assert_eq!(headers[0].total_len(), 0x20);
        assert_eq!(&headers[1].box_type, b"meta");
        assert_eq!(headers[1].payload_len, 0);
    }

    #[test]
    fn rejects_truncated() {
        let buf = [0, 0, 0, 0x20, b'f', b't', b'y', b'p', 0, 0, 0]; // 20 advertised, only 11 present
        let err = parse_box_header(&buf, 0).unwrap_err();
        assert!(format!("{err}").contains("out of range"));
    }

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

    #[test]
    fn read_u32_rejects_overflow_offset() {
        let buf = [0u8; 4];
        let err = read_u32(&buf, usize::MAX - 1).unwrap_err();
        assert!(format!("{err}").contains("overflow"));
    }
}
