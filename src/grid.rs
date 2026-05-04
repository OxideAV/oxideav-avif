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

use crate::error::{AvifError as Error, Result};
use crate::image::{AvifFrame as VideoFrame, AvifPixelFormat as PixelFormat, AvifPlane as VideoPlane};

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
///
/// `format`, `tile_w`, `tile_h` describe every tile (per-frame metadata
/// no longer rides on [`VideoFrame`]). The returned frame's display
/// dimensions are `(grid.output_width, grid.output_height)`.
pub fn composite_grid(
    grid: &ImageGrid,
    tiles: &[VideoFrame],
    format: PixelFormat,
    tile_w: u32,
    tile_h: u32,
) -> Result<VideoFrame> {
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
            // dst_x / dst_y are luma coordinates; chroma offsets are the
            // shift-divided values. tile_w / tile_h are even for any
            // 4:2:x / 4:2:0 AV1 tile (AV1 §5.6.1 requires even coded
            // dimensions for subsampled chroma), so dst_x >> sx and
            // dst_y >> sy are exact whole chroma columns / rows.
            let plane_dst_x = dst_x >> plane_shift_x;
            let plane_dst_y = dst_y >> plane_shift_y;
            // Chroma copy extents use **ceiling** division of the luma
            // copy extents — when the right-most or bottom-most tile is
            // trimmed to an odd luma count (HEIF §6.6.2.3.3 allows the
            // last column / row to be partial), a plain `>> 1` would
            // drop the trailing chroma sample. Example: 4:2:0 grid with
            // tile_w=4 + output_w=7. tile 1 contributes copy_w=3 luma
            // cols, which cover 2 chroma cols (cols 2 and 3), not 1.
            // The `.max(1)` floor remains for the degenerate copy_w=0
            // case (filtered out earlier by the `dst_x >= out_w`
            // guard, but kept for defence in depth).
            let plane_copy_w = ceil_shift(copy_w, plane_shift_x).max(1);
            let plane_copy_h = ceil_shift(copy_h, plane_shift_y).max(1);
            // Also clamp the chroma copy to the source tile's chroma
            // plane width / height — when the tile happens to have
            // fewer chroma samples than the luma-derived ceiling
            // suggests (e.g. an encoder that rounded down), copying past
            // the source row boundary would smear later luma data into
            // chroma. ppw_src / chroma rows of source are the upper
            // bound.
            let src_chroma_h = (src.data.len() / src.stride.max(1)) as u32;
            let plane_copy_w = plane_copy_w.min(ppw_src);
            let plane_copy_h = plane_copy_h.min(src_chroma_h);
            // And clamp again to the destination plane's available
            // columns / rows so a tile that spills past the canvas
            // edge silently truncates rather than walking off the
            // buffer.
            let plane_copy_w = plane_copy_w.min(ppw_dst.saturating_sub(plane_dst_x));
            let plane_copy_h = plane_copy_h.min(
                (dst.data.len() as u32 / dst.stride.max(1) as u32).saturating_sub(plane_dst_y),
            );
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
        pts: tiles[0].pts,
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

/// Ceiling shift — `ceil(v / 2^shift)`. Used to map a luma extent to
/// the chroma extent that fully covers it. The reverse of the floor
/// shift used to derive chroma plane *positions* (chroma-plane offsets
/// always use plain `>>` because tile-edge alignment guarantees luma
/// coordinates land on even chroma boundaries — see
/// [`composite_grid`]).
fn ceil_shift(v: u32, shift: u32) -> u32 {
    if shift == 0 {
        v
    } else {
        let unit = 1u32 << shift;
        (v + unit - 1) >> shift
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_gray_tile(w: u32, h: u32, fill: u8) -> VideoFrame {
        VideoFrame {
            pts: None,
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
        let out = composite_grid(&grid, &tiles, PixelFormat::Gray8, 2, 2).unwrap();
        assert_eq!(out.planes[0].stride, 4);
        assert_eq!(out.planes[0].data.len() / out.planes[0].stride, 4);
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
        assert!(composite_grid(&grid, &tiles, PixelFormat::Gray8, 2, 2).is_err());
    }

    /// HEIF §6.6.2.3.1 says: "tile_width*columns is greater than or
    /// equal to output_width and tile_height*rows is greater than or
    /// equal to output_height". A grid whose tiles can't cover the
    /// output rectangle must error.
    #[test]
    fn composite_undersized_grid_rejected() {
        // 2x2 grid of 2x2 tiles claims 5x5 output (needs at least
        // 6x6). Should error.
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 2,
            columns: 2,
            output_width: 5,
            output_height: 5,
        };
        let tiles = [
            make_gray_tile(2, 2, 10),
            make_gray_tile(2, 2, 20),
            make_gray_tile(2, 2, 30),
            make_gray_tile(2, 2, 40),
        ];
        let err = composite_grid(&grid, &tiles, PixelFormat::Gray8, 2, 2).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    /// Non-square 1x4 grid (single row, four tiles) — exercises the
    /// row-major paste with rows=1.
    #[test]
    fn composite_1x4_horizontal_strip() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 1,
            columns: 4,
            output_width: 8,
            output_height: 2,
        };
        let tiles = [
            make_gray_tile(2, 2, 11),
            make_gray_tile(2, 2, 22),
            make_gray_tile(2, 2, 33),
            make_gray_tile(2, 2, 44),
        ];
        let out = composite_grid(&grid, &tiles, PixelFormat::Gray8, 2, 2).unwrap();
        assert_eq!(out.planes[0].stride, 8);
        // First row cells: 11,11,22,22,33,33,44,44.
        assert_eq!(&out.planes[0].data[..8], &[11, 11, 22, 22, 33, 33, 44, 44]);
        // Second row, same content.
        assert_eq!(
            &out.planes[0].data[8..16],
            &[11, 11, 22, 22, 33, 33, 44, 44]
        );
    }

    /// Non-square 4x1 grid (single column, four tiles).
    #[test]
    fn composite_4x1_vertical_strip() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 4,
            columns: 1,
            output_width: 2,
            output_height: 8,
        };
        let tiles = [
            make_gray_tile(2, 2, 1),
            make_gray_tile(2, 2, 2),
            make_gray_tile(2, 2, 3),
            make_gray_tile(2, 2, 4),
        ];
        let out = composite_grid(&grid, &tiles, PixelFormat::Gray8, 2, 2).unwrap();
        assert_eq!(out.planes[0].stride, 2);
        // 4 row blocks of 2 rows each, fills 1,1,2,2,3,3,4,4 per column.
        for r in 0..8 {
            let block = (r / 2) + 1;
            assert_eq!(
                &out.planes[0].data[r * 2..r * 2 + 2],
                &[block as u8, block as u8]
            );
        }
    }

    /// 1x1 degenerate grid: one tile, no compositing — output equals
    /// the single tile cropped to the declared output size.
    #[test]
    fn composite_1x1_degenerate_grid() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 1,
            columns: 1,
            output_width: 3,
            output_height: 3,
        };
        let tiles = [make_gray_tile(4, 4, 99)];
        let out = composite_grid(&grid, &tiles, PixelFormat::Gray8, 4, 4).unwrap();
        assert_eq!(out.planes[0].stride, 3);
        assert_eq!(out.planes[0].data.len(), 9);
        for &v in &out.planes[0].data {
            assert_eq!(v, 99);
        }
    }

    /// Output rectangle that only fits part of the bottom-most row of
    /// tiles — those bottom tiles must be cropped to a single row each.
    #[test]
    fn composite_crops_bottom_row_to_one_pixel() {
        // 2x2 grid of 2x2 tiles, output is 4x3 — bottom row tiles
        // contribute exactly 1 pixel of height.
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 2,
            columns: 2,
            output_width: 4,
            output_height: 3,
        };
        let tiles = [
            make_gray_tile(2, 2, 10),
            make_gray_tile(2, 2, 20),
            make_gray_tile(2, 2, 30),
            make_gray_tile(2, 2, 40),
        ];
        let out = composite_grid(&grid, &tiles, PixelFormat::Gray8, 2, 2).unwrap();
        assert_eq!(out.planes[0].stride, 4);
        assert_eq!(out.planes[0].data.len(), 12);
        // Row 2 (last visible row) takes the first row of tiles 2 + 3.
        assert_eq!(&out.planes[0].data[8..12], &[30, 30, 40, 40]);
    }

    /// 4:2:0 grid composition: each tile carries Y(2x2) + U(1x1) + V(1x1).
    /// Confirms the chroma-plane shift logic correctly stitches the
    /// subsampled planes at half-resolution offsets.
    #[test]
    fn composite_2x2_grid_yuv420() {
        let make_tile = |yfill: u8, ufill: u8, vfill: u8| VideoFrame {
            pts: None,
            planes: vec![
                VideoPlane {
                    stride: 2,
                    data: vec![yfill; 4],
                },
                VideoPlane {
                    stride: 1,
                    data: vec![ufill; 1],
                },
                VideoPlane {
                    stride: 1,
                    data: vec![vfill; 1],
                },
            ],
        };
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 2,
            columns: 2,
            output_width: 4,
            output_height: 4,
        };
        let tiles = [
            make_tile(10, 100, 200),
            make_tile(20, 110, 210),
            make_tile(30, 120, 220),
            make_tile(40, 130, 230),
        ];
        let out = composite_grid(&grid, &tiles, PixelFormat::Yuv420P, 2, 2).unwrap();
        assert_eq!(out.planes.len(), 3);
        // Y plane: 4x4.
        let y = &out.planes[0];
        assert_eq!(y.stride, 4);
        assert_eq!(y.data.len(), 16);
        // Y rows 0-1 hold tiles 0,1 luma; rows 2-3 hold tiles 2,3.
        assert_eq!(&y.data[0..4], &[10, 10, 20, 20]);
        assert_eq!(&y.data[8..12], &[30, 30, 40, 40]);
        // U plane: 2x2 (each tile contributes one chroma pixel).
        let u = &out.planes[1];
        assert_eq!(u.stride, 2);
        assert_eq!(u.data.len(), 4);
        assert_eq!(u.data, vec![100, 110, 120, 130]);
        // V plane: same shape.
        let v = &out.planes[2];
        assert_eq!(v.data, vec![200, 210, 220, 230]);
    }

    /// Reject mismatched plane counts — a tile passed in with the wrong
    /// number of planes for the requested PixelFormat must return
    /// `InvalidData`.
    #[test]
    fn composite_plane_count_mismatch_rejected() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 1,
            columns: 1,
            output_width: 2,
            output_height: 2,
        };
        // Single Gray8 tile passed in for a 4:2:0 grid — yuv420p
        // expects 3 planes per tile.
        let tile = make_gray_tile(2, 2, 50);
        let err = composite_grid(&grid, &[tile], PixelFormat::Yuv420P, 2, 2).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    /// Reject zero output dimensions — neither width nor height may be 0.
    #[test]
    fn composite_zero_output_rejected() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 1,
            columns: 1,
            output_width: 0,
            output_height: 4,
        };
        let tile = make_gray_tile(2, 2, 5);
        assert!(composite_grid(&grid, &[tile], PixelFormat::Gray8, 2, 2).is_err());
    }

    /// Maximum-size descriptor: rows and columns each at 0xFF (after the
    /// `+1` they become 256), with tiny tile dimensions so the data
    /// stays bounded. Confirms the parser doesn't underflow / overflow
    /// at the edge of `rows_minus_one` / `columns_minus_one`.
    #[test]
    fn parse_max_rows_cols() {
        let buf = [0u8, 0, 0xff, 0xff, 0x00, 0x10, 0x00, 0x10];
        let g = ImageGrid::parse(&buf).unwrap();
        assert_eq!(g.rows, 256);
        assert_eq!(g.columns, 256);
        assert_eq!(g.expected_tile_count(), 256 * 256);
        assert_eq!(g.output_width, 16);
    }

    /// `expected_tile_count` must match `rows * columns`, including the
    /// trivial 1x1 case.
    #[test]
    fn expected_tile_count_basic() {
        let g = ImageGrid {
            version: 0,
            flags: 0,
            rows: 1,
            columns: 1,
            output_width: 2,
            output_height: 2,
        };
        assert_eq!(g.expected_tile_count(), 1);
        let g = ImageGrid {
            version: 0,
            flags: 0,
            rows: 7,
            columns: 5,
            output_width: 70,
            output_height: 50,
        };
        assert_eq!(g.expected_tile_count(), 35);
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
        let out = composite_grid(&grid, &tiles, PixelFormat::Gray8, 2, 2).unwrap();
        assert_eq!(out.planes[0].stride, 3);
        assert_eq!(out.planes[0].data.len() / out.planes[0].stride, 3);
        // Top-right tile contributes only a 1-pixel column.
        assert_eq!(out.planes[0].data[2], 20);
        assert_eq!(out.planes[0].data[5], 20);
        // Bottom-left tile contributes only a 1-pixel row.
        assert_eq!(out.planes[0].data[6], 30);
        assert_eq!(out.planes[0].data[7], 30);
    }

    /// Build a 4:2:0 tile of luma `tile_w × tile_h` plus its (tile_w/2 ×
    /// tile_h/2) chroma planes, every plane filled with the given fill
    /// values. Used by the round-21 chroma-edge tests.
    fn make_yuv420_tile(tile_w: u32, tile_h: u32, y: u8, u: u8, v: u8) -> VideoFrame {
        let lw = tile_w as usize;
        let lh = tile_h as usize;
        let cw = (tile_w / 2) as usize;
        let ch = (tile_h / 2) as usize;
        VideoFrame {
            pts: None,
            planes: vec![
                VideoPlane {
                    stride: lw,
                    data: vec![y; lw * lh],
                },
                VideoPlane {
                    stride: cw,
                    data: vec![u; cw * ch],
                },
                VideoPlane {
                    stride: cw,
                    data: vec![v; cw * ch],
                },
            ],
        }
    }

    /// Round 21: chroma-edge sample handling for a 4:2:0 grid whose
    /// output_width is odd. With `tile_w = 4`, `output_w = 7` the
    /// right-most tile contributes 3 luma columns. A naive `copy_w >> 1`
    /// gives 1 chroma column — which loses the trailing chroma sample
    /// even though the chroma plane is 4 samples wide
    /// (`(7 + 1) / 2 = 4`). The fix uses `ceil(copy_w / 2)` so the
    /// right edge contributes 2 chroma cols, fully filling the canvas
    /// chroma plane.
    #[test]
    fn composite_yuv420_odd_width_copies_full_chroma_edge() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 1,
            columns: 2,
            output_width: 7,
            output_height: 4,
        };
        let tiles = [
            make_yuv420_tile(4, 4, 10, 100, 200),
            make_yuv420_tile(4, 4, 20, 110, 210),
        ];
        let out = composite_grid(&grid, &tiles, PixelFormat::Yuv420P, 4, 4).unwrap();
        // Y plane: 7×4. Tile 0 fills cols 0..=3, tile 1 fills cols 4..=6.
        let y = &out.planes[0];
        assert_eq!(y.stride, 7);
        assert_eq!(y.data.len(), 28);
        for row in 0..4 {
            assert_eq!(&y.data[row * 7..row * 7 + 4], &[10, 10, 10, 10]);
            assert_eq!(&y.data[row * 7 + 4..row * 7 + 7], &[20, 20, 20]);
        }
        // U plane: ceil(7/2)=4 cols × ceil(4/2)=2 rows.
        let u = &out.planes[1];
        assert_eq!(u.stride, 4);
        assert_eq!(u.data.len(), 8);
        // Tile 0 covers chroma cols 0..=1 (2 cols, full tile chroma);
        // tile 1 covers chroma cols 2..=3 (2 cols — ceil(3/2)=2).
        // Without the ceil-shift fix the right two cols would still be 0.
        for row in 0..2 {
            assert_eq!(
                &u.data[row * 4..row * 4 + 4],
                &[100, 100, 110, 110],
                "row {row} chroma U trailing samples lost — chroma off-by-one at tile edge"
            );
        }
        let v = &out.planes[2];
        for row in 0..2 {
            assert_eq!(&v.data[row * 4..row * 4 + 4], &[200, 200, 210, 210]);
        }
    }

    /// Round 21: same off-by-one risk on the **bottom** edge. 1-column
    /// 2-row grid of 4×4 4:2:0 tiles, output_height = 7 (odd). Bottom
    /// tile contributes 3 luma rows; ceil(3/2) = 2 chroma rows. Without
    /// the ceil-shift fix only 1 chroma row would be copied, leaving
    /// the canvas chroma row 3 blank.
    #[test]
    fn composite_yuv420_odd_height_copies_full_chroma_edge() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 2,
            columns: 1,
            output_width: 4,
            output_height: 7,
        };
        let tiles = [
            make_yuv420_tile(4, 4, 10, 100, 200),
            make_yuv420_tile(4, 4, 20, 110, 210),
        ];
        let out = composite_grid(&grid, &tiles, PixelFormat::Yuv420P, 4, 4).unwrap();
        // U plane: 2 cols × ceil(7/2)=4 rows.
        let u = &out.planes[1];
        assert_eq!(u.stride, 2);
        assert_eq!(u.data.len(), 8);
        // Rows 0-1 hold tile 0 chroma (100), rows 2-3 hold tile 1 (110).
        assert_eq!(&u.data[0..2], &[100, 100]);
        assert_eq!(&u.data[2..4], &[100, 100]);
        assert_eq!(&u.data[4..6], &[110, 110]);
        assert_eq!(
            &u.data[6..8],
            &[110, 110],
            "bottom-most chroma row lost — chroma off-by-one at vertical tile edge"
        );
    }

    /// Round 21: a grid with odd output **on both axes** simultaneously
    /// — 2×2 tiles of 4×4, output 7×7. Verifies the corner tile (bottom
    /// right) is trimmed to 3×3 luma + 2×2 chroma without dropping
    /// either the trailing column or the trailing row.
    #[test]
    fn composite_yuv420_odd_both_axes_trims_corner_tile() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 2,
            columns: 2,
            output_width: 7,
            output_height: 7,
        };
        let tiles = [
            make_yuv420_tile(4, 4, 10, 100, 200),
            make_yuv420_tile(4, 4, 20, 110, 210),
            make_yuv420_tile(4, 4, 30, 120, 220),
            make_yuv420_tile(4, 4, 40, 130, 230),
        ];
        let out = composite_grid(&grid, &tiles, PixelFormat::Yuv420P, 4, 4).unwrap();
        let u = &out.planes[1];
        // U plane geometry: 4×4.
        assert_eq!(u.stride, 4);
        assert_eq!(u.data.len(), 16);
        // Row layout: tile 0 (chroma 2×2) at top-left, tile 1 right,
        // tile 2 below tile 0, tile 3 bottom-right (trimmed to 2×2 chroma
        // — same as a full tile because (3+1)/2 == 2).
        // Rows 0-1: [100,100,110,110] (tiles 0, 1).
        for row in 0..2 {
            assert_eq!(
                &u.data[row * 4..row * 4 + 4],
                &[100, 100, 110, 110],
                "top half of chroma U canvas wrong on row {row}"
            );
        }
        // Rows 2-3: [120,120,130,130] (tiles 2, 3) — bottom-right corner
        // must contribute its full chroma tile (chroma extents 2×2).
        for row in 2..4 {
            assert_eq!(
                &u.data[row * 4..row * 4 + 4],
                &[120, 120, 130, 130],
                "bottom half of chroma U canvas wrong on row {row}"
            );
        }
    }

    /// Round 21: 4:2:2 chroma subsampling — horizontal-only chroma
    /// halving. An output_w of 5 with tile_w=4 means tile 1 contributes
    /// 1 luma column; ceil(1/2) = 1 chroma column. Vertical chroma is
    /// not subsampled, so a 4:2:2 tile contributes its full row count.
    #[test]
    fn composite_yuv422_odd_width_chroma_edge() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 1,
            columns: 2,
            output_width: 5,
            output_height: 4,
        };
        // 4:2:2 tile: Y(4×4), U(2×4), V(2×4).
        let make_422 = |y, u, v| VideoFrame {
            pts: None,
            planes: vec![
                VideoPlane {
                    stride: 4,
                    data: vec![y; 16],
                },
                VideoPlane {
                    stride: 2,
                    data: vec![u; 8],
                },
                VideoPlane {
                    stride: 2,
                    data: vec![v; 8],
                },
            ],
        };
        let tiles = [make_422(10, 100, 200), make_422(20, 110, 210)];
        let out = composite_grid(&grid, &tiles, PixelFormat::Yuv422P, 4, 4).unwrap();
        let u = &out.planes[1];
        // Chroma plane: ceil(5/2)=3 cols × 4 rows.
        assert_eq!(u.stride, 3);
        assert_eq!(u.data.len(), 12);
        for row in 0..4 {
            assert_eq!(
                &u.data[row * 3..row * 3 + 3],
                &[100, 100, 110],
                "4:2:2 trailing chroma column lost on row {row}"
            );
        }
    }

    /// Round 21: clamp regression — when a tile happens to ship an
    /// undersized chroma plane (an encoder that rounded down rather
    /// than up), the composite must not walk off the source buffer.
    /// This reproduces the corner case: 4:2:0 tile with a 1-row chroma
    /// plane being asked to fill a 2-row chroma destination region.
    /// Expected behaviour: copy the available source row(s); leave the
    /// rest of the destination untouched (zero).
    #[test]
    fn composite_yuv420_undersized_source_chroma_safely_clamps() {
        let grid = ImageGrid {
            version: 0,
            flags: 0,
            rows: 1,
            columns: 1,
            output_width: 4,
            output_height: 4,
        };
        // Tile says 4×4 luma but its chroma plane only ships 1 row × 2
        // cols (instead of the spec-compliant 2×2). composite_grid
        // must clamp to that.
        let tile = VideoFrame {
            pts: None,
            planes: vec![
                VideoPlane {
                    stride: 4,
                    data: vec![10; 16],
                },
                VideoPlane {
                    stride: 2,
                    data: vec![100; 2], // 1 row only
                },
                VideoPlane {
                    stride: 2,
                    data: vec![200; 2],
                },
            ],
        };
        let out = composite_grid(&grid, &[tile], PixelFormat::Yuv420P, 4, 4).unwrap();
        let u = &out.planes[1];
        // Output U plane is 2×2; first row should be 100,100 (copied),
        // second row should be 0,0 (untouched, since source had no data).
        assert_eq!(u.stride, 2);
        assert_eq!(u.data.len(), 4);
        assert_eq!(&u.data[0..2], &[100, 100]);
        assert_eq!(&u.data[2..4], &[0, 0]);
    }

    /// `ceil_shift` matches `ceil(v / 2^shift)` for the practical
    /// range used by `composite_grid`: shift in {0, 1}.
    #[test]
    fn ceil_shift_matches_division_ceiling() {
        // shift = 0 is the identity.
        for v in [0u32, 1, 2, 3, 7, 100, 4096] {
            assert_eq!(ceil_shift(v, 0), v);
        }
        // shift = 1 is `(v + 1) / 2`.
        for v in 0..=33u32 {
            let want = v.div_ceil(2);
            assert_eq!(ceil_shift(v, 1), want, "ceil_shift({v}, 1)");
        }
        // shift = 2 (would arise for 4:1:0 — not currently emitted by
        // composite_grid, but the helper should still match).
        for v in 0..=33u32 {
            let want = v.div_ceil(4);
            assert_eq!(ceil_shift(v, 2), want, "ceil_shift({v}, 2)");
        }
    }
}
