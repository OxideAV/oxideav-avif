# oxideav-avif

Pure-Rust **AVIF** (AV1 Image File Format) decoder. Walks the HEIF /
ISOBMFF box hierarchy, resolves the primary item via `pitm` + `iloc`,
surfaces the `av1C` configuration record + `ispe` / `colr` / `pixi` /
`pasp` item properties, then hands the AV1 OBU bitstream to
[`oxideav-av1`](https://crates.io/crates/oxideav-av1) and composites
the result (grid tiles, alpha auxiliary, `irot` / `imir` / `clap`
post-transforms) into the frame returned to the caller. Zero C
dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Status

End-to-end AVIF decode works: `AvifDecoder::send_packet` +
`receive_frame` yield a `VideoFrame` whose dimensions match the
primary item's `ispe`. Pixel fidelity tracks the current state of
[`oxideav-av1`](https://crates.io/crates/oxideav-av1) — on simple
flat / synthetic content the decoded samples are tight against the
source; on rich content (natural photos) the intra-prediction path
still loses significant signal.

| Stage                                  | Coverage                                                                                                                                                   |
|----------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `ftyp` brand check                     | accepts `avif` / `avis` / `mif1` / `msf1` / `miaf`                                                                                                         |
| `meta` sub-boxes                       | `hdlr`, `pitm` (v0/v1), `iinf` (v0/v1) + `infe` (v2/v3), `iloc` (v0/v1/v2), `iref`, `iprp` / `ipco` / `ipma` (v0/v1, small + large property indices)       |
| Item properties                        | `av1C`, `ispe`, `colr` (nclx + ICC), `pixi`, `pasp`, `irot`, `imir`, `clap`, `auxC`, `mdcv`, `clli`, `cclv`; unknown boxes retained as `Property::Other` so indices stay valid |
| CICP color signalling                  | `colr` nclx → `CicpTriple` (primaries / transfer / matrix / full_range) with H.273 defaults (`Unspecified` = `2/2/2/false`); ICC + Unknown fall back to Unspecified; alpha auxiliary CICP constant carries `full_range = true` per av1-avif §4.1 |
| HDR metadata                           | `mdcv` (SMPTE ST 2086 mastering display primaries + luminance), `clli` (MaxCLL / MaxFALL cd/m²), `cclv` (draft av1-avif extension, same layout as `clli`); surfaced via `AvifInfo::{mdcv, clli, cclv, has_hdr_metadata(), max_cll(), max_fall()}` |
| AV1 wrap pass-through                  | `av1C`-derived bit depth (8/10/12-bit), monochrome flag, and chroma subsampling `(x, y)` decoded and surfaced via `AvifInfo::{bit_depth, monochrome, chroma_subsampling}`; callers no longer need to re-parse `av1C` |
| Primary item data                      | resolved via `iloc` construction_method 0 (file offset); single-extent items return a zero-copy slice; multi-extent items are concatenated via `item_bytes_owned()` (HEIF §8.11.3.3) |
| Grid primary items (HEIF §6.6.2)       | grid descriptor parse + per-tile decode via `dimg` iref + composite into the declared output rectangle                                                     |
| Alpha auxiliary                        | `auxl` + `auxC` URN detection, AV1-coded monochrome item decoded, composited onto the color frame (`Gray8 → YA8`, `Yuv → YuvA`)                            |
| Post-transforms                        | `clap` (centre crop) → `irot` (90/180/270°) → `imir` (horizontal/vertical), applied in that order per §6.5.10                                              |
| AV1 hand-off                           | `av1C` plumbed through `CodecParameters::extradata`; primary-item OBU payload fed to `oxideav_av1::Av1Decoder`; frame returned through `AvifDecoder`       |
| MIAF profile dispatch                  | `BrandClass` flags `is_baseline_profile` (MA1B) + `is_advanced_profile` (MA1A) + `is_miaf`; surfaced through `AvifInfo::brands`                            |
| AVIS image sequences                   | sample-table walk (`parse_avis` / `sample_table`) emits a flat frame-offset list; caller feeds each sample to `oxideav_av1` for sequential decode          |
| Encoder                                | **not implemented**: no AV1 encoder exists in oxideav                                                                                                      |

### What decodes

- Tiny flat-content AVIFs (avifenc-produced 16x16..64x64 mono or
  lossless 4:4:4) — sample means land within 1-2 units of the target
  value. See `tests/fixtures/{gray32,midgray,white16,red,black420}.avif`
  and the `decodes_flat_gray_to_mid_value` integration test.
- The 1280×720 `monochrome.avif` conformance fixture —
  `send_packet`/`receive_frame` succeed and return a full 1280×720
  Gray8 plane with a plausible brightness histogram.

### What fails / lossy

- Rich / natural-image AVIFs — the decoded YUV planes collapse toward
  mid-gray (intra edge filter + chroma intra still imperfect in the
  av1 crate). For the `testsrc` intra baseline in `oxideav-av1` PSNR
  hovers around 11 dB.
- `bbb_alpha.avif` (3840×2160 4:2:0 + alpha) — the AV1 layer rejects
  the bottom-edge `TX 64×56` shape (§5.11.27). The AVIF container
  handoff is verified end-to-end (alpha auxiliary item is correctly
  located and its OBU stream is well-formed) — the failure is in
  the AV1 crate's TX-set coverage, not the AVIF wrapper. A previous
  panic at `symbol.rs:105` is no longer reproducible — the av1 crate
  now surfaces a clean `Unsupported`.
- `kimono_rotate90.avif` (1024×722 4:2:0) — rejected by av1 as
  "TX 32×41 not in the AV1 set"; the irregular bottom edge
  (722 mod 64 = 18) lands on a TX size oxideav-av1 doesn't yet
  emit. The AVIF container code surfaces the error verbatim, and the
  `irot` property is exposed via `transforms_for` for callers that
  want to apply it themselves.

See `examples/diag_decode.rs` for a drop-in report of exactly which
stage each input reaches.

### Round 22 — HDR metadata + AV1 wrap pass-through + multi-extent items

Three headroom items addressed for the 60% → 75% coverage push:

* **HDR metadata (`mdcv` / `clli` / `cclv`)**: All three HDR-signalling
  item property boxes are now parsed and surfaced through `AvifInfo`.
  `mdcv` (MasteringDisplayColourVolumeBox — SMPTE ST 2086) carries
  display primaries + white point + max/min luminance; `clli`
  (ContentLightLevelBox — ISO/IEC 14496-12 §12.1.5.4) carries MaxCLL +
  MaxFALL in cd/m²; `cclv` is a draft av1-avif extension with the same
  layout as `clli`. Grid primaries follow the same fallback chain as
  `colr`/`pixi`/`pasp` (grid item first, tile 0 second). Helper methods
  `has_hdr_metadata()`, `max_cll()`, and `max_fall()` provide convenient
  gates for downstream consumers.

* **AV1 wrap pass-through** (`bit_depth`, `monochrome`,
  `chroma_subsampling`): The `av1C` record's three key subsampling/depth
  fields are now decoded inline in `decode_av1c_flags()` and stored
  directly on `AvifInfo` — callers no longer need to re-parse the record.
  `bit_depth` maps `(high_bitdepth, twelve_bit)` → `(8, 10, 12)`;
  `monochrome` mirrors the `av1C` mono bit; `chroma_subsampling` carries
  `(subsampling_x, subsampling_y)` or `None` for monochrome streams
  (subsampling is undefined for 4:0:0).

* **Multi-extent `iloc` items** (`item_bytes_owned`): A new public
  helper `item_bytes_owned(file, loc) -> Result<Vec<u8>>` concatenates
  all extents when an item spans more than one `iloc` extent entry (HEIF
  §8.11.3.3). The existing `item_bytes` fast path is preserved for the
  common single-extent case; only the new helper allocates. The old
  `Unsupported` error from `item_bytes` on multi-extent items remains
  so callers can decide when to use the slower path.

### Round 21 — grid hardening

Two correctness fixes for multi-tile MIAF AVIFs (HEIF §6.6.2.3 +
av1-avif §4.2.1):

* **Tile-edge chroma alignment**: `composite_grid` now uses ceiling
  division of the trimmed luma copy extent when computing chroma copy
  width / height, so a 4:2:0 grid whose right-most or bottom-most
  tile is clipped to an odd luma column / row no longer drops the
  trailing chroma sample. Previously a `tile_w=4`, `output_w=7`
  grid lost the right-most chroma column. Same fix covers 4:2:2
  horizontal-only chroma. Source-side and destination-side clamps
  guard against tiles whose chroma plane is smaller than the
  luma-derived ceiling, and against tiles that overhang the canvas.
* **Grid `colr` / `pixi` / `pasp` resolution**: every descriptive
  property now follows the canonical HEIF chain — grid item first
  (describes the reconstructed canvas), tile-0 second (the libheif
  writer pattern; av1-avif §4.2 keeps per-tile values uniform).
  Previously only `colr` had the fallback wiring; `pixi` looked
  only at tile 0 and `pasp` only at the grid item, so two real
  encoder placement patterns went unread.

`AvifInfo::effective_cicp()` consequently returns the same triple for
a grid whether `colr` lives on the grid item, on each tile, or on
both (av1-avif §4.2.1 — derived items inherit the colour signalling
of their inputs). When the grid and its tiles all lack `colr`, the
triple folds to `Unspecified` per ITU-T H.273.

### Round 20 — CICP color path

Per av1-avif §4.2.3.1 ("No color space conversion, matrix coefficients,
or transfer characteristics function shall be applied to the input
samples"), AVIF readers do **not** transform decoded pixels. The CICP
triple is signalling: it tells downstream consumers which colour space
the samples occupy. The crate now exposes a resolved
`(primaries, transfer, matrix, full_range)` quadruple via
`AvifInfo::effective_cicp() -> CicpTriple`:

* `Some(Colr::Nclx { .. })` → fields surfaced verbatim.
* `Some(Colr::Icc(_))` → ICC bytes are authoritative; CICP folds to
  the spec-mandated `Unspecified (2, 2, 2, false)`.
* `None` (no `colr` property) → `Unspecified (2, 2, 2, false)`.

`CicpTriple` ships predicates for the common decision points
(`is_unspecified`, `is_identity_matrix` for matrix=0 RGB AVIFs,
`is_libavif_srgb_default` for the `(1, 13, 6)` libavif default,
`has_reserved` flagging any axis in an ITU-T H.273 reserved range)
plus three name lookup helpers (`primaries_name`, `transfer_name`,
`matrix_name`) covering BT.709, BT.2020, Display P3, sRGB, PQ, HLG,
identity matrix, BT.601, BT.2020 NCL, ICtCp.

For alpha auxiliary items, av1-avif §4.1 mandates `color_range = 1`
and instructs readers to ignore any attached `colr`. The crate
exposes that as `CicpTriple::ALPHA` / `CicpTriple::for_alpha()`.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-codec = "0.1"
oxideav-av1 = "0.0"
oxideav-avif = "0.0"
```

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

### Register with a `CodecRegistry`

```rust
use oxideav_codec::CodecRegistry;
use oxideav_core::{CodecId, CodecParameters, Error, Packet, TimeBase};

let mut reg = CodecRegistry::new();
// `register_with_av1` installs both the AVIF entry and the AV1 decoder
// factory in one call — AVIF delegates to oxideav-av1 internally.
oxideav_avif::register_with_av1(&mut reg);

let params = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
let mut dec = reg.make_decoder(&params)?;

let bytes = std::fs::read("image.avif")?;
let pkt = Packet::new(0, TimeBase::new(1, 1), bytes);
dec.send_packet(&pkt)?;

match dec.receive_frame() {
    Ok(frame) => {
        // Flat / simple inputs decode cleanly.
        eprintln!("decoded frame: {frame:?}");
    }
    Err(Error::Unsupported(msg)) => {
        // Rich content still hits oxideav-av1 gaps — the message
        // names which AV1 feature is unsupported.
        eprintln!("av1 unsupported: {msg}");
    }
    Err(other) => return Err(other.into()),
}
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
// primary image's AV1 OBU stream. Pair it with `img.av1c` when
// building a CodecParameters for oxideav-av1.
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Codec id

- Codec: `"avif"`; capability name declared to the registry is
  `avif_heif_av1_decode` — end-to-end pipeline: parsed container + AV1
  frame decode + grid / alpha / transform composition.
- `CodecParameters::extradata` is the `av1C` byte record; width /
  height reflect `ispe` from the primary item.

## Test fixtures

`tests/fixtures/monochrome.avif` is `Monochrome.avif` from
[AOMediaCodec/av1-avif](https://github.com/AOMediaCodec/av1-avif/tree/main/testFiles/Microsoft)
— 1280×720, monochrome, single 8-bit plane. Integration tests walk
its complete HEIF hierarchy, extract the primary item, and decode it
end-to-end through `oxideav-av1`.

`tests/fixtures/{gray32,midgray,white16,red,black420}.avif` are tiny
(16×16 … 64×64) AVIFs produced by libavif's `avifenc` in lossless
mode (monochrome + 4:4:4) or q60 (4:2:0). They exist so the CI
decode-gate covers every colour-plane layout we support without
depending on an AV1 implementation that decodes rich photos
perfectly.

## License

MIT — see [LICENSE](LICENSE).
