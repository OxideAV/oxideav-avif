//! Bridge between this crate's frame model ([`AvifFrame`] +
//! [`AvifPixelFormat`], the layouts the AV1 decoder emits and the
//! framework consumes — planar YUV(A), gray, and the packed `Ya8` /
//! `Ya16Le` monochrome + alpha layouts) and the container crate's
//! planar [`HeifFrame`] that its composition layer (`grid` / `iovl` /
//! `iden`, alpha attachment, `clap` / `irot` / `imir`) works on.
//!
//! The conversion is a re-layout, never a resample: samples are copied
//! bit-for-bit, 16-bit little-endian words stay words, packed YA is
//! split into (or re-interleaved from) a luma plane and an alpha plane.

use oxideav_heif::{Chroma, HeifFrame, HeifPixelFormat, HeifPlane};

use crate::error::{AvifError as Error, Result};
use crate::image::{AvifFrame, AvifPixelFormat, AvifPlane};

/// Chroma structure of a crate-local layout.
fn chroma_of(format: AvifPixelFormat) -> Chroma {
    if format.plane_count() == 1 || format.is_packed_ya() {
        return Chroma::Mono;
    }
    match format.chroma_subsampling() {
        (1, 1) => Chroma::Yuv420,
        (1, 0) => Chroma::Yuv422,
        _ => Chroma::Yuv444,
    }
}

/// Effective coded depth of a layout (`Ya16Le` stores 16-bit words for
/// a 10 / 12-bit picture, so the caller supplies it).
fn depth_of(format: AvifPixelFormat, bit_depth: u8) -> u8 {
    if format == AvifPixelFormat::Ya16Le {
        bit_depth
    } else {
        format.bit_depth()
    }
}

/// Tightly packed copy of one plane (`row_bytes × rows`) out of a plane
/// whose stride may exceed the row width.
fn tight_plane(plane: &AvifPlane, row_bytes: usize, rows: usize, label: &str) -> Result<Vec<u8>> {
    if rows == 0 || row_bytes == 0 {
        return Ok(Vec::new());
    }
    if plane.stride < row_bytes || plane.data.len() < plane.stride * (rows - 1) + row_bytes {
        return Err(Error::invalid(format!(
            "avif: {label} plane too short ({} bytes, stride {}, need {rows} rows of {row_bytes})",
            plane.data.len(),
            plane.stride
        )));
    }
    if plane.stride == row_bytes && plane.data.len() == row_bytes * rows {
        return Ok(plane.data.clone());
    }
    let mut out = Vec::with_capacity(row_bytes * rows);
    for r in 0..rows {
        let s = r * plane.stride;
        out.extend_from_slice(&plane.data[s..s + row_bytes]);
    }
    Ok(out)
}

/// Re-layout a crate-local frame as the container's planar frame.
/// `bit_depth` is only consulted for [`AvifPixelFormat::Ya16Le`].
pub(crate) fn to_heif(
    frame: &AvifFrame,
    format: AvifPixelFormat,
    bit_depth: u8,
    width: u32,
    height: u32,
) -> Result<HeifFrame> {
    if width == 0 || height == 0 {
        return Err(Error::invalid("avif: zero-sized frame"));
    }
    let expect = format.plane_count();
    if frame.planes.len() != expect {
        return Err(Error::invalid(format!(
            "avif: {format:?} frame carries {} planes, expected {expect}",
            frame.planes.len()
        )));
    }
    let depth = depth_of(format, bit_depth);
    let chroma = chroma_of(format);
    let bps = format.bytes_per_sample();
    let hf = HeifPixelFormat::new(chroma, depth, format.has_alpha())?;
    let (w, h) = (width as usize, height as usize);
    let mut planes = Vec::with_capacity(hf.plane_count());
    if format.is_packed_ya() {
        let packed = tight_plane(&frame.planes[0], w * 2 * bps, h, "packed YA")?;
        let mut y = Vec::with_capacity(w * h * bps);
        let mut a = Vec::with_capacity(w * h * bps);
        for px in packed.chunks_exact(2 * bps) {
            y.extend_from_slice(&px[..bps]);
            a.extend_from_slice(&px[bps..]);
        }
        planes.push(HeifPlane {
            stride: w * bps,
            data: y,
        });
        planes.push(HeifPlane {
            stride: w * bps,
            data: a,
        });
    } else {
        for (p, src) in frame.planes.iter().enumerate() {
            let (pw, ph) = hf.plane_dims(p, width, height);
            let row_bytes = pw as usize * bps;
            let data = tight_plane(src, row_bytes, ph as usize, "colour")?;
            planes.push(HeifPlane {
                stride: row_bytes,
                data,
            });
        }
    }
    let out = HeifFrame {
        width,
        height,
        format: hf,
        planes,
    };
    out.validate()?;
    Ok(out)
}

