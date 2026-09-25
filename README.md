# oxideav-avif

[![CI](https://github.com/OxideAV/oxideav-avif/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-avif/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-avif.svg)](https://crates.io/crates/oxideav-avif) [![docs.rs](https://docs.rs/oxideav-avif/badge.svg)](https://docs.rs/oxideav-avif) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Pure-Rust **AVIF** (AV1 Image File Format): the AV1 profile of HEIF /
MIAF. The ISOBMFF / HEIF container — box reader, `meta` item model,
item payload resolution, derived-image graph, grid / overlay /
identity composition, alpha attachment, `clap` / `irot` / `imir`,
image-sequence tables and the still-image writer — is
[`oxideav-heif`](https://github.com/OxideAV/oxideav-heif); this crate
owns everything AV1-specific on top of it: `av1C` and the AV1 item /
sample handling, the av1-avif §2–§8 audits and profile brands, the
AVIF item layout of the muxer, `a1op` / `a1lx` / `lsel` layered items,
the HEIF-extension properties it types beyond the container's set,
`avis` sequences, HDR metadata authoring and the pixel encoder's tile
election. AV1 pixels come from
[`oxideav-av1`](https://github.com/OxideAV/oxideav-av1). Zero C
dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Layering

| Layer | Crate | What lives there |
|-------|-------|------------------|
| ISOBMFF boxes, `meta` tree, `iloc` (all construction methods), `iref`, `ipco` / `ipma`, `idat`, `grpl` | `oxideav-heif` (`HeifFile`, `Meta`, `boxes`) | `parse_header` / `parse` build this crate's `Meta` from the container's (`Meta::from_container`); `AvifHeader::heif` / `item_data` resolve item bytes |
| Typed properties | `oxideav-heif` (`props`) + this crate | `ispe` `pixi` `colr` `pasp` `clap` `irot` `imir` `iscl` `auxC` `clli` `amve` `rloc` `lsel` `a1op` `a1lx` `rref` `crtt` `mdft` `udes` `altt` converted from the container; `av1C` raw for the AV1 layer; `mdcv` / `cclv` read here; the HEIF-extension family (`aebr` … `cmin`, transitions, `prdi`, `sstr`, `txlo`, `elng`, `fnch`, `mskC`) parsed here from the raw property bytes |
| Composition | `oxideav-heif` (`compose`) | grid stitch, `iovl` painting (fill converted with the output's H.273 matrix), alpha attachment, `clap` → `irot` → `imir` in `ipma` order, MIAF §7.3.6.7 4:4:4 promotion; the decoder walks the graph on the container frame and re-lays the result out as this crate's `AvifFrame` |
| AVIF rules on top | this crate | av1-avif §4.1 same-depth alpha, nearest-neighbour alpha resize, `ispe` trim and grid trim with AV1's ceiling chroma (odd 4:2:0 stays 4:2:0), monochrome overlays stay monochrome, degenerate `clap` passes through, a 4:2:2 quarter turn is refused, `lsel` / `a1op` layer selection, the AV1 decode itself |
| Image sequences | `oxideav-heif` (`sequence::parse_movie`) + this crate | first-track sample table, `stsd` / `av1C`, `tkhd` / `mdhd` / `hdlr` / `elst` from the container; sample groups (`sbgp` / `csgp` / `sgpd`), `prft` / `ssix` and the §3 / §8 audits here |
| Writer | `oxideav-heif` (`HeifWriter`) + this crate | this crate decides brands, item order, property sets, `auxC` URNs and descriptors; the container serialises `meta` / `idat` / `mdat`. The sequence `moov` is still written here (the container's `SequenceWriter` has no brand override / cover aliasing) |

## Status

Parsing, derived-image composition, image sequences and the writer
ride the container crate; the AVIF-side surface below is complete and
heavily tested.

**Pixel decode is live at every AV1 bit depth.** The primary item's
AV1 OBU stream (and every AVIS sample) is handed to
[`oxideav-av1`](https://github.com/OxideAV/oxideav-av1)'s
conformance-grade spec-driver decoder; the composition layer then
stitches grid tiles, composites the alpha auxiliary (4:2:0 / 4:2:2 /
4:4:4 / monochrome masters), and applies `clap` / `irot` / `imir` —
at 8, 10 and 12 bits end to end. 10/12-bit output rides little-endian
16-bit word planes (`Yuv*P10Le` / `*P12Le`, `Gray10Le` / `Gray12Le`,
alpha-composited `Yuva*P1xLe`, and packed `Ya16Le` for HBD
monochrome + alpha with the effective depth on the core
significant-bits side channel). Every committed fixture decodes end
to end — including the 3840×2160 alpha composite and the multi-sample
AVIS sequence — and the encode→decode HBD matrix round-trips
sample-exact where lossless.

**Derived images decode to pixels.** The decoder resolves the
primary's *output image* per HEIF §6.3 for any item type —
recursively over `dimg` inputs, cycle- and depth-guarded: `grid`
tile stitch, `iovl` overlay composition (§6.6.2.2 canvas + offsets +
layering with the §6.9.1 straight / pre-multiplied alpha rendering,
8/10/12-bit, translucent fills yielding a `Yuva*` canvas), `iden`
identity derivations carrying their own `clap` / `irot` / `imir`, and
every item's transformative properties in `ipma` order. A grid
primary's alpha may be a coded item or a grid of monochrome tiles.
Layered (progressive) items honour `lsel` / `a1op` (av1-avif §2.3).

**Pixel encode is live** (`still` module). `StillImage` +
`encode_still` / `encode_still_grid` drive oxideav-av1's KEY-frame
encoder across the full (bit depth × chroma format) matrix — 8/10/12
bit × 4:2:0 / 4:2:2 / 4:4:4 / monochrome — plus RGB(A) via the H.273
identity matrix in 4:4:4 (byte-exact lossless round-trips), alpha as
a same-depth monochrome auxiliary (av1-avif §4.1, full-range
signalled in-stream), arbitrary extents (edge-replicated pad to the
coded 8-grid + top-left `clap`, `ispe` = coded extents per §2.2.2),
grid tiling with right/bottom trim (64-pixel tile floor + tiling
election, alpha as a hidden alpha grid, pass-through properties on
the grid item), `iovl` overlays (`encode_still_overlay`), `iden`
identity derivations (`StillProperties::identity_derivation`),
layered / progressive items (`encode_still_layered`: AV1 spatial
layers + `a1lx` + `lsel`), depth-map auxiliaries, and `avis` image
sequences (`sequence::encode_sequence`: all-intra or KEY + P groups,
full `moov` / `trak` / `stbl`). The `ftyp` profile brand
follows the elected AV1 profile (§8: Main → `MA1B`, High → `MA1A`,
Professional → general brands only). Lossless encodes decode back
sample-exact through this crate's own decoder; lossy encodes are
PSNR-gated in the test suite; black-box acceptance decodes the
emitted files through an external AVIF decoder binary where one is
installed. The registry `Encoder` (`make_encoder` / `AvifEncoder`)
rides the same pipeline: one video frame in, one complete AVIF file
packet out (`q` / `alpha_q` / `premultiplied` codec options).

The **container muxer** (`mux` module) remains available standalone:
given an already-coded AV1 Image Item Data payload plus its `av1C`
record, `AvifMuxer` / `AvifGridMuxer` / `AvifOverlayMuxer` /
`encode_still_av1` emit a conformant AVIF file — `ftyp`
(`avif`/`mif1`/`miaf` + `MA1B`/`MA1A`/no profile brand), the `meta`
tree serialised by `oxideav-heif`'s `HeifWriter` (derived-item bodies
in `idat`, MIAF `mdat` order, de-duplicated `ipco`) — with `ispe` /
`pixi` / `colr` / `pasp` / `clap` / `irot` / `imir` item properties, an
optional AV1-coded alpha auxiliary (`auxC` + `auxl`, plus `prem` from
the master when premultiplied, HEIF 3rd ed. §6.9.1), and optional
`grid` tiling (`dimg`). The output round-trips through this crate's
own `parse` path byte-for-byte (coded payload and every property) and
passes `audit_mif1`. At this layer the AV1 bitstream is taken
**black-box**.

## Container coverage

| Stage | Coverage |
|-------|----------|
| `ftyp` brand check | accepts `avif` / `avis` / `mif1` / `msf1` / `miaf` (this crate's rule, run before the container parse) |
| `meta` sub-boxes | by `oxideav-heif`: `hdlr`, `pitm` (v0/v1), `iinf` (v0/v1) + `infe` (v2/v3), `iloc` (v0/v1/v2, construction methods 0/1/2), `iref`, `iprp` / `ipco` / `ipma` (v0/v1, small + large property indices), `idat`, `grpl` — converted into this crate's `Meta` |
| Item properties | `av1C`, `ispe`, `colr` (nclx + ICC), `pixi`, `pasp`, `irot`, `imir`, `clap`, `auxC`, `mdcv`, `clli`, `cclv`, `rloc`, `lsel`, `a1op`, `a1lx` (the container-typed ones converted, `mdcv` / `cclv` read here), plus the HEIF §6.5 descriptive family parsed here (`iscl`, `rref`, `crtt`, `mdft`, `udes`, `altt`, `aebr`, `wbbr`, `fobr`, `afbr`, `dobr`, `pano`, `subs`, `tols`, `prdi`, the slideshow transition-effect set `wipe`/`zoom`/`fade`/`splt`/`stpe`/`ssld`, `cmex` camera-extrinsics (quaternion + position + rotation matrix; v1 ISO/IEC 23090-7 rotation struct out of scope), `cmin` camera-intrinsics, `sstr` single-stream (§6.5.38 bare marker on a derived item), plus the HEIF §6.10 text/font item-property family `txlo` text-layout (§6.10.2.1, `(flags&1)` 16/32-bit geometry + 8.8 font-size percent + TTML2 direction/writing-mode), `elng` extended-language (§6.10.2.2), `fnch` font-characteristics (§6.10.4.1, family/style/weight)), and the §11.2.2.2 mask-configuration property `mskC` (bits-per-pixel + §11.2.2 pixels-per-byte packing). Unknown boxes are retained as `Property::Other` so indices stay valid |
| Region items (`rgan`) | `region::RegionItem::parse` decodes a `'rgan'` region item's data into typed `RegionGeometry` variants — point / rectangle / ellipse / polygon / polyline / referenced-mask / inline-mask (HEIF §11.2.1, `(flags&1)` 16/32-bit fields, sign-extended coords); `inspect::region_items` / `region_items_for` enumerate the region items attached to the primary (or any image item) via the `'cdsc'` iref, resolving each one's data through the construction-method-aware `iloc` path into a `ResolvedRegionItem`. Derived region items (`drgn` §11.3.3) resolved by `region::resolve_derived_region_items` — every `'iden'` item carrying a `'drgn'` reference to its input region item, audited against the §11.3.3.2.1 `shall`s (single `'drgn'` input + `'drgn'`-iref-count ≤ 1 + no item body) as a `DerivedRegionItem` |
| Essential-property enforcement | `Meta::{unsupported_essential_properties, has_unsupported_essential_property}` flag any `ipma`-essential property that lands in `Property::Other` (av1-avif §2.3.2.1.2 + MIAF §7.3.5) |
| Sample Transform (`sato`) | descriptor parser + per-sample evaluator (av1-avif §4.2.3); image composition deferred until a real AV1 decoder lands |
| Tone Map (`tmap`) | four-CC detection + §4.2.2 compliance audit + `GainMapMetadata::parse` (ISO 21496-1:2025 Annex C.2) + **§6 application**: `unnormalize_log2_gain` (Formula 1), `weight_factor` (Formula 3), `apply_component` / `apply_rgb` / `apply_plane_rgb` reconstruct the linear alternate (HDR) rendition `(Baseline + k_base)·2^(W·G) − k_alt` (Formula 2) from a linear baseline + the decoded gain plane, with §5.2.5.1 per-component-metadata broadcast and §6.3 NOTE 2 achromatic handling; `apply_plane_rgb_coded` + `normalize_full_range_plane` accept the gain plane in its coded integer form at 8/10/12/16 bits (full-range scaling at the coded depth) |
| AV1 layered properties | `a1op` operating-point selector + `a1lx` layered-image index (av1-avif §2.3.2) |
| Auxiliary classification | `auxC` URN routed to `Alpha` / `DepthMap` / `HdrGainMap` / `Other` |
| Container encoder / muxer (`mux`) | `AvifMuxer` / `AvifGridMuxer` / `AvifOverlayMuxer` / `encode_still_av1` decide the AVIF layout — brands (profile tri-state `MA1B` default / `MA1A` / general-brands-only per av1-avif §8.1), item order (primary = item 1, a `grid` / `iovl` before its inputs), per-item property sets with their essential flags (`av1C` / `clap` / `irot` / `imir` / `lsel` / `a1op` essential), `auxC` URNs, the grid / overlay descriptors — and `oxideav-heif`'s `HeifWriter` serialises `meta` (`iloc` v1, derived bodies in `idat`, de-duplicated `ipco`, `prem` master → alpha) + `mdat`. Round-trips through `parse` byte-for-byte; passes `audit_mif1`; opens in `heif-info` / ImageMagick / ffmpeg (and `avifdec` for coded / grid primaries) |
| Pixel → AVIF still encoder (`still`) | `StillImage` + `encode_still` / `encode_still_grid` drive oxideav-av1's conformance-grade KEY-frame encoder: 8/10/12-bit × 4:2:0/4:2:2/4:4:4/monochrome; RGB(A) via the H.273 identity matrix in 4:4:4 (byte-exact lossless); alpha as a same-depth monochrome auxiliary with in-stream full-range signalling (the §5.5.2 `color_range` bit re-signalled by re-encoding the Sequence Header OBU — av1-avif §4.1 readers ignore `colr` on alpha); arbitrary extents via edge-replicated pad + top-left `clap` (§2.2.3) with `ispe` = coded extents (§2.2.2 `shall`); grid tiling (HEIF §6.6.2) with right/bottom trim; `ftyp` profile brand elected from `seq_profile` (§8). Lossless round-trips sample-exact through the crate's own decoder; lossy PSNR-gated; black-box-accepted by an external AVIF decoder binary |
| Registry `Encoder` | one video frame → one complete AVIF file packet: planar YUV 8-bit (+ `YuvJ*` full-range), 10/12-bit LE planar, `Gray8/10/12`, packed `Rgb24`/`Rgba`/`Bgr24`/`Bgra` (identity 4:4:4, alpha auxiliary), `Yuva420P`, `Ya8`; codec options `q`/`alpha_q`/`premultiplied` |
| Derived images | `iovl` overlay + `iden` identity + `tmap` tone-map derivations resolved end-to-end (HEIF §6.3 / §6.6.2, av1-avif §4.2.2) via a box-graph geometry resolver — no AV1 decode: `transform_chain` / `output_dims_from_reconstructed` apply `irot`/`imir`/`clap`/`iscl` in `ipma` order (§6.3); `reconstructed_dims` resolves grid/iovl descriptor dims + recursive `iden` inputs + `tmap` base-input extents + `sato` own/`ispe`-or-input extents + coded `ispe` (cycle-guarded, depth 16; descriptor bytes resolve through `iloc` construction methods 0/1/**2**); `resolve_overlays` clips each `OverlayPlacement` against the canvas (§6.6.2.2.3); `resolve_iden_derivations` folds the iden's own transforms over its source; `resolve_tone_maps` resolves each `tmap`'s base/gain-map input ids + rendered (base) extents + per-gain-map coded extents, flagging gain-map up-sampling. Surfaced on `AvifInfo::{overlay_resolutions, iden_resolutions, tone_map_resolutions}`; `inspect()` accepts `iovl`/`iden`/`tmap`/`sato` derived primaries. **Pixel composition** (`oxideav-heif`'s `compose`, through `overlay::composite_overlay` and the decoder's recursive output-image path): `iovl` canvases painted in `dimg` order with the §6.9.1 alpha rendering (straight / `prem`), canvas opacity seeded from the RGBA fill (converted with the output's H.273 matrix at every `colr`), inputs clipped at the canvas edge, an input at an odd offset on a subsampled axis or carrying alpha promotes the canvas to 4:4:4, monochrome inputs stay monochrome, `MAX_OVERLAY_CANVAS_PIXELS` bound; `iden` = its input's output image (alpha inherited) plus the iden's own transforms; both writable via `AvifOverlayMuxer` / `encode_still_overlay` and `AvifMuxer::with_identity_derivation`. `tmap` / `sato` remain geometry-only: the staged HEIF 3rd ed. / MIAF texts carry no `tmap` clause and av1-avif §4.2.2 only defers to HEIF, so the container's gain-map application (`oxideav_heif::apply_gain_map`, measured against producers) is not exposed here until the clause is staged; `sato` has no container counterpart |
| Unified derivation graph (`build_derivation_graph` / `inspect::derivation_graph`) | one decode-free traversal (HEIF §6.6) that walks any derived primary into a `DerivationGraph` — every reachable node (`DerivationNode` { `DerivationKind` ∈ Coded/Grid/Overlay/Identity/ToneMap/SampleTransform/Unknown, reconstructed + output dims, depth }) plus the de-duplicated coded-`av01` leaf decode set in first-visit order. Handles **nested** derivations (iden-of-grid, tmap-over-grid) and **diamond** graphs (shared leaf listed once); iterative pre-order with a `MAX_DERIVATION_DEPTH` cycle guard (`truncated` flag). Accessors: `output_dims` / `root_is_coded` / `coded_leaf_dims` (decode-buffer sizing) / `nodes_at_depth` / `derived_node_count`. No pixel composition — the dependency planner a renderer feeds its AV1 decoder |
| Entity grouping (`grpl`) | typed `EntityGroup` per `EntityToGroupBox` (retaining the 24-bit `flags`); `altr` / `ster` / `eqiv` / `pano` / `prgr` progressive-rendering (§6.8.10) / `brst` burst (§6.8.9) / `msrc` multi-source (HEIF §9.4) / `tsyn` time-synchronized capture (§6.8.3) / `iaug` audio-to-image (§6.8.4, `audio_repeats()` = flags-LSB) / `slid` slideshow (§6.8.9) / `albc` album + `favc` favorites user-collections (§6.8.7) / the five §6.8.6 bracketed capture-time sets `aebr`/`wbbr`/`fobr`/`afbr`/`dobr` (`BracketingKind` / `is_bracketed_set`). `inspect::entity_groups` enumerates the file's `grpl` decode-free for the typed projections |
| Text / font items (`text` / `font`) | `inspect::text_items` / `text_items_for` enumerate the §6.10.1 text items (`'mime'` items linked to an image via the `'text'` iref) annotating the primary (or any image item), surfacing each one's `content_type` and the `'font'`-iref-linked font item ids as a `ResolvedTextItem`; `is_font_item` recognises a §6.10.3 font item (`'mime'` + `content_type` starting `font/`, RFC 8081) |
| Coded-item dependency roles | `inspect::coded_item_dependencies` classifies an image item from its outgoing item references into `CodedItemDependencies` { `pred` predictively-coded decoding-order list (§6.4.9), `base` pre-derived-coded inputs (§6.4.7), `exbl` scalable base-layer (§6.4.8), `tbas` tile-base relation } with `is_predictively_coded` / `is_pre_derived` / `has_dependencies` projections; backed by the new `Meta::iref_targets_of` outgoing-reference walker |
| Brand compliance audit | `audit_mif1` (HEIF §10.2.1.1); MIAF Baseline (MA1B) / Advanced (MA1A) profile dispatch + av1-avif §8.2/§8.3 profile audit; `audit_pred_brand` (HEIF §10.2.4.2 — `'pred'` brand: per-predictive-item `'pred'`-dependency closure + the mif1 primary-independence `shall`, surfaced on `PredBrandCompliance`) |
| Metadata items | `cdsc` iref resolves Exif + XMP attached to the primary; raw bytes on demand via `item_payload_bytes` |
| Thumbnails | `thmb` iref enumeration via `AvifInfo::thumbnail_item_ids` |
| Premultiplied alpha | HEIF `prem` iref detected and surfaced |
| CICP colour signalling | `colr` nclx → `CicpTriple` with H.273 defaults; ICC + Unknown fall back to Unspecified |
| HDR metadata | `mdcv` (ST 2086), `clli` (MaxCLL/MaxFALL), `cclv`, `amve` ambient viewing environment (AVIF §6.5.36 / ISO/IEC 14496-12; 0.0001-lux illuminance + CIE 1931 ambient-light chromaticity, surfaced on `AvifInfo::amve`) |
| `av1C` introspection | bit depth (8/10/12), monochrome flag, chroma subsampling decoded into `AvifInfo` |
| Sequence Header OBU audit | av1-avif §2.1 "exactly one Sequence Header OBU" container-layer audit |
| Primary item data | resolved by the container (`HeifFile::item_data`, every `iloc` construction method: 0 file-offset, 1 idat-offset, 2 item-offset via the `'iloc'` reference, ISO/IEC 14496-12 §8.11.3) through `AvifHeader::item_data`; `parse` keeps a zero-copy slice for a single-span cm=0 primary. The bare-`ItemLocation` helpers `item_bytes` / `item_bytes_with_idat` / `item_bytes_owned*` remain for callers holding a plain `Meta`. Grid tiles, the alpha auxiliary, and metadata items (`item_payload_bytes`: Exif / XMP / mime / `tmap`) are all construction-method-aware |
| Grid primary items | grid descriptor parse + `dimg` tile composition by the container (8/10/12-bit tiles; `MAX_GRID_CANVAS_PIXELS` bound; the right / bottom trim applied here with AV1's ceiling chroma so an odd 4:2:0 output stays 4:2:0) + alpha auxiliary as a coded item **or a grid of monochrome tiles** (HEIF §6.4.1) + av1-avif §7 derivation-chain audit + **decode-free tile-geometry resolution** (`resolve_grids` → `GridResolution`: common tile dims + per-tile row-major canvas placement, `GridTilePlacement::visible` right/bottom trim §6.6.2.3.1, `covers_canvas` / `trimmed_tile_count`); surfaced on `AvifInfo::{grid_resolutions, has_grid, grid_resolution_for}` |
| Alpha auxiliary | `auxl` + `auxC` detection + attachment at every depth (`Gray8→Ya8`, `Gray10/12→Ya16Le`, `Yuv→YuvA` incl. `*10Le`/`*12Le`) + av1-avif §4.1 same-bit-depth `shall` enforced before attachment and audited at the container layer |
| Post-transforms | `clap` → `irot` → `imir` in `ipma` order (HEIF §6.3), at 8/10/12-bit and on the packed `Ya8`/`Ya16Le` layouts; an odd clean aperture on subsampled chroma promotes to 4:4:4 (MIAF §7.3.6.7); the standalone `apply_*` entry points refuse a layout change their `(frame, width, height)` result cannot carry |
| Layered image items | `a1lx` / `lsel` / `a1op` written by `AvifMuxer::{with_layered_index, with_layer_selector, with_operating_point}` and `encode_still_layered` (2..=4 AV1 spatial layers in one temporal unit, per-layer byte sizes from an OBU walk, `ispe`/`clap` of the selected layer — av1-avif §2.2.2 / §2.3); the decoder drains every shown frame, renders the `lsel` layer (or the top one) and decodes non-default `a1op` operating points |
| AVIS image sequences | **encode** (`encode_sequence`: `ftyp` `avis`/`msf1`/`av01` (+`avio`), cover still written by the container writer with the primary aliasing sample 0, `moov`/`trak` with `pict` handler, `av01` sample entry + `av1C`/`colr`/`clap`, `stts`/`stss`/`stsc`/`stsz`/`stco`; all-intra or KEY + P groups; lossless sequences decode exact through this crate, the external AVIF decoder and ffmpeg) + sample-table walk by the container (`parse_avis` over `oxideav_heif::sequence::parse_movie`; ISO/IEC 14496-12's mandatory `mvhd` / `tkhd` / `mdhd` / `hdlr` / `stts` enforced) + `inspect_avis` aggregator + §3 / §8.2 / §8.3 audits + `edts/elst` edit list (ISO/IEC 14496-12 §8.6.6) + `mdhd` media-timescale plumb + `prft` ProducerReferenceTimeBox (§8.16.5, v0/v1 NTP→Unix, top-level walk) on `AvisMeta::producer_reference_times` + `ssix` SubsegmentIndexBox (§8.16.4, v0; per-subsegment `(level: u8, range_size: u24)` leva-level byte-range partitions for partial-subsegment access, top-level walk) on `AvisMeta::subsegment_indexes` |
| Sample grouping | `sbgp` (SampleToGroupBox, ISO/IEC 14496-12:2015 §8.9.2, v0/v1) + `csgp` (CompactSampleToGroupBox, :2020 §8.9.5 — 4/8/16/32-bit packed field widths, pattern expansion, `traf` fragment-local msb) + `sgpd` (§8.9.3, v0/v1/v2 default index) which now also **retains + slices per-entry `VisualSampleGroupEntry` payloads** (v1 `default_length` fixed-size or self-describing `description_length`) into `SampleGroupDescription::entries`; typed decoders for the HEIF §6.8.6 bracketing entries (`BracketingEntry`: `aebr` auto-exposure / `wbbr` white-balance / `fobr` focus incl. infinity / `afbr` flash-exposure / `dobr` depth-of-field via `bracketing_entries()`) and the §6.8.1.2.2 `eqiv` `VisualEquivalenceEntry` (`time_offset` + 8.8 `timescale_multiplier`, `equivalence_entries()`); per-sample group-index lookup via `SampleToGroup::group_index_for_sample`, surfaced on `AvisMeta::{sample_to_groups, sample_group_descriptions}` |

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
`default-features = false` for an `oxideav-core`-free container parser
(it still depends on `oxideav-heif` with its default features off).

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
layout. Integration tests walk the full HEIF hierarchy, extract the
primary item, and decode every fixture to pixels end to end (the flat
lossless fixtures assert exact constant planes).

## License

MIT — see [LICENSE](LICENSE).
