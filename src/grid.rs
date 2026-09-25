//! AVIF grid-image composition (HEIF §6.6.2.3).
//!
//! A `grid` item stores a small payload declaring how many rows and
//! columns of tile items make up the picture, plus the final output
//! pixel dimensions. Each tile item is referenced from the grid via a
//! `dimg` iref entry. After every tile has been decoded the final
//! canvas is built by pasting the tiles in row-major order and cropping
//! to the declared output size.
//!
//! The descriptor and the pixel composition are the container crate's
//! ([`oxideav_heif::derived::GridDescriptor`],
//! [`oxideav_heif::compose::composite_grid`]); [`ImageGrid`] keeps this
//! crate's descriptor shape (with the raw `version` / `flags` bytes)
//! and [`composite_grid`] its frame model. All tiles must share the
//! same pixel format and tile size; mismatches return
//! `Error::InvalidData`.

use oxideav_heif::compose;
use oxideav_heif::derived::GridDescriptor;

use crate::error::{AvifError as Error, Result};
use crate::frame_bridge::{from_heif, to_heif, trim_top_left};
#[cfg(test)]
use crate::image::AvifPlane as VideoPlane;
use crate::image::{AvifFrame as VideoFrame, AvifPixelFormat as PixelFormat};

/// The `ImageGrid` descriptor (HEIF §6.6.2.3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageGrid {
    pub version: u8,
    pub flags: u8,
    pub rows: u16,
    pub columns: u16,
    pub output_width: u32,
    pub output_height: u32,
}

impl ImageGrid {
    /// Parse a `grid` item body.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        match payload.first() {
            None => return Err(Error::invalid("avif grid: payload 0 bytes < 8")),
            Some(0) => {}
            Some(v) => return Err(Error::invalid(format!("avif grid: version {v}"))),
        }
        let desc = GridDescriptor::parse(payload)?;
        Ok(Self {
            version: payload.first().copied().unwrap_or(0),
            flags: payload.get(1).copied().unwrap_or(0),
            rows: desc.rows,
            columns: desc.columns,
            output_width: desc.output_width,
            output_height: desc.output_height,
        })
    }

    /// `rows × columns`.
    pub fn expected_tile_count(&self) -> usize {
        (self.rows as usize) * (self.columns as usize)
    }
}

/// Stitch `tiles` (row-major, all `tile_w × tile_h` in `format`) into
/// the grid's `output_width × output_height` canvas, trimming the right
/// and bottom edges (§6.6.2.3.1). Tile layouts carrying alpha are not
/// grid inputs in this crate (the alpha of a grid rides on the grid
/// item, see the crate README).
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
    if grid.output_width == 0 || grid.output_height == 0 {
        return Err(Error::InvalidData(
            "avif grid: output dimensions zero".to_string(),
        ));
    }
    if (tile_w as u64) * (grid.columns as u64) < grid.output_width as u64
        || (tile_h as u64) * (grid.rows as u64) < grid.output_height as u64
    {
        return Err(Error::InvalidData(format!(
            "avif grid: {}×{} tiles of {}x{} don't cover {}x{}",
            grid.rows, grid.columns, tile_w, tile_h, grid.output_width, grid.output_height
        )));
    }
    if format.has_alpha() {
        return Err(Error::unsupported(format!(
            "avif grid: pixel format {format:?} not supported as a tile layout"
        )));
    }
    let planes = format.plane_count();
    if tiles[0].planes.len() != planes {
        return Err(Error::InvalidData(format!(
            "avif grid: format {:?} expects {} planes, got {}",
            format,
            planes,
            tiles[0].planes.len()
        )));
    }
    let depth = format.bit_depth();
    let mut heif_tiles = Vec::with_capacity(tiles.len());
    for (i, t) in tiles.iter().enumerate() {
        heif_tiles.push(
            to_heif(t, format, depth, tile_w, tile_h)
                .map_err(|e| Error::invalid(format!("avif grid: tile {i}: {e}")))?,
        );
    }
    let composed = composite_grid_frames(grid, &heif_tiles)?;
    let (mut out, _) = from_heif(&composed)?;
    out.pts = tiles[0].pts;
    Ok(out)
}

