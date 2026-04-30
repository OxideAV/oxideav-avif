# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
