# oxideav-avif

Pure-Rust **AVIF** (AV1 Image File Format) container parser. Walks
the HEIF / ISOBMFF box hierarchy, resolves the primary item via
`pitm` + `iloc`, surfaces the `av1C` configuration record + `ispe` /
`colr` / `pixi` / `pasp` item properties, and hands the AV1 OBU
bitstream to [`oxideav-av1`](https://crates.io/crates/oxideav-av1)
for further decoding. Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Status

HEIF container parsed → AV1 bitstream extracted → decode blocked at
AV1 tile decode. Concretely:

| Stage                                  | Coverage                                                                                                                                                   |
|----------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `ftyp` brand check                     | accepts `avif` / `avis` / `mif1` / `msf1` / `miaf`                                                                                                         |
| `meta` sub-boxes                       | `hdlr`, `pitm` (v0/v1), `iinf` (v0/v1) + `infe` (v2/v3), `iloc` (v0/v1/v2), `iprp` / `ipco` / `ipma` (v0/v1, small + large property indices)               |
| Item properties                        | `av1C`, `ispe`, `colr` (nclx + ICC), `pixi`, `pasp`; other property boxes retained as `Property::Other` so association indices stay valid                  |
| Primary item data                      | resolved via `iloc` construction_method 0 (file offset), single-extent items; multi-extent + idat / item-offset are currently rejected with `Unsupported`  |
| AV1 hand-off                           | `av1C` plumbed through `CodecParameters::extradata`; OBU payload fed to `oxideav_av1::Av1Decoder` — sequence header + frame header + `tile_info` parse     |
| Pixel decode                           | **not implemented**: `oxideav-av1` stops at the tile body, so `AvifDecoder::receive_frame()` returns `Error::Unsupported("avif pixel decode blocked …")`   |
| Encoder                                | **not implemented**: no AV1 encoder exists in oxideav                                                                                                      |

Until the AV1 crate grows partition / coefficient decode / prediction /
loop filter / CDEF / loop restoration, `AvifDecoder` returns headers
only. Use `AvifDecoder::info()` or the free function
`oxideav_avif::inspect(&bytes)` to retrieve dimensions, bit depth,
colour info, and the raw OBU slice.

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
    Ok(_frame) => unreachable!("AV1 pixel decode not yet implemented"),
    Err(Error::Unsupported(msg)) => {
        // Expected — the message names the AV1 stopping point.
        eprintln!("as-expected: {msg}");
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
  `avif_heif_av1_parse` (parsed container + AV1 headers, no pixels).
- `CodecParameters::extradata` is the `av1C` byte record; width /
  height reflect `ispe` from the primary item.

## Test fixture

`tests/fixtures/monochrome.avif` is `Monochrome.avif` from
[AOMediaCodec/av1-avif](https://github.com/AOMediaCodec/av1-avif/tree/main/testFiles/Microsoft)
— 1280×720, monochrome, single 8-bit plane. Unit tests walk its
complete HEIF hierarchy, extract the primary item, and feed it to the
AV1 decoder to confirm the hand-off succeeds up to the tile-body
stopping point.

## License

MIT — see [LICENSE](LICENSE).
