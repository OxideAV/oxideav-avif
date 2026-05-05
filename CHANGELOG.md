# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.6](https://github.com/OxideAV/oxideav-avif/compare/v0.0.5...v0.0.6) - 2026-05-04

### Other

- standalone retrofit follow-up: gate integration tests + diag_decode example on `registry`
- handle irot dim swap in decoder_pipes_through_av1_errors_cleanly
- rustfmt sweep on standalone-retrofit + fuzz round 2 commits
- fuzz round 2: pixel cross-validation + oxideav<->libavif re-encode roundtrip ([#304](https://github.com/OxideAV/oxideav-avif/pull/304))
- standalone retrofit: gate oxideav-core + oxideav-av1 behind `registry` feature
- cargo-fuzz harness with libavif as cross-decode oracle

### Fixed

- Standalone retrofit follow-up (#360). `cargo build/test/clippy
  --no-default-features` now actually walks every target. The
  `tests/integration.rs` suite is gated `#![cfg(feature = "registry")]`
  (every test wraps `AvifDecoder` + `oxideav_core::Decoder`), and the
  `examples/diag_decode.rs` example carries `required-features =
  ["registry"]` in `Cargo.toml` so cargo skips it cleanly when the
  feature is off. Without these gates the standalone build was green
  but `cargo test --no-default-features` failed to compile the
  integration target. `cargo tree --no-default-features` now shows
  zero transitive deps.

### Added

- Fuzz round 2 (#304). Two libavif-driven cross-validation harnesses
  added to `fuzz/`:
  - `libavif_encode_oxideav_libavif_decode_match` — encode with
    libavif lossless YUV444+IDENTITY, decode the resulting bitstream
    with BOTH `oxideav-avif` and `libavif`, assert pixels match
    plane-by-plane (Y=G, U=B, V=R per the IDENTITY-matrix lossless
    contract). Catches decoder divergences from the libavif
    reference.
  - `libavif_oxideav_reencode_roundtrip` — closest realisable
    approximation of the literal "self-roundtrip" task: oxideav
    decodes → libavif re-encodes the decoded pixels → oxideav decodes
    again → assert P₁ == P₂. Validates oxideav-avif's decoder is
    bit-stable across a re-encode by libavif.
  - The literal "fuzz-generated AVIF → decode → re-encode → decode
    again" of the task spec is blocked on an oxideav AVIF encoder
    (today `make_encoder` returns `Error::Unsupported` because
    oxideav lacks an AV1 encoder). Both harnesses skip silently when
    libavif isn't installed; the daily fuzz workflow installs
    `libavif-dev` so CI exercises the assertions.

### Added

- Standalone-friendly retrofit (#360 sweep). New default-on `registry`
  Cargo feature gates the `oxideav-core` + `oxideav-av1` dependencies
  plus the `oxideav_core::Decoder` trait surface (`AvifDecoder`,
  `make_decoder`, `register`, `register_with_av1`, `make_encoder`).
  With the feature off the crate exposes the HEIF box walker
  (`box_parser`, `meta`, `parser`), AVIS sample-table walker
  (`avis::parse_avis`), grid + alpha + transform composition layer
  (operating on crate-local `AvifFrame` / `AvifPixelFormat` /
  `AvifPlane`), `inspect::AvifInfo` + container-side colour helpers
  (`cicp::*`), and the `AvifError` / `Result` types — all without
  pulling either framework dep into the dep tree.
  - New crate-local types: `AvifError`, `AvifFrame`, `AvifPlane`,
    `AvifPixelFormat`. With `registry` enabled, `From` /
    `TryFrom` conversions to/from `oxideav_core::frame::VideoFrame` /
    `VideoPlane` / `PixelFormat` are exposed for callers that bridge
    both worlds.
  - Module split: `inspect.rs` carries the standalone container-side
    surface (`AvifInfo`, `inspect`, `transforms_for`,
    `build_info`, `build_info_grid`); `decoder.rs` keeps the
    registry-only AV1 + composition pipeline.
  - Inline `ci-standalone` job builds and tests the lib with
    `--no-default-features` so future regressions where a pipeline
    module re-grows an `oxideav-core` import fail CI.

## [0.0.5](https://github.com/OxideAV/oxideav-avif/compare/v0.0.4...v0.0.5) - 2026-05-03

### Other

- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- grid hardening — chroma tile-edge alignment + colr/pixi/pasp fallback
- round 20 — CICP color path
- round 19 — pixi/pasp helpers, grid hardening, AVIS sample bytes
- round 18 — MIAF brand validation + colr surface + imir/clap end-to-end tests
- round 17 — drop obsolete panic catch_unwind, add irot end-to-end + transforms_for tests
- adopt slim VideoFrame/AudioFrame shape
- pin release-plz to patch-only bumps

### Added

- r21: grid hardening for multi-tile MIAF AVIFs (HEIF §6.6.2.3 +
  av1-avif §4.2.1).
  - **Tile-edge chroma alignment** (`composite_grid`): chroma copy
    extents now use ceiling division of the trimmed luma copy
    extent, so a 4:2:0 grid whose right-most or bottom-most tile is
    clipped to an odd luma column / row count copies the full
    trailing chroma sample instead of dropping it. Example regression
    fixed: 4:2:0 grid with `tile_w=4`, `output_w=7` previously copied
    1 chroma column for the right tile (canvas needed 2). Same fix
    applies to 4:2:2 horizontal subsampling. Source-side and
    destination-side clamps added so a tile whose chroma plane is
    smaller than its luma-derived ceiling — or that overhangs the
    canvas edge — truncates safely.
  - **Grid `colr` / `pixi` / `pasp` resolution** (`build_info_grid`):
    every descriptive property now follows the same fallback chain —
    grid-item association first (canonical placement, describes the
    reconstructed canvas), tile-0 association second (the libheif
    writer pattern, OK because av1-avif §4.2 makes per-tile values
    uniform). Previously only `colr` had the fallback; `pixi` looked
    only at tile 0 and `pasp` only at the grid item.
  - New tests: `composite_yuv420_odd_width_copies_full_chroma_edge`,
    `composite_yuv420_odd_height_copies_full_chroma_edge`,
    `composite_yuv420_odd_both_axes_trims_corner_tile`,
    `composite_yuv422_odd_width_chroma_edge`,
    `composite_yuv420_undersized_source_chroma_safely_clamps`,
    `ceil_shift_matches_division_ceiling` (lib),
    `effective_cicp_grid_test`, `pixi_resolves_via_grid_then_tile_fallback`,
    `pasp_resolves_via_grid_then_tile_fallback`,
    `grid_tile_edge_geometry_round_trips` (integration).
- r20: CICP color signalling — `CicpTriple` quadruple
  `(primaries, transfer, matrix, full_range)` with ITU-T H.273
  defaults (`Unspecified = 2/2/2/false`) when `colr` is absent or
  ICC. Surfaced via `AvifInfo::effective_cicp()` and
  `effective_cicp(Option<&Colr>)`. Predicates: `is_unspecified`,
  `is_identity_matrix` (matrix=0 RGB), `is_libavif_srgb_default`
  ((1, 13, 6)), `has_reserved`. Name lookups: `primaries_name`,
  `transfer_name`, `matrix_name`. `CicpTriple::ALPHA` /
  `for_alpha()` reflects av1-avif §4.1 alpha-auxiliary defaults
  (`full_range = true`, others Unspecified).

### Notes

- AVIF readers must NOT apply colour transforms to decoded
  samples — av1-avif §4.2.3.1. The CICP path is signalling only.

## [0.0.4](https://github.com/OxideAV/oxideav-avif/compare/v0.0.3...v0.0.4) - 2026-04-25

### Added

- parse HEIF container + extract AV1 OBUs; hand off to oxideav-av1

### Other

- ignore decodes_flat_gray_to_mid_value pending av1 fix
- fix clippy 1.95 lints
- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- round-5 end-to-end decode gate — flat-content AVIFs decode
- phase 8 integration tests + conformance fixtures
- phase 8 — grid, alpha, transform, AVIS sample table
- bump oxideav-av1 dep to 0.1
- drop Cargo.lock — this crate is a library
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- bump to oxideav-core 0.1.1 + codec 0.1.1
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
