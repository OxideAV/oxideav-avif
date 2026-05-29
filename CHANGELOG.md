# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Round 182 — av1-avif v1.2.0 §2.1 "The AV1 Image Item Data shall have
  exactly one Sequence Header OBU" container-layer compliance audit.
  New API `oxideav_avif::derived::audit_sequence_header_obu(meta, file)`
  enumerates every `'av01'` image item, resolves its payload via
  `iloc`, walks the OBU framing per AV1 §5.3.1 / §5.3.2 (header byte
  + leb128 `obu_size` per §4.10.5; optional one-byte extension header
  when `obu_extension_flag == 1` per §5.3.3) and counts OBUs whose
  `obu_type` equals `OBU_SEQUENCE_HEADER == 1` (per AV1 §6.2.1's
  `obu_type` enumeration). One `SequenceHeaderObuAudit { item_id,
  sequence_header_count, total_obu_count, missing_iloc, truncated_obu,
  has_size_field_zero, is_compliant(), missing() }` record per av01
  item, in `iinf` declaration order. The OBU payload bodies themselves
  are not decoded — only the type field and framing are inspected.
- `AvifInfo::sequence_header_obu_compliance:
  Vec<crate::derived::SequenceHeaderObuAudit>` populated by both the
  single-item and grid `build_info` paths, plus
  `AvifInfo::sequence_header_obu_strict_compliant()` predicate folding
  every record into a single boolean (trivially `true` when no av01
  items are present — degenerate, since AVIF requires the primary
  item be an av01 or a derivation rooted on av01s).
- 14 new tests: 11 unit tests in `derived::tests` covering the happy
  path (one SH OBU → compliant), §2.1 violations (zero SH OBUs flagged
  `av01-item-missing-sequence-header-obu`; two SH OBUs flagged
  `av01-item-multiple-sequence-header-obus`), structural failures
  (truncated payload past declared `obu_size`, truncated leb128
  mid-OBU, `obu_has_size_field == 0` chaining failure, missing iloc),
  the extension-header skip path (`obu_extension_flag == 1`), one
  record per av01 item ordering, and non-av01 item filtering; 3 unit
  tests covering the `read_leb128` helper directly
  (single/multi/maximum-width valid values, truncated continuation,
  overlong 8-byte cap from AV1 §4.10.5). 2 new integration tests pin
  the audit on real fixtures: `monochrome.avif` (one `'av01'` item, SH
  count == 1, strict-compliant) and `bbb_alpha_inverted.avif` (two
  `'av01'` items — colour primary + alpha auxiliary — each with SH
  count == 1, strict-compliant).
- `oxideav_avif::SequenceHeaderObuAudit` and
  `oxideav_avif::audit_sequence_header_obu` re-exported at the crate
  root. `build_info` signature extended with a trailing `file: &[u8]`
  argument; `build_info_grid` reuses the `hdr.file` slice it already
  carries.

- Round 176 — av1-avif v1.2.0 §4.1 Auxiliary-Image bit-depth match
  audit. The §4.1 `shall` "An AV1 Alpha Image Item (respectively an
  AV1 Alpha Image Sequence) shall be encoded with the same bit depth
  as the associated master AV1 Image Item (respectively AV1 Image
  Sequence)" is now validated at the container layer via
  `oxideav_avif::derived::audit_alpha_bit_depth(&Meta)`, returning one
  `AlphaBitDepthAudit { alpha_item_id, master_item_id,
  alpha_bit_depth, master_bit_depth, alpha_missing_av1c,
  master_missing_av1c, is_compliant(), missing() }` record per
  `(alpha, master)` pairing declared by an `'auxl'` iref whose source
  carries an `'auxC'` URN starting with the alpha prefix
  (`urn:mpeg:mpegB:cicp:systems:auxiliary:alpha`). A single alpha
  attached to multiple masters emits one record per master in
  `to_ids` order; iref entries are processed in declaration order.
