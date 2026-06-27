# oxideav-avif

Pure-Rust **AVIF** (AV1 Image File Format) container parser. Walks the
HEIF / ISOBMFF box hierarchy, resolves the primary item via `pitm` +
`iloc`, surfaces the `av1C` configuration record and the full HEIF
item-property / metadata family, and provides the grid / alpha /
post-transform composition layer that turns decoded AV1 planes into a
final frame. Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Status

The container layer is complete: the HEIF box walk, primary-item
resolution, item-property extraction, metadata items, image sequences
(AVIS), and the grid/alpha/transform composition logic all work and
are heavily tested.

**Pixel decode is currently stubbed.** Following the 2026-05-20
clean-room orphan rebuild of [`oxideav-av1`](https://github.com/OxideAV/oxideav-av1),
this crate no longer depends on it; the local `av1_stub` module parses
the `av1C` box structurally but returns `Error::Unsupported` for the
actual AV1 bitstream. `AvifDecoder::receive_frame` therefore surfaces
`Error::Unsupported` for real images. The hand-off will be re-wired
once the AV1 crate's rebuild lands a decoder usable from here. Standalone
callers can pair this crate's container surface with their own AV1
decoder today.

The encoder is **not implemented** (an AVIF encoder requires an AV1
encoder, which oxideav does not yet have); `make_encoder` returns
`Error::Unsupported`.

## Container coverage

| Stage | Coverage |
|-------|----------|
| `ftyp` brand check | accepts `avif` / `avis` / `mif1` / `msf1` / `miaf` |
| `meta` sub-boxes | `hdlr`, `pitm` (v0/v1), `iinf` (v0/v1) + `infe` (v2/v3), `iloc` (v0/v1/v2), `iref`, `iprp` / `ipco` / `ipma` (v0/v1, small + large property indices) |
| Item properties | `av1C`, `ispe`, `colr` (nclx + ICC), `pixi`, `pasp`, `irot`, `imir`, `clap`, `auxC`, `mdcv`, `clli`, `cclv`, `rloc`, `lsel`, `a1op`, `a1lx`, plus the HEIF §6.5 descriptive family (`iscl`, `rref`, `crtt`, `mdft`, `udes`, `altt`, `aebr`, `wbbr`, `fobr`, `afbr`, `dobr`, `pano`, `subs`, `tols`, `prdi`, the slideshow transition-effect set `wipe`/`zoom`/`fade`/`splt`/`stpe`/`ssld`, `cmex` camera-extrinsics (quaternion + position + rotation matrix; v1 ISO/IEC 23090-7 rotation struct out of scope), `cmin` camera-intrinsics, `sstr` single-stream (§6.5.38 bare marker on a derived item), plus the HEIF §6.10 text/font item-property family `txlo` text-layout (§6.10.2.1, `(flags&1)` 16/32-bit geometry + 8.8 font-size percent + TTML2 direction/writing-mode), `elng` extended-language (§6.10.2.2), `fnch` font-characteristics (§6.10.4.1, family/style/weight)), and the §11.2.2.2 mask-configuration property `mskC` (bits-per-pixel + §11.2.2 pixels-per-byte packing). Unknown boxes are retained as `Property::Other` so indices stay valid |
| Region items (`rgan`) | `region::RegionItem::parse` decodes a `'rgan'` region item's data into typed `RegionGeometry` variants — point / rectangle / ellipse / polygon / polyline / referenced-mask / inline-mask (HEIF §11.2.1, `(flags&1)` 16/32-bit fields, sign-extended coords); `inspect::region_items` / `region_items_for` enumerate the region items attached to the primary (or any image item) via the `'cdsc'` iref, resolving each one's data through the construction-method-aware `iloc` path into a `ResolvedRegionItem`. Derived region items (`drgn` §11.3.3) deferred |
| Essential-property enforcement | `Meta::{unsupported_essential_properties, has_unsupported_essential_property}` flag any `ipma`-essential property that lands in `Property::Other` (av1-avif §2.3.2.1.2 + MIAF §7.3.5) |
| Sample Transform (`sato`) | descriptor parser + per-sample evaluator (av1-avif §4.2.3); image composition deferred until a real AV1 decoder lands |
| Tone Map (`tmap`) | four-CC detection + §4.2.2 compliance audit + `GainMapMetadata::parse` (ISO 21496-1:2025 Annex C.2) + **§6 application**: `unnormalize_log2_gain` (Formula 1), `weight_factor` (Formula 3), `apply_component` / `apply_rgb` / `apply_plane_rgb` reconstruct the linear alternate (HDR) rendition `(Baseline + k_base)·2^(W·G) − k_alt` (Formula 2) from a linear baseline + the decoded gain plane, with §5.2.5.1 per-component-metadata broadcast and §6.3 NOTE 2 achromatic handling |
| AV1 layered properties | `a1op` operating-point selector + `a1lx` layered-image index (av1-avif §2.3.2) |
| Auxiliary classification | `auxC` URN routed to `Alpha` / `DepthMap` / `HdrGainMap` / `Other` |
| Derived images | `iovl` overlay + `iden` identity + `tmap` tone-map derivations resolved end-to-end (HEIF §6.3 / §6.6.2, av1-avif §4.2.2) via a box-graph geometry resolver — no AV1 decode: `transform_chain` / `output_dims_from_reconstructed` apply `irot`/`imir`/`clap`/`iscl` in `ipma` order (§6.3); `reconstructed_dims` resolves grid/iovl descriptor dims + recursive `iden` inputs + `tmap` base-input extents + `sato` own/`ispe`-or-input extents + coded `ispe` (cycle-guarded, depth 16; descriptor bytes resolve through `iloc` construction methods 0/1/**2**); `resolve_overlays` clips each `OverlayPlacement` against the canvas (§6.6.2.2.3); `resolve_iden_derivations` folds the iden's own transforms over its source; `resolve_tone_maps` resolves each `tmap`'s base/gain-map input ids + rendered (base) extents + per-gain-map coded extents, flagging gain-map up-sampling. Surfaced on `AvifInfo::{overlay_resolutions, iden_resolutions, tone_map_resolutions}`; `inspect()` accepts `iovl`/`iden`/`tmap`/`sato` derived primaries. Pixel composition still pending a decoder |
| Unified derivation graph (`build_derivation_graph` / `inspect::derivation_graph`) | one decode-free traversal (HEIF §6.6) that walks any derived primary into a `DerivationGraph` — every reachable node (`DerivationNode` { `DerivationKind` ∈ Coded/Grid/Overlay/Identity/ToneMap/SampleTransform/Unknown, reconstructed + output dims, depth }) plus the de-duplicated coded-`av01` leaf decode set in first-visit order. Handles **nested** derivations (iden-of-grid, tmap-over-grid) and **diamond** graphs (shared leaf listed once); iterative pre-order with a `MAX_DERIVATION_DEPTH` cycle guard (`truncated` flag). Accessors: `output_dims` / `root_is_coded` / `coded_leaf_dims` (decode-buffer sizing) / `nodes_at_depth` / `derived_node_count`. No pixel composition — the dependency planner a renderer feeds its AV1 decoder |
| Entity grouping (`grpl`) | typed `EntityGroup` per `EntityToGroupBox`; `altr` / `ster` / `eqiv` / `pano` / `prgr` progressive-rendering (§6.8.10) / `brst` burst (§6.8.9) / `msrc` multi-source recognised (HEIF §9.4) |
| Text / font items (`text` / `font`) | `inspect::text_items` / `text_items_for` enumerate the §6.10.1 text items (`'mime'` items linked to an image via the `'text'` iref) annotating the primary (or any image item), surfacing each one's `content_type` and the `'font'`-iref-linked font item ids as a `ResolvedTextItem`; `is_font_item` recognises a §6.10.3 font item (`'mime'` + `content_type` starting `font/`, RFC 8081) |
| Coded-item dependency roles | `inspect::coded_item_dependencies` classifies an image item from its outgoing item references into `CodedItemDependencies` { `pred` predictively-coded decoding-order list (§6.4.9), `base` pre-derived-coded inputs (§6.4.7), `exbl` scalable base-layer (§6.4.8), `tbas` tile-base relation } with `is_predictively_coded` / `is_pre_derived` / `has_dependencies` projections; backed by the new `Meta::iref_targets_of` outgoing-reference walker |
| Brand compliance audit | `audit_mif1` (HEIF §10.2.1.1); MIAF Baseline (MA1B) / Advanced (MA1A) profile dispatch + av1-avif §8.2/§8.3 profile audit |
| Metadata items | `cdsc` iref resolves Exif + XMP attached to the primary; raw bytes on demand via `item_payload_bytes` |
| Thumbnails | `thmb` iref enumeration via `AvifInfo::thumbnail_item_ids` |
| Premultiplied alpha | HEIF `prem` iref detected and surfaced |
| CICP colour signalling | `colr` nclx → `CicpTriple` with H.273 defaults; ICC + Unknown fall back to Unspecified |
| HDR metadata | `mdcv` (ST 2086), `clli` (MaxCLL/MaxFALL), `cclv`, `amve` ambient viewing environment (AVIF §6.5.36 / ISO/IEC 14496-12; 0.0001-lux illuminance + CIE 1931 ambient-light chromaticity, surfaced on `AvifInfo::amve`) |
| `av1C` introspection | bit depth (8/10/12), monochrome flag, chroma subsampling decoded into `AvifInfo` |
| Sequence Header OBU audit | av1-avif §2.1 "exactly one Sequence Header OBU" container-layer audit |
| Primary item data | resolved via `iloc` construction_method 0 (file-offset), **1** (idat-offset — bytes in the `meta` box's `idat` / ItemDataBox, ISO/IEC 14496-12 §8.11.3) **and 2** (item-offset — bytes are a range of another item's data, named via the `'iloc'` item reference; `extent_index` 1-based, 0 implies 1; `extent_length` 0 = whole referenced item, §8.11.3.3); single-extent cm=0 is a zero-copy slice, idat-backed or multi-extent items are concatenated, cm=2 resolves recursively (depth-capped, self-/cycle-rejecting). `item_bytes_with_idat` / `item_bytes_owned_with_idat` cover methods 0/1; `item_bytes_owned_full` (and `item_payload_bytes`) additionally follow cm=2. Grid tiles, the alpha auxiliary, and metadata items (`item_payload_bytes`: Exif / XMP / mime / `tmap`) are all construction-method-aware |
| Grid primary items | grid descriptor parse + `dimg` tile composition + av1-avif §7 derivation-chain audit + **decode-free tile-geometry resolution** (`resolve_grids` → `GridResolution`: common tile dims + per-tile row-major canvas placement, `GridTilePlacement::visible` right/bottom trim §6.6.2.3.1, `covers_canvas` / `trimmed_tile_count`); surfaced on `AvifInfo::{grid_resolutions, has_grid, grid_resolution_for}` |
| Alpha auxiliary | `auxl` + `auxC` detection + composition (`Gray8→YA8`, `Yuv→YuvA`) + av1-avif §4.1 bit-depth audit |
| Post-transforms | `clap` → `irot` → `imir`, applied in that order (HEIF §6.5.10) |
| AVIS image sequences | sample-table walk (`parse_avis` / `sample_table`) + `inspect_avis` aggregator + §3 / §8.2 / §8.3 audits + `edts/elst` edit list (ISO/IEC 14496-12 §8.6.6) + `mdhd` media-timescale plumb + `prft` ProducerReferenceTimeBox (§8.16.5, v0/v1 NTP→Unix, top-level walk) on `AvisMeta::producer_reference_times` + `ssix` SubsegmentIndexBox (§8.16.4, v0; per-subsegment `(level: u8, range_size: u24)` leva-level byte-range partitions for partial-subsegment access, top-level walk) on `AvisMeta::subsegment_indexes` |
| Sample grouping | `sbgp` (SampleToGroupBox, ISO/IEC 14496-12:2015 §8.9.2, v0/v1) + `csgp` (CompactSampleToGroupBox, :2020 §8.9.5 — 4/8/16/32-bit packed field widths, pattern expansion, `traf` fragment-local msb) + `sgpd` generic header (§8.9.3, v0/v1/v2 default index); per-sample group-index lookup via `SampleToGroup::group_index_for_sample`, surfaced on `AvisMeta::{sample_to_groups, sample_group_descriptions}` |

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-codec = "0.1"
oxideav-avif = "0.0"
```

The default-on `registry` feature pulls in `oxideav-core` and exposes
the `oxideav_core::Decoder` trait surface (`AvifDecoder`,
`make_decoder`, `register`, `make_encoder`). Build with
`default-features = false` for an `oxideav-core`-free container parser.

## Use

### Inspect an AVIF file without decoding

```rust
use oxideav_avif::inspect;

let bytes = std::fs::read("image.avif")?;
let info = inspect(&bytes)?;
println!("{}x{} bits_per_channel={:?} av1c_len={}",
    info.width, info.height, info.bits_per_channel, info.av1c.len());
println!("primary OBU stream is {} bytes", info.obu_bytes.len());
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Low-level: parse the container yourself

```rust
use oxideav_avif::parse;

let bytes = std::fs::read("image.avif")?;
let img = parse(&bytes)?;
for item in &img.meta.items {
    println!("item {} type={:?} name={:?}", item.id,
        std::str::from_utf8(&item.item_type), item.name);
}
// `img.primary_item_data` is the slice inside `mdat` holding the
// primary image's AV1 OBU stream. Pair it with `img.av1c` and your
// own AV1 decoder to recover pixels.
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Codec id

- Codec: `"avif"`; capability name declared to the registry is
  `avif_heif_av1_decode`.
- `CodecParameters::extradata` is the `av1C` byte record; width /
  height reflect `ispe` from the primary item.

## Test fixtures

`tests/fixtures/monochrome.avif` is `Monochrome.avif` from
[AOMediaCodec/av1-avif](https://github.com/AOMediaCodec/av1-avif/tree/main/testFiles/Microsoft)
(1280×720, monochrome). `tests/fixtures/{gray32,midgray,white16,red,black420}.avif`
are tiny reference-encoder-produced AVIFs covering each colour-plane
layout. Container-layer integration tests walk the full HEIF hierarchy
and extract the primary item; pixel-decode tests are gated on the AV1
rebuild.

## License

MIT — see [LICENSE](LICENSE).
