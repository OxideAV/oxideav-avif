//! AVIF grid-image composition (HEIF §6.6.2.3).
//!
//! A `grid` item stores a small payload declaring how many rows and
//! columns of tile items make up the picture, plus the final output
//! pixel dimensions. Each tile item is referenced from the grid via a
//! `dimg` iref entry. After every tile has been decoded the final
//! canvas is built by pasting the tiles in row-major order and cropping
//! to the declared output size.
//!
//! This module decodes the grid descriptor and exposes `composite_grid`
//! which assembles a single `VideoFrame` from a slice of decoded tile
//! frames. All tiles must share the same pixel format and tile size;
//! mismatches return `Error::InvalidData`.

use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{Error, PixelFormat, Result};

/// Parsed `ImageGridBox` payload. Dimensions may be 16-bit or 32-bit
/// depending on `flags & 1` (bit 0 = 1 selects the 32-bit layout).
#[derive(Clone, Copy, Debug)]
pub struct ImageGrid {
    pub version: u8,
    pub flags: u8,
    pub rows: u16,
    pub columns: u16,
    pub output_width: u32,
    pub output_height: u32,
}

impl ImageGrid {
    /// Parse a grid item's payload — the bytes returned by
    /// `iloc`-resolving the grid item. Spec: HEIF §6.6.2.3.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        if payload.len() < 8 {
            return Err(Error::InvalidData(format!(
                "avif grid: payload {} bytes < 8",
                payload.len()
            )));
        }
        let version = payload[0];
        if version != 0 {
            return Err(Error::InvalidData(format!("avif grid: version {version}")));
        }
        let flags = payload[1];
        let wide = (flags & 1) != 0;
        let rows = (payload[2] as u16) + 1;
        let columns = (payload[3] as u16) + 1;
        let mut pos = 4;
        let (output_width, output_height) = if wide {
            if payload.len() < pos + 8 {
                return Err(Error::InvalidData(
                    "avif grid: 32-bit dims truncated".to_string(),
                ));
            }
            let w = u32::from_be_bytes([
                payload[pos],
                payload[pos + 1],
                payload[pos + 2],
                payload[pos + 3],
            ]);
            pos += 4;
            let h = u32::from_be_bytes([
                payload[pos],
                payload[pos + 1],
                payload[pos + 2],
                payload[pos + 3],
            ]);
            (w, h)
        } else {
            if payload.len() < pos + 4 {
                return Err(Error::InvalidData(
                    "avif grid: 16-bit dims truncated".to_string(),
                ));
            }
            let w = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as u32;
            let h = u16::from_be_bytes([payload[pos + 2], payload[pos + 3]]) as u32;
            (w, h)
        };
        Ok(Self {
            version,
            flags,
            rows,
            columns,
            output_width,
            output_height,
        })
    }

    pub fn expected_tile_count(&self) -> usize {
        (self.rows as usize) * (self.columns as usize)
    }
}