/// Re-layout a container frame as a crate-local frame plus its layout
/// (`Ya8` / `Ya16Le` for monochrome + alpha).
pub(crate) fn from_heif(frame: &HeifFrame) -> Result<(AvifFrame, AvifPixelFormat)> {
    let hf = frame.format;
    let (sx, sy) = hf.chroma.shift();
    let gray = hf.chroma == Chroma::Mono;
    // The packed 16-bit-word monochrome + alpha layout is the one
    // crate-local layout whose storage width (16) is not its coded depth.
    let format = if gray && hf.has_alpha && hf.bit_depth == 16 {
        Some(AvifPixelFormat::Ya16Le)
    } else {
        AvifPixelFormat::from_layout(hf.bit_depth, sx as u8, sy as u8, gray, hf.has_alpha)
    };
    let format = format.ok_or_else(|| {
        Error::unsupported(format!(
            "avif: no pixel layout for {:?} {}-bit alpha={}",
            hf.chroma, hf.bit_depth, hf.has_alpha
        ))
    })?;
    let tight = frame.tight();
    let bps = hf.bytes_per_sample();
    let planes = if format.is_packed_ya() {
        let (y, a) = (&tight.planes[0], &tight.planes[1]);
        let mut data = Vec::with_capacity(y.data.len() + a.data.len());
        for (ys, as_) in y.data.chunks_exact(bps).zip(a.data.chunks_exact(bps)) {
            data.extend_from_slice(ys);
            data.extend_from_slice(as_);
        }
        vec![AvifPlane {
            stride: frame.width as usize * 2 * bps,
            data,
        }]
    } else {
        tight
            .planes
            .into_iter()
            .map(|p| AvifPlane {
                stride: p.stride,
                data: p.data,
            })
            .collect()
    };
    Ok((AvifFrame { pts: None, planes }, format))
}

/// Keep the top-left `w × h` window of a container frame, every plane
/// trimmed at its own resolution with ceiling extents — the AV1 /
/// AVIF reading of an odd-sized subsampled picture (chroma covers the
/// last luma column / row), applied by this crate where the container
/// would instead promote an odd trim to 4:4:4.
pub(crate) fn trim_top_left(frame: &HeifFrame, w: u32, h: u32) -> Result<HeifFrame> {
    if w == 0 || h == 0 || w > frame.width || h > frame.height {
        return Err(Error::invalid(format!(
            "avif: trim {w}x{h} outside {}x{}",
            frame.width, frame.height
        )));
    }
    if (w, h) == (frame.width, frame.height) {
        return Ok(frame.clone());
    }
    let bps = frame.format.bytes_per_sample();
    let mut planes = Vec::with_capacity(frame.planes.len());
    for (p, src) in frame.planes.iter().enumerate() {
        let (pw, ph) = frame.format.plane_dims(p, w, h);
        let row_bytes = pw as usize * bps;
        let mut data = Vec::with_capacity(row_bytes * ph as usize);
        for r in 0..ph as usize {
            let s = r * src.stride;
            let row = src
                .data
                .get(s..s + row_bytes)
                .ok_or_else(|| Error::invalid(format!("avif: trim reads past plane {p}")))?;
            data.extend_from_slice(row);
        }
        planes.push(HeifPlane {
            stride: row_bytes,
            data,
        });
    }
    let out = HeifFrame {
        width: w,
        height: h,
        format: frame.format,
        planes,
    };
    out.validate()?;
    Ok(out)
}