/// Grid composition on container frames: the tiles are stitched by the
/// container onto the full `columns × tile_w` by `rows × tile_h`
/// canvas, then this crate trims to `output_width × output_height`
/// with ceiling chroma extents — an odd 4:2:0 / 4:2:2 output keeps the
/// layout AV1 codes it in (chroma covering the last luma column / row)
/// instead of being promoted to 4:4:4 by an odd trim.
pub(crate) fn composite_grid_frames(
    grid: &ImageGrid,
    tiles: &[oxideav_heif::HeifFrame],
) -> Result<oxideav_heif::HeifFrame> {
    let first = tiles
        .first()
        .ok_or_else(|| Error::invalid("avif grid: empty tile list"))?;
    let full_w = u64::from(first.width) * u64::from(grid.columns);
    let full_h = u64::from(first.height) * u64::from(grid.rows);
    if full_w < u64::from(grid.output_width) || full_h < u64::from(grid.output_height) {
        return Err(Error::invalid(format!(
            "avif grid: {}×{} tiles of {}x{} don't cover {}x{}",
            grid.rows,
            grid.columns,
            first.width,
            first.height,
            grid.output_width,
            grid.output_height
        )));
    }
    let (full_w, full_h) = (
        u32::try_from(full_w).map_err(|_| Error::invalid("avif grid: canvas width overflow"))?,
        u32::try_from(full_h).map_err(|_| Error::invalid("avif grid: canvas height overflow"))?,
    );
    let full = GridDescriptor {
        rows: grid.rows,
        columns: grid.columns,
        output_width: full_w,
        output_height: full_h,
    };
    let composed = compose::composite_grid(&full, tiles)?;
    trim_top_left(&composed, grid.output_width, grid.output_height)
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
        // must refuse the malformed plane up front — never read past it.
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
        let err = composite_grid(&grid, &[tile], PixelFormat::Yuv420P, 4, 4).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)), "{err}");
    }

    /// HBD grid stitch: 2×2 grid of 10-bit 4:2:0 tiles (16-bit LE
    /// words) pastes whole words at byte-doubled offsets — luma and
    /// chroma planes both land at the right canvas positions.
    #[test]
    fn composite_2x2_grid_yuv420_10bit() {
        let mk_plane = |w: u32, h: u32, fill: u16| {
            let mut data = Vec::with_capacity((w * h) as usize * 2);
            for _ in 0..w * h {
                data.extend_from_slice(&fill.to_le_bytes());
            }
            VideoPlane {
                stride: (w as usize) * 2,
                data,
            }
        };
        let make_tile = |y: u16, u: u16, v: u16| VideoFrame {
            pts: None,
            planes: vec![mk_plane(2, 2, y), mk_plane(1, 1, u), mk_plane(1, 1, v)],
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
            make_tile(600, 100, 200),
            make_tile(700, 110, 210),
            make_tile(800, 120, 220),
            make_tile(900, 130, 230),
        ];
        let out = composite_grid(&grid, &tiles, PixelFormat::Yuv420P10Le, 2, 2).unwrap();
        let words = |p: &VideoPlane| -> Vec<u16> {
            p.data
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect()
        };
        let y = &out.planes[0];
        assert_eq!(y.stride, 8, "byte stride = 2 × canvas width");
        let yw = words(y);
        assert_eq!(&yw[0..4], &[600, 600, 700, 700]);
        assert_eq!(&yw[8..12], &[800, 800, 900, 900]);
        let u = words(&out.planes[1]);
        assert_eq!(u, vec![100, 110, 120, 130]);
        let v = words(&out.planes[2]);
        assert_eq!(v, vec![200, 210, 220, 230]);
    }
}