/// Composite the decoded tile frames in `tiles` into a single output
/// frame of `grid.output_width × grid.output_height`, pasting tiles in
/// row-major order (`tile_index = row * columns + col`) at `(col *
/// tile_w, row * tile_h)`. Tiles that spill past the output rectangle
/// are clipped at the edge.
///
/// All tiles must share the same pixel format + dimensions; the output
/// frame inherits that format. The caller is responsible for decoding
/// the tile items via the regular AV1 path and passing them in the same
/// order as the `dimg` iref.
pub fn composite_grid(grid: &ImageGrid, tiles: &[VideoFrame]) -> Result<VideoFrame> {
    let expected = grid.expected_tile_count();
    if tiles.len() != expected {
        return Err(Error::InvalidData(format!(
            "avif grid: {}×{} grid declares {expected} tiles but got {}",
            grid.rows,
            grid.columns,
            tiles.len()
        )));
    }
    if tiles.is_empty() {
        return Err(Error::InvalidData("avif grid: empty tile list".to_string()));
    }
    let format = tiles[0].format;
    let tile_w = tiles[0].width;
    let tile_h = tiles[0].height;
    for (i, t) in tiles.iter().enumerate().skip(1) {
        if t.format != format || t.width != tile_w || t.height != tile_h {
            return Err(Error::InvalidData(format!(
                "avif grid: tile {i} size/format {:?} {}x{} differs from tile 0 {:?} {}x{}",
                t.format, t.width, t.height, format, tile_w, tile_h
            )));
        }
    }
    let out_w = grid.output_width;
    let out_h = grid.output_height;
    if out_w == 0 || out_h == 0 {
        return Err(Error::InvalidData(
            "avif grid: output dimensions zero".to_string(),
        ));
    }
    // Grid rows × tile_h must cover output_height (same for width).
    if (tile_w as u64) * (grid.columns as u64) < out_w as u64
        || (tile_h as u64) * (grid.rows as u64) < out_h as u64
    {
        return Err(Error::InvalidData(format!(
            "avif grid: {}×{} tiles of {}x{} don't cover {}x{}",
            grid.rows, grid.columns, tile_w, tile_h, out_w, out_h
        )));
    }
    let planes = format.plane_count();
    if planes == 0 || tiles[0].planes.len() != planes {
        return Err(Error::InvalidData(format!(
            "avif grid: format {:?} expects {} planes, got {}",
            format,
            planes,
            tiles[0].planes.len()
        )));
    }
    let (sx, sy) = subsampling_shifts(format)?;
    // Build planar output buffers at the output grid's final size.
    let mut out_planes: Vec<VideoPlane> = Vec::with_capacity(planes);
    for p in 0..planes {
        let (pw, ph) = plane_dims(out_w, out_h, p, sx, sy);
        out_planes.push(VideoPlane {
            stride: pw as usize,
            data: vec![0u8; (pw as usize) * (ph as usize)],
        });
    }
    for (i, tile) in tiles.iter().enumerate() {
        let row = i / grid.columns as usize;
        let col = i % grid.columns as usize;
        let dst_x = col as u32 * tile_w;
        let dst_y = row as u32 * tile_h;
        if dst_x >= out_w || dst_y >= out_h {
            // Tiles entirely outside the declared output rectangle are
            // silently dropped — goavif behaves the same.
            continue;
        }
        let copy_w = (out_w - dst_x).min(tile_w);
        let copy_h = (out_h - dst_y).min(tile_h);
        for (p, (src, dst)) in tile
            .planes
            .iter()
            .zip(out_planes.iter_mut())
            .enumerate()
            .take(planes)
        {
            let (ppw_src, _pph_src) = plane_dims(tile_w, tile_h, p, sx, sy);
            let (ppw_dst, _pph_dst) = plane_dims(out_w, out_h, p, sx, sy);
            let plane_shift_x = if p == 0 { 0 } else { sx };
            let plane_shift_y = if p == 0 { 0 } else { sy };
            let plane_dst_x = dst_x >> plane_shift_x;
            let plane_dst_y = dst_y >> plane_shift_y;
            let plane_copy_w = (copy_w >> plane_shift_x).max(1);
            let plane_copy_h = (copy_h >> plane_shift_y).max(1);
            for row_i in 0..plane_copy_h as usize {
                let dst_row_start =
                    (plane_dst_y as usize + row_i) * dst.stride + plane_dst_x as usize;
                let src_row_start = row_i * src.stride;
                let cw = plane_copy_w as usize;
                if dst_row_start + cw > dst.data.len() || src_row_start + cw > src.data.len() {
                    return Err(Error::InvalidData(format!(
                        "avif grid: tile {i} plane {p} row {row_i} out of range (src_w={}, dst_w={})",
                        ppw_src, ppw_dst
                    )));
                }
                dst.data[dst_row_start..dst_row_start + cw]
                    .copy_from_slice(&src.data[src_row_start..src_row_start + cw]);
            }
        }
    }
    Ok(VideoFrame {
        format,
        width: out_w,
        height: out_h,
        pts: tiles[0].pts,
        time_base: tiles[0].time_base,
        planes: out_planes,
    })
}

fn subsampling_shifts(format: PixelFormat) -> Result<(u32, u32)> {
    match format {
        PixelFormat::Yuv420P => Ok((1, 1)),
        PixelFormat::Yuv422P => Ok((1, 0)),
        PixelFormat::Yuv444P | PixelFormat::Gray8 => Ok((0, 0)),
        other => Err(Error::unsupported(format!(
            "avif grid: pixel format {other:?} not supported"
        ))),
    }
}