- Bit depth is decoded directly from each item's `av1C` flag byte
  (`8`, `10`, or `12`) via a new private `decode_av1c_bit_depth`
  helper. The audit also surfaces two §2.1 violations that would
  defeat the §4.1 check: missing `av1C` association
  (`{alpha,master}_missing_av1c`) and truncated `av1C` payload
  (decoded depth is `None` with the missing flag still false).
- `AvifInfo::alpha_bit_depth_compliance:
  Vec<crate::derived::AlphaBitDepthAudit>` populated by both the
  single-item and grid `build_info` paths, plus
  `AvifInfo::alpha_bit_depth_strict_compliant()` predicate folding
  every record into a single boolean (trivially `true` when no alpha
  auxiliaries present, so combine with `has_alpha` for a presence +
  compliance gate).
- 10 new unit tests in `derived::tests` covering: matching 8-bit
  pairing compliant; 10-bit master vs 8-bit alpha flagged with
  `alpha-master-bit-depth-mismatch`; 12-bit pairing compliant; alpha
  item missing `av1C`; master item missing `av1C`; truncated `av1C`
  surfaces as `alpha-item-av1C-truncated` distinct from missing;
  depth-map auxiliary (non-alpha URN) ignored; one alpha pointing at
  multiple masters emits one record per pairing; empty audit list
  when no alpha auxiliaries present; multiple distinct alpha
  auxiliaries in one file each emit their own record in iref order.
  `decode_av1c_bit_depth` separately covers 8/10/12 + truncation.
  2 new integration tests pin the audit on real fixtures:
  `monochrome_fixture_has_no_alpha_bit_depth_audit_records` confirms
  the no-alpha vacuum on the Microsoft monochrome fixture, and
  `bbb_alpha_fixture_alpha_master_bit_depth_match` confirms the
  end-to-end §4.1 compliant shape on `bbb_alpha_inverted.avif`
  (both alpha and master carry `av1C` and agree on bit depth).

