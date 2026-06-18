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
| Item properties | `av1C`, `ispe`, `colr` (nclx + ICC), `pixi`, `pasp`, `irot`, `imir`, `clap`, `auxC`, `mdcv`, `clli`, `cclv`, `rloc`, `lsel`, `a1op`, `a1lx`, plus the HEIF §6.5 descriptive family (`iscl`, `rref`, `crtt`, `mdft`, `udes`, `altt`, `aebr`, `wbbr`, `fobr`, `afbr`, `dobr`, `pano`, `subs`, `tols`, `prdi`, the slideshow transition-effect set `wipe`/`zoom`/`fade`/`splt`/`stpe`/`ssld`, `cmex` camera-extrinsics (quaternion + position + rotation matrix; v1 ISO/IEC 23090-7 rotation struct out of scope), and `cmin` camera-intrinsics). Unknown boxes are retained as `Property::Other` so indices stay valid |
| Essential-property enforcement | `Meta::{unsupported_essential_properties, has_unsupported_essential_property}` flag any `ipma`-essential property that lands in `Property::Other` (av1-avif §2.3.2.1.2 + MIAF §7.3.5) |
| Sample Transform (`sato`) | descriptor parser + per-sample evaluator (av1-avif §4.2.3); image composition deferred until a real AV1 decoder lands |
| Tone Map (`tmap`) | four-CC detection + §4.2.2 compliance audit + `GainMapMetadata::parse` (ISO 21496-1:2025 Annex C.2) |
| AV1 layered properties | `a1op` operating-point selector + `a1lx` layered-image index (av1-avif §2.3.2) |
| Auxiliary classification | `auxC` URN routed to `Alpha` / `DepthMap` / `HdrGainMap` / `Other` |
| Derived images | `iovl` ImageOverlay descriptor + `iden` item-type constant (composition pending a decoder) |
| Entity grouping (`grpl`) | typed `EntityGroup` per `EntityToGroupBox`; `altr` / `ster` / `eqiv` / panorama recognised (HEIF §9.4) |
| Brand compliance audit | `audit_mif1` (HEIF §10.2.1.1); MIAF Baseline (MA1B) / Advanced (MA1A) profile dispatch + av1-avif §8.2/§8.3 profile audit |
| Metadata items | `cdsc` iref resolves Exif + XMP attached to the primary; raw bytes on demand via `item_payload_bytes` |
| Thumbnails | `thmb` iref enumeration via `AvifInfo::thumbnail_item_ids` |
| Premultiplied alpha | HEIF `prem` iref detected and surfaced |
| CICP colour signalling | `colr` nclx → `CicpTriple` with H.273 defaults; ICC + Unknown fall back to Unspecified |
| HDR metadata | `mdcv` (ST 2086), `clli` (MaxCLL/MaxFALL), `cclv`, `amve` ambient viewing environment (AVIF §6.5.36 / ISO/IEC 14496-12; 0.0001-lux illuminance + CIE 1931 ambient-light chromaticity, surfaced on `AvifInfo::amve`) |
| `av1C` introspection | bit depth (8/10/12), monochrome flag, chroma subsampling decoded into `AvifInfo` |
| Sequence Header OBU audit | av1-avif §2.1 "exactly one Sequence Header OBU" container-layer audit |
| Primary item data | resolved via `iloc` construction_method 0; single-extent zero-copy slice, multi-extent concatenated (HEIF §8.11.3.3) |
| Grid primary items | grid descriptor parse + `dimg` tile composition + av1-avif §7 derivation-chain audit |
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