/// Drop the chroma planes of a frame whose colour content is
/// monochrome (the container promotes monochrome overlay inputs with
/// alpha to 4:4:4 with neutral chroma; this crate keeps them
/// monochrome). Alpha is kept.
pub(crate) fn demote_to_mono(frame: &HeifFrame) -> Result<HeifFrame> {
    if frame.format.chroma == Chroma::Mono {
        return Ok(frame.clone());
    }
    let format =
        HeifPixelFormat::new(Chroma::Mono, frame.format.bit_depth, frame.format.has_alpha)?;
    let mut planes = vec![frame.planes[0].clone()];
    if let Some(a) = frame.format.alpha_plane() {
        planes.push(frame.planes[a].clone());
    }
    let out = HeifFrame {
        width: frame.width,
        height: frame.height,
        format,
        planes,
    };
    out.validate()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(planes: Vec<(usize, Vec<u8>)>) -> AvifFrame {
        AvifFrame {
            pts: None,
            planes: planes
                .into_iter()
                .map(|(stride, data)| AvifPlane { stride, data })
                .collect(),
        }
    }

    #[test]
    fn yuv420_round_trips_and_drops_padding() {
        // 2x2 luma with a padded stride of 4; 1x1 chroma.
        let f = frame(vec![
            (4, vec![1, 2, 0, 0, 3, 4, 0, 0]),
            (1, vec![9]),
            (1, vec![8]),
        ]);
        let h = to_heif(&f, AvifPixelFormat::Yuv420P, 8, 2, 2).unwrap();
        assert_eq!(h.format.chroma, Chroma::Yuv420);
        assert_eq!(h.planes[0].data, vec![1, 2, 3, 4]);
        let (back, fmt) = from_heif(&h).unwrap();
        assert_eq!(fmt, AvifPixelFormat::Yuv420P);
        assert_eq!(back.planes[0].stride, 2);
        assert_eq!(back.planes[0].data, vec![1, 2, 3, 4]);
        assert_eq!(back.planes[1].data, vec![9]);
    }

    #[test]
    fn packed_ya16_splits_and_reinterleaves() {
        // Two pixels: (Y=0x0123, A=0x0FFF), (Y=0x0004, A=0x0000) as LE words.
        let f = frame(vec![(
            8,
            vec![0x23, 0x01, 0xFF, 0x0F, 0x04, 0x00, 0x00, 0x00],
        )]);
        let h = to_heif(&f, AvifPixelFormat::Ya16Le, 12, 2, 1).unwrap();
        assert_eq!(h.format.bit_depth, 12);
        assert!(h.format.has_alpha);
        assert_eq!(h.planes[0].data, vec![0x23, 0x01, 0x04, 0x00]);
        assert_eq!(h.planes[1].data, vec![0xFF, 0x0F, 0x00, 0x00]);
        let (back, fmt) = from_heif(&h).unwrap();
        assert_eq!(fmt, AvifPixelFormat::Ya16Le);
        assert_eq!(back.planes[0].data, f.planes[0].data);
    }

    #[test]
    fn gray8_with_alpha_becomes_ya8() {
        let h = HeifFrame {
            width: 2,
            height: 1,
            format: HeifPixelFormat::new(Chroma::Mono, 8, true).unwrap(),
            planes: vec![
                HeifPlane {
                    stride: 2,
                    data: vec![10, 20],
                },
                HeifPlane {
                    stride: 2,
                    data: vec![255, 0],
                },
            ],
        };
        let (back, fmt) = from_heif(&h).unwrap();
        assert_eq!(fmt, AvifPixelFormat::Ya8);
        assert_eq!(back.planes[0].data, vec![10, 255, 20, 0]);
    }

    #[test]
    fn short_plane_is_refused() {
        let f = frame(vec![(2, vec![1, 2, 3])]);
        assert!(to_heif(&f, AvifPixelFormat::Gray8, 8, 2, 2).is_err());
    }
}