fn plane_dims(w: u32, h: u32, plane: usize, sx: u32, sy: u32) -> (u32, u32) {
    if plane == 0 {
        (w, h)
    } else {
        let pw = (w + (1 << sx) - 1) >> sx;
        let ph = (h + (1 << sy) - 1) >> sy;
        (pw.max(1), ph.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::TimeBase;

    fn make_gray_tile(w: u32, h: u32, fill: u8) -> VideoFrame {
        VideoFrame {
            format: PixelFormat::Gray8,
            width: w,
            height: h,
            pts: None,
            time_base: TimeBase::new(1, 1),
            planes: vec![VideoPlane {
                stride: w as usize,
                data: vec![fill; (w * h) as usize],
            }],
        }
    }

    #[test]
    fn parse_16bit_dims() {
        // version 0, flags 0 (16-bit), rows_minus_1=1, cols_minus_1=1,
        // output_width=0x0200, output_height=0x0100
        let buf = [0u8, 0, 1, 1, 0x02, 0x00, 0x01, 0x00];
        let g = ImageGrid::parse(&buf).unwrap();
        assert_eq!(g.rows, 2);
        assert_eq!(g.columns, 2);
        assert_eq!(g.output_width, 0x200);
        assert_eq!(g.output_height, 0x100);
    }

    #[test]
    fn parse_32bit_dims() {
        let mut buf = vec![0u8, 1, 0, 1]; // flags=1 -> 32-bit, rows=1, cols=2
        buf.extend_from_slice(&256u32.to_be_bytes());
        buf.extend_from_slice(&128u32.to_be_bytes());
        let g = ImageGrid::parse(&buf).unwrap();
        assert_eq!(g.rows, 1);
        assert_eq!(g.columns, 2);
        assert_eq!(g.output_width, 256);
        assert_eq!(g.output_height, 128);
    }

    #[test]
    fn parse_bad_version() {
        let buf = [1u8, 0, 0, 0, 0, 0, 0, 0];
        let err = ImageGrid::parse(&buf).unwrap_err();
        match err {
            Error::InvalidData(_) => {}
            _ => panic!("expected InvalidData"),
        }
    }

    #[test]
    fn composite_2x2_grid_gray() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 2,
            columns: 2,
            output_width: 4,
            output_height: 4,
        };
        let tiles = [
            make_gray_tile(2, 2, 10),
            make_gray_tile(2, 2, 20),
            make_gray_tile(2, 2, 30),
            make_gray_tile(2, 2, 40),
        ];
        let out = composite_grid(&grid, &tiles).unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 4);
        // Row 0: tile 0 at x=0 (10s), tile 1 at x=2 (20s).
        assert_eq!(&out.planes[0].data[0..2], &[10, 10]);
        assert_eq!(&out.planes[0].data[2..4], &[20, 20]);
        // Row 2: tile 2 at x=0 (30s), tile 3 at x=2 (40s).
        assert_eq!(&out.planes[0].data[8..10], &[30, 30]);
        assert_eq!(&out.planes[0].data[10..12], &[40, 40]);
    }

    #[test]
    fn composite_tile_count_mismatch() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 2,
            columns: 2,
            output_width: 4,
            output_height: 4,
        };
        let tiles = [make_gray_tile(2, 2, 10)];
        assert!(composite_grid(&grid, &tiles).is_err());
    }

    #[test]
    fn composite_crops_trailing_tiles() {
        // 2x2 tiles of 2x2 but output is only 3x3 — right column and
        // bottom row are cropped to 1 pixel each.
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 2,
            columns: 2,
            output_width: 3,
            output_height: 3,
        };
        let tiles = [
            make_gray_tile(2, 2, 10),
            make_gray_tile(2, 2, 20),
            make_gray_tile(2, 2, 30),
            make_gray_tile(2, 2, 40),
        ];
        let out = composite_grid(&grid, &tiles).unwrap();
        assert_eq!(out.width, 3);
        assert_eq!(out.height, 3);
        // Top-right tile contributes only a 1-pixel column.
        assert_eq!(out.planes[0].data[2], 20);
        assert_eq!(out.planes[0].data[5], 20);
        // Bottom-left tile contributes only a 1-pixel row.
        assert_eq!(out.planes[0].data[6], 30);
        assert_eq!(out.planes[0].data[7], 30);
    }
}