- Round 176 — HEIF v1.2.0 §6.6.2.1 Identity Derived Image Item
  (`iden`) `shall`-level compliance audit. The HEIF §6.6.2.1
  constraints ("derived image item shall have no item body" and
  "`reference_count` for the `dimg` item reference of a `iden` derived
  image item shall be equal to 1") together with the crosscutting
  §6.6.1 `shall` ("number of `SingleItemTypeReferenceBoxes` with the
  box type `dimg` and with the same value of `from_item_ID` shall not
  be greater than 1") are now audited at the container layer via
  `oxideav_avif::derived::audit_iden_derivations(&Meta)`. Returns one
  `IdenCompliance { iden_item_id, dimg_reference_count,
  dimg_iref_count, has_item_body, source_item_id, is_compliant(),
  missing() }` record per `'iden'` item in `iinf` declaration order.
- `AvifInfo::iden_item_ids` enumerates every `'iden'` carrier; the
  paired `AvifInfo::iden_compliance: Vec<IdenCompliance>` reports the
  per-item audit result, and `AvifInfo::iden_strict_compliant()`
  folds the AND of every record (vacuously `true` when no iden items
  exist).
- Public re-exports of `audit_iden_derivations` and `IdenCompliance`
  from the crate root, mirroring the existing `audit_tone_map` /
  `audit_grid_derivations` access pattern.
- Test delta: +9 unit tests in `derived::tests::audit_iden_*`
  covering the happy path (no `iloc` entry); compliant zero-length
  extent; non-empty body flagged; zero `dimg` inputs; two inputs in
  one iref; multiple iref entries sharing the same `from_item_ID`;
  empty audit list when no iden items; multi-iden iinf-ordering;
  non-`dimg` irefs ignored. +1 integration
  (`monochrome_fixture_has_no_iden_audit_records`) pins the
  no-iden-item vacuum on the Microsoft monochrome conformance fixture.

- Round 172 — av1-avif v1.2.0 §7 General-constraints
  transformative-property audit for grid derivation chains. The §7
  `shall` "Transformative properties shall not be associated with items
  in a derivation chain that serves as an input to a grid derived image
  item" is now validated at the container layer via
  `oxideav_avif::derived::audit_grid_derivations(&Meta)`, returning one
  `GridDerivationAudit { grid_item_id, tile_item_ids, offenders,
  is_compliant(), offending_tile_ids() }` record per `'grid'` item in
  `iinf` declaration order. Each record lists the offending
  `(tile_item_id, property_kind)` pairs (transformative properties
  recognised: `'clap'`, `'irot'`, `'imir'`) attached to any tile in the
  grid's `'dimg'` derivation chain. Transformative properties on the
  grid item itself are explicitly permitted by §7 and don't surface.
- `AvifInfo::grid_derivation_compliance:
  Vec<crate::derived::GridDerivationAudit>` populated by both the
  single-item and grid `build_info` paths, plus
  `AvifInfo::grid_derivations_strict_compliant()` predicate folding
  every record into a single boolean (trivially `true` when no grid
  items present, so combine with `is_grid` for a presence + compliance
  gate).
- 7 new unit tests in `derived::tests` covering: clean derivation chain
  with grid-level `irot` (permitted by §7 — the audit must not flag the
  grid item itself); single tile carrying `irot` flagged as an
  offender; one tile carrying all three transformative kinds emits
  three offender entries in stable `(clap, irot, imir)` order; two
  tiles offending in different ways with the unique-tile-id list
  collapsing duplicates; empty audit list when no grid items present;
  multi-grid file producing one record per grid in `iinf` order; grid
  without a `dimg` iref is vacuously compliant. 2 new integration tests
  pin the audit end-to-end: `synthetic_4x1_strip_passes_grid_
  derivation_audit` confirms the 4-tile-clean shape through `inspect`,
  and `monochrome_fixture_has_no_grid_derivation_audit_records` pins
  the no-grid-item shape on the Microsoft monochrome conformance
  fixture.

- Round 130 — Tone Map Derived Image Item (`tmap`) compliance audit
  (av1-avif v1.2.0 §4.2.2). The HEIF-defined `tmap` descriptor body
  parse is intentionally out of scope (the only HEIF edition currently
  shipped in `docs/image/heif/` is the 2017 first edition which
  predates `tmap`); what av1-avif §4.2.2 *does* normatively require
  independently of the body is two file-shape `should` constraints
  this round audits:
  1. The `tmap` item and its base image item (input `0` of the tmap's
     `'dimg'` iref) should be grouped together by an `'altr'` entity
     group so legacy readers still see a valid alternate.
  2. Each gain-map input image item (`to_ids[1..]` of the same iref)
     should be a HEIF [hidden image item][HEIF §6.4.2] (`infe` flags
     low bit set) so it's never surfaced directly.
  New surface: `derived::ToneMapCompliance` struct (per-item record),
  `derived::audit_tone_map(&Meta)` walker, plus
  `AvifInfo::tone_map_compliance: Vec<ToneMapCompliance>` populated in
  both the single-item and grid `build_info` paths, with a summary
  `AvifInfo::tone_map_strict_compliant()` predicate.
- `ItemInfo` now retains the 24-bit `infe` FullBox `flags` field
  (previously discarded). New `ItemInfo::is_hidden()` helper exposes
  the HEIF §6.4.2 hidden-image bit (`(flags & 0x01) == 0x01`).
- 8 new unit tests in `derived::tests` covering: full happy-path
  pairing (one tmap + base + `altr`); compliance with a hidden gain
  map; both-failures path (no `grpl` + visible gain map) surfacing
  both `missing()` strings; `altr` group missing the tmap id;
  tmap with no `dimg` iref at all; empty audit list when no tmap
  items present; multiple tmap items returned in `iinf` declaration
  order; `ItemInfo::is_hidden` low-bit semantics across the 24-bit
  flag space.

- Round 127 — Sample Transform Derived Image Item (`sato`) descriptor
  parser + evaluator (av1-avif v1.2.0 §4.2.3). Container-level only,
  no AV1 decode dependency. The descriptor is decoded with
  `oxideav_avif::derived::SampleTransform::parse(payload,
  reference_count)`; the strict parser enforces every spec assertion
  (`66976029` non-zero `token_count`, `1f569fa5` sample-index ≤
  `reference_count`, `989adc85` postfix order, `98b07e13` unary stack
  pre-condition, `75c5cbbc` binary stack pre-condition, `bac41e3a`
  single-element terminal stack, reserved-token rejection per
  §4.2.3.3). A relaxed counterpart (`parse_relaxed`) surfaces reserved
  tokens as `Token::Reserved(u8)` for diagnostic dumps. The full
  operator table is implemented: unary `negation` / `abs` / `not` /
  `bsr` (Table 2 rows 64..=67), binary `sum` / `difference` /
  `product` / `quotient` / `and` / `or` / `xor` / `pow` / `min` /
  `max` (rows 128..=137), `Constant` (row 0) with bit-depth-keyed
  field width (1 / 2 / 4 / 8 bytes for `bit_depth` 0..=3 per Table
  1), and `Sample(n)` (1-based input index). `SampleTransform::
  evaluate(&inputs)` walks the postfix expression to produce one
  output sample value; intermediate arithmetic saturates at i64 then
  clamps to the `num_bits` precision per §4.2.3.3 underflow / overflow
  rule. Composition into a reconstructed image is deferred until
  `oxideav-av1` exposes a decoder again.
- New `meta::ITEM_TYPE_SATO` + `meta::ITEM_TYPE_TMAP` four-CC
  constants and a generic `Meta::item_ids_of_type(&four_cc)` walker
  for enumerating derived-image carriers by type.
- `AvifInfo` surfaces `sato_item_ids: Vec<u32>` + `tmap_item_ids:
  Vec<u32>` populated by both the single-item and grid `build_info`
  paths, with `has_sample_transform()` / `has_tone_map()` predicates
  for callers that only need a presence gate. The Tone Map carrier
  side parses the item-type four-CC only; the HEIF-defined `tmap`
  descriptor body parse is a follow-up.
- 21 new unit tests in `derived::tests` covering: round-trip parse +
  evaluation at every `bit_depth` (0..=3 → 8/16/32/64-bit
  intermediate); two-sample postfix sum and difference (right-pop-
  first ordering verified); the av1-avif Appendix A
  MSB/residual recombination example
  (`Sample(1) Const(2) Const(8) pow product Sample(2) sum` =
  `(msb << 8) | residual`); unary negation; unary `bsr` (0 for
  `L <= 0`, `truncate(log2(L))` for `L > 0`); quotient with `R == 0`
  returning `L`; pow with `L == 0` returning `0`; min / max; rejection
  of `token_count = 0`, non-zero `version`, sample index >
  `reference_count`, every reserved-byte range (33..=63, 68..=127,
  138..=255), binary op with insufficient operands, expression with
  leftover stack, truncated token stream, truncated constant payload;
  Token classification helpers; min/max value per bit-depth; and the
  graceful error path when `evaluate` receives fewer inputs than the
  expression dereferences. 2 new integration tests build a synthetic
  AVIF with an `av01` primary + `sato` derived item linked by `dimg`
  and exercise the full pipeline: `inspect` returns the right
  `sato_item_ids`, `item_payload_bytes` resolves the descriptor body
  through `iloc`, and `SampleTransform::parse` round-trips the
  identity (`Sample(1)`) expression. The companion "no sato in
  typical files" test pins the Microsoft monochrome fixture's
  baseline.

- Round 123 — AV1 layered-image item properties + essential-property
  enforcement (av1-avif §2.3.2 + MIAF §7.3.5). Container-level box work,
  no AV1 decode dependency:
  - `a1op` OperatingPointSelectorProperty parser (av1-avif §2.3.2.1) —
    bare `ItemProperty` carrying a single `unsigned int(8) op_index`.
    New `meta::A1op { op_index }` type. The spec mandates this property
    be marked essential.
  - `a1lx` AV1LayeredImageIndexingProperty parser (av1-avif §2.3.2.3) —
    `unsigned int(7) reserved + unsigned int(1) large_size` byte then
    three `(large_size+1)*16`-bit `layer_size` values. New
    `meta::A1lx { large_size, layer_size: [u32; 3] }` with a
    `documented_layers()` helper that counts the leading non-zero run
    (= number of layers minus one).
  - Both routed through `Property::A1op` / `Property::A1lx` (previously
    fell into `Property::Other`) and surfaced on `AvifInfo` as
    `operating_point: Option<A1op>` / `layered_index: Option<A1lx>`,
    resolved on the primary item for both single-item and grid paths.
  - Essential-property enforcement: `Meta::unsupported_essential_properties`
    + `Meta::has_unsupported_essential_property` report any property that
    is marked essential (the `ipma` association high bit) yet lands in
    `Property::Other` — i.e. an essential property this crate cannot
    interpret. Per av1-avif §2.3.2.1.2 + MIAF §7.3.5 a reader must not
    process such an item. A recognised property (typed, even if only
    ignored downstream) and any non-essential unknown property do not
    block; a dangling association index that is essential does.
  - Tests: +8 unit (`a1op`/`a1lx` field-width round-trips, `ipco`
    dispatch, three essential-enforcement cases) + 3 integration
    (synthetic single-item AVIF carrying `a1op`/`a1lx` surfaced through
    `inspect`, the negative no-props path on the mono fixture, and an
    essential-but-recognised `a1op` not blocking the item).

- Round 81 — derived-image + entity-grouping + MIAF compliance. Container
  side gains a coordinated batch of HEIF surface that doesn't need the
  AV1 decoder (oxideav-av1 is a `NotImplemented` scaffold post the
  2026-05-20 orphan rebuild):
  - `auxC` URN classification (`AuxKind { Alpha, DepthMap, HdrGainMap,
    Other }`) covering MPEG and HEVC-HEIF URN spellings plus Apple's
    HDR gain-map URN. `Meta::aux_items_for` enumerates every aux item
    attached to a given target; `AvifInfo` adds `aux_items`,
    `alpha_aux_kind`, `depth_map_item_id`, `hdr_gain_map_item_id`,
    `has_depth_map()`, `has_hdr_gain_map()`.
  - `rloc` relative-location property parser (HEIF §6.5.7) — FullBox
    with two big-endian u32 offsets.
  - `lsel` layer-selector property parser (HEIF §6.5.11) — ItemProperty
    (no FullBox) with one u16 layer_id.
  - `iovl` image-overlay descriptor parser (HEIF §6.6.2.2) in the new
    `derived` module. Handles both 16-bit and 32-bit field-width
    variants (`flags & 1`) and signed offsets per spec; emits
    `ImageOverlay { canvas_fill_value, output_*, entries: Vec<OverlayEntry> }`.
  - Entity-grouping (`grpl`) parser (HEIF §9.4) — `parse_grpl` walks
    a `GroupsListBox` payload into typed `EntityGroup` per
    `EntityToGroupBox`, with `is_alternates()` / `is_stereo_pair()` /
    `is_equivalence()` helpers. `Meta` captures the raw `grpl` slice
    during walk; `Meta::groups()` lazy-parses on demand.
  - `audit_mif1` brand-compliance audit (HEIF §10.2.1.1) returning a
    `Mif1Compliance { is_compliant(), missing(), claims_mif1, ... }`.
    `AvifInfo.mif1_compliance` carries the audit alongside
    `is_strict_mif1()`. Pinned against the Microsoft monochrome
    fixture (fully compliant) plus a synth ftyp-only no-meta input.
  - `Meta` exposes raw `grpl` + `idat` slices for downstream routing
    of entity groups and item-data-bearing derived items.

### Notes

- Workspace-local builds (when the umbrella `[patch.crates-io]` table
  resolves `oxideav-av1` to the orphan-rebuilt master) currently fail
  the registry-gated build because the rebuilt av1 crate is a
  `NotImplemented` scaffold with no `Av1CodecConfig` / `Av1Decoder`.
  CI for this repo checks out the avif crate alone and pulls
  `oxideav-av1` from crates.io (currently 0.1.8, pre-rebuild), so the
  registry path keeps building + testing through CI. Resolution
  arrives when the av1 clean-room ships its decoder; until then the
  consumer-must-wait-for-publisher pattern in the workspace memory
  applies.
- `tests/integration.rs` graceful-skip predicate accepts the future
  `oxideav-av1` "decoder unavailable" / `NotImplemented` shape
  alongside the existing coded_lossless / §7.7.4 limitation so that
  when av1 0.2.x publishes and the registry path starts returning
  the new error string, end-to-end decode tests still graceful-skip
  rather than failing.

- Round 75 — HEIF item properties + iref typed-relationship enumeration.
  Container side pushes further into the descriptive surface around the
  primary AV1 OBU stream:
  - `ItemInfo` carries optional `content_type`, `content_encoding`,
    and `item_uri_type` populated from the tail of an `infe` v2/v3
    box for `item_type == 'mime'` and `item_type == 'uri '` per
    ISO/IEC 14496-12 §8.11.6.2. Generic item types stop after
    `item_name` so the common path stays compact.
  - `Meta::iref_sources_of(&BoxType, u32) -> Vec<u32>` walks every
    iref of a given reference_type whose `to_ids` contains the
    target — needed because a primary may have multiple thumbnails
    or be linked from multiple metadata items.
  - `Meta::is_alpha_premultiplied_for(u32) -> bool` checks for a
    HEIF `prem` iref linking an alpha auxiliary to the colour image
    per ISO/IEC 23008-12 §6.10.1.1.
  - `AvifInfo` gains `thumbnail_item_ids: Vec<u32>`,
    `exif_item_id: Option<u32>`, `xmp_item_id: Option<u32>`, and
    `premultiplied_alpha: bool`. Helpers: `has_thumbnails()`,
    `has_descriptive_metadata()`. Exif detection accepts native
    `item_type == 'Exif'` AND the libheif-style `mime` carrier with
    `application/octet-stream` / `image/tiff` / `image/x-exif`
    content_type. XMP is detected via `mime` +
    `application/rdf+xml` (case-insensitive).
  - `item_payload_bytes(file, item_id) -> Result<Vec<u8>>`:
    public extractor that wraps `item_bytes_owned` so a caller with
    a populated `AvifInfo` can pull the Exif TIFF / XMP RDF/XML
    blob out in one call.
  - Public constants exposed: `ITEM_TYPE_AV01`, `ITEM_TYPE_GRID`
    (re-exported from `parser`), `ITEM_TYPE_EXIF`, `ITEM_TYPE_MIME`,
    `ITEM_TYPE_URI` (in `meta`).
  - New tests: `infe_v2_mime_parses_content_type_and_encoding`,
    `infe_v3_mime_octet_stream_for_exif`,
    `infe_v2_uri_parses_uri_type`,
    `infe_v2_generic_item_type_stops_at_name`,
    `iref_sources_of_returns_all_matches`,
    `is_alpha_premultiplied_for_detects_prem_iref` (meta.rs);
    `inspect_fixture_resolves_native_exif_metadata_item` plus six
    `resolve_metadata_*` cases (inspect.rs). Lib test count
    104 → 118.

- Fuzz-driven hardening pass at the AVIF→AV1 boundary (workspace task
  #730). Adds defensive validation that refuses adversarial input
  before it reaches the AV1 decoder's entropy / transform stages,
  guarding against the arithmetic-overflow class of crashes the
  daily fuzz workflow surfaced through round 46:
  - New `validate_av1_config` rejects an `av1C` record whose
    `seq_profile > 2` (AV1 §A.4 reserved), whose `seq_level_idx_0`
    falls in the reserved 24..=30 range (AV1 §A.3), whose
    `monochrome` flag is set without both chroma-subsampling bits
    (AV1 §5.5.2 requires 4:0:0 to set both), whose 4:2:2 chroma
    declaration appears outside `seq_profile = 2` (AV1 §5.5.2), or
    whose 4:4:4 chroma declaration appears in `seq_profile = 0`
    (AV1 §5.5.2). Six unit tests cover each rejection plus the
    canonical 4:2:0 / profile-0 acceptance case.
  - `decode_av01_item` + `decode_avis_file` enforce a 32 MiB soft
    cap on the AV1 OBU payload they will hand to the AV1 decoder.
    Real-world AVIF items stay well under this; the cap protects
    against pathological inputs that would dominate the fuzz wall
    clock without surfacing useful crashes.
  - `infer_av1_pixmap` swaps the `u.stride * 2` debug-overflowable
    multiplication for `saturating_mul`, and now refuses a zero
    U-plane stride explicitly (AV1 §7.3.1 requires positive plane
    strides on every decoded frame).
- `oxideav-avif::avis::sample_table` enforces a soft cap of
  16 Mi expanded samples to defend against `stsc` entries whose
  `samples_per_chunk` field has been inflated to `0xFFFF_FFFF` —
  without this guard the per-chunk expansion loop ran for hours
  (ISO/IEC 14496-12 §8.7.4 doesn't bound the field, but real AVIS
  streams stay 5 orders of magnitude below the cap). Unit test
  `sample_table_rejects_oversized_stsc_expansion` pins the path.
- Defensive arithmetic across the box walker:
  `parse_box_header` / `read_u16` / `read_u32` / `read_u64` now
  use `checked_add` for every offset computation and refuse
  `usize::MAX`-adjacent positions instead of debug-panicking
  (ISO/IEC 14496-12 §4.2 box-size invariants). Two new unit tests
  in `box_parser`: `rejects_offset_overflow` and
  `read_u32_rejects_overflow_offset`.
- New regression-test crate
  (`crates/oxideav-avif/tests/fuzz_regressions.rs`) anchored on
  three real AVIF bitstreams captured from the daily fuzz workflow
  (`y_plane_divergence_match.avif`, `y_plane_roundtrip_avif1.avif`,
  `y_plane_roundtrip_avif2.avif`). The tests assert decode does
  not panic; pixel correctness remains the cross-decode harness's
  responsibility (the residual Y-plane divergence is tracked as
  workspace task #786 in `oxideav-av1`). Fourth test pins the
  malformed-av1C high-`seq_profile` rejection from the validator
  pass above.
- AVIS (AVIF Image Sequence) end-to-end decode pipeline
  (`AvifDecoder::decode_avis_file`). Walks the track's sample table
  via the existing [`avis::parse_avis`] surface, lifts the
  `AV1CodecConfigurationRecord` from `stsd` → `av01` → `av1C` (new
  field `AvisMeta::av1_codec_config`), and fans every sample through
  a single shared [`oxideav_av1::Av1Decoder`] so inter-frame
  prediction state is preserved across samples (when av1 supports
  it). Each successfully decoded sample is queued on the
  `pending` buffer with a `pts` derived from the cumulative `stts`
  duration so `Decoder::receive_frame` returns frames in
  presentation order.
- `AvifDecoder::send_packet` now dispatches to the sequence path
  automatically when the brand classification surfaces `is_sequence`
  (`avis`) or `has_msf1` (`msf1`) and the file carries a `moov`. The
  still-image path remains the fallback when the sequence claim is
  bogus (no `moov` present), so a misbranded file is still decoded.
- New AVIS-decode integration tests:
  `avis_decode_dispatches_to_sequence_path`,
  `decode_avis_file_returns_frame_count_or_propagates_av1_error`,
  plus three unit tests for the new `find_av1c_in_stbl` helper
  (round-trip on a synthesized stsd→av01→av1C chain, missing-av01
  guard, truncated av01 payload guard) and one fixture-driven test
  (`alpha_video_avis_exposes_av1c`).

### Changed

- Integration tests that previously called `AvifDecoder::send_packet`
  on lossless RED64 / GRAY32 / MIDGRAY64 / WHITE16 fixtures now
  tolerate the `Error::Unsupported(coded_lossless …)` path that
  oxideav-av1 returns until §7.7.4 IWHT dispatch + coefficient
  context derivation lands (workspace task #765). The transform-
  pipeline tests (`end_to_end_decode_then_irot_roundtrips`,
  `end_to_end_decode_then_imir_roundtrips`,
  `end_to_end_decode_then_clap_centre_crop`,
  `clap_with_zero_denominator_is_passthrough`) fall back to a
  deterministic synthetic 4:4:4 frame when av1 declines, so the
  pixel-permutation invariants they exercise still run end-to-end.
  No `#[ignore]` attribute added; the tests still execute and assert.

## [0.0.7](https://github.com/OxideAV/oxideav-avif/compare/v0.0.6...v0.0.7) - 2026-05-06

### Other

- drop dead `linkme` dep
- re-export __oxideav_entry from registry_glue sub-module
- HDR metadata + AV1 wrap pass-through + multi-extent iloc
- registry calls: rename make_decoder/make_encoder → first_decoder/first_encoder
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- unify entry point on register(&mut RuntimeContext) ([#502](https://github.com/OxideAV/oxideav-avif/pull/502))
- add register_containers for .avif / .avifs extension lookup

### Added

- r22: HDR metadata pass-through (`mdcv` / `clli` / `cclv` item
  properties). All three boxes are now parsed and surfaced through
  `AvifInfo`:
  - `mdcv` (`MasteringDisplayColourVolumeBox`, SMPTE ST 2086): display
    primaries (R/G/B) in chromaticity × 50000, white point, and max/min
    display luminance in 1/10000 cd/m² units. New `Mdcv` type in
    `meta.rs`.
  - `clli` (`ContentLightLevelBox`, ISO/IEC 14496-12 §12.1.5.4):
    MaxCLL + MaxFALL in cd/m². New `Clli` type.
  - `cclv` (draft av1-avif extension — same binary layout as `clli`).
    New `Cclv` type.
  - `AvifInfo` gains `mdcv: Option<Mdcv>`, `clli: Option<Clli>`,
    `cclv: Option<Cclv>`, plus helpers `has_hdr_metadata()`,
    `max_cll() -> Option<u16>`, `max_fall() -> Option<u16>`.
  - Grid primaries resolve HDR properties with the same fallback
    chain as `colr`/`pixi`/`pasp`: grid item first, tile 0 second.
  - New unit tests: `mdcv_round_trip`, `mdcv_rejects_truncated`,
    `clli_round_trip`, `clli_rejects_truncated`, `cclv_round_trip`,
    `cclv_rejects_truncated` (meta.rs); `inspect_sdr_fixture_has_no_hdr_metadata` (inspect.rs).

- r22: AV1 wrap pass-through — `bit_depth`, `monochrome`,
  `chroma_subsampling` decoded from `av1C` and stored on `AvifInfo`:
  - `bit_depth: Option<u8>` — 8 / 10 / 12 derived from
    `(high_bitdepth, twelve_bit)` flags in the `av1C` record. `None`
    when `av1c` is empty (< 3 bytes).
  - `monochrome: bool` — mirrors the `av1C` monochrome bit.
  - `chroma_subsampling: Option<(bool, bool)>` — `(subsampling_x,
    subsampling_y)` for colour streams; `None` for monochrome.
  - New `decode_av1c_flags()` internal helper (also tested directly).
  - New tests: `inspect_av1c_flags_decoded`,
    `decode_av1c_flags_hdr_bit_depths` (inspect.rs).

- r22: Multi-extent `iloc` item support — new public `item_bytes_owned`
  helper concatenates all extents for items that span more than one
  `iloc` extent entry (HEIF §8.11.3.3). The existing zero-copy
  `item_bytes` fast path is preserved for the common single-extent case.
  `item_bytes` now returns a descriptive `Unsupported` error for
  multi-extent items so callers know to use `item_bytes_owned`. New
  tests: `item_bytes_owned_single_extent_matches_item_bytes`,
  `item_bytes_owned_multi_extent_concatenates`,
  `item_bytes_owned_rejects_idat_method` (parser.rs).

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
