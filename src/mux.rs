//! AVIF container **muxer** (encoder) — emits a conformant AVIF file
//! (`ftyp` + `meta` box tree + `mdat`) around one or more already-coded
//! AV1 Image Item bitstreams.
//!
//! This module operates purely at the ISO-BMFF / HEIF / MIAF container
//! level. It takes the AV1 bitstream as an **opaque byte payload** (the
//! AV1 Image Item Data — the content of a `sync` AV1 sample, av1-avif
//! §2.1) plus its `av1C` configuration record, and wraps them in the box
//! hierarchy an AVIF reader expects. It does **not** encode pixels to
//! AV1 — that is the job of an AV1 encoder (not yet available in
//! oxideav; see the crate README's "Encoder" section).
//!
//! The emitted structure is deliberately the minimal-but-conformant set
//! every AVIF reader understands (av1-avif §9.1.1 "Minimum set of
//! boxes"):
//!
//! * `ftyp` — `avif` major brand, `[avif, mif1, miaf, MA1B]` compatible
//!   brands (AVIF Baseline Profile, av1-avif §8.2).
//! * `meta` (FullBox v0) containing `hdlr` (`pict`), `pitm`, `iinf` /
//!   `infe` (v2), `iref` (when an alpha auxiliary or grid derivation is
//!   present), `iprp` (`ipco` + `ipma`), and `iloc` (v0, file-offset
//!   `construction_method == 0`).
//! * `mdat` — the concatenated item payloads.
//!
//! Item properties emitted per item: `av1C` (essential), `ispe`, `pixi`,
//! `colr` (`nclx` or ICC), `pasp`, `clap` / `irot` / `imir` (essential
//! transformative properties).
//!
//! # Round-trip
//!
//! The output is designed to read back through this crate's own
//! [`crate::parse`] / [`crate::parse_header`] path pixel-consistently:
//! the coded AV1 payload and every property round-trips byte-for-byte.
//!
//! # Layout strategy
//!
//! `iloc` extent offsets are absolute file offsets. Because the width of
//! an `iloc` offset field (4 bytes here) is independent of its value, the
//! `meta` box's *size* does not depend on the offset values. The muxer
//! therefore builds the `meta` box once to measure its length, computes
//! the `mdat` data start (`ftyp.len() + meta.len() + 8`), then rebuilds
//! `meta` with the real absolute offsets patched in.

use crate::error::{AvifError as Error, Result};
use crate::meta::{Amve, Clap, Clli, Colr, Imir, Irot, Mdcv, Pasp};

/// Little byte-buffer builder for box bodies.
#[derive(Default)]
struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn fourcc(&mut self, b: &[u8; 4]) {
        self.0.extend_from_slice(b);
    }
    /// NUL-terminated ASCII string.
    fn cstr(&mut self, s: &str) {
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
    }
    fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

/// Encode a plain `Box`: `size(4) + type(4) + body`.
fn boxed(box_type: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let size = (8 + body.len()) as u32;
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(box_type);
    out.extend_from_slice(body);
    out
}

/// Encode a `FullBox`: prepends `version(1) + flags(3)` to `body`.
fn full_boxed(box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + body.len());
    inner.push(version);
    inner.push((flags >> 16) as u8);
    inner.push((flags >> 8) as u8);
    inner.push(flags as u8);
    inner.extend_from_slice(body);
    boxed(box_type, &inner)
}

// ─────────────────────────── item-property encoders ───────────────────

/// One item property, ready to be placed in `ipco` and referenced by
/// `ipma`.
#[derive(Clone)]
struct PropBox {
    /// Fully-encoded property box bytes (header + body).
    bytes: Vec<u8>,
    /// Whether the association marks this property essential.
    essential: bool,
}

fn prop_av1c(av1c: &[u8]) -> PropBox {
    PropBox {
        bytes: boxed(b"av1C", av1c),
        essential: true,
    }
}

fn prop_ispe(width: u32, height: u32) -> PropBox {
    let mut w = W::default();
    w.u32(width);
    w.u32(height);
    PropBox {
        bytes: full_boxed(b"ispe", 0, 0, &w.into_vec()),
        essential: false,
    }
}

fn prop_pixi(bits: &[u8]) -> PropBox {
    let mut w = W::default();
    w.u8(bits.len() as u8);
    w.bytes(bits);
    PropBox {
        bytes: full_boxed(b"pixi", 0, 0, &w.into_vec()),
        essential: false,
    }
}

fn prop_colr(colr: &Colr) -> Result<PropBox> {
    let mut w = W::default();
    match colr {
        Colr::Nclx {
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range,
        } => {
            w.fourcc(b"nclx");
            w.u16(*colour_primaries);
            w.u16(*transfer_characteristics);
            w.u16(*matrix_coefficients);
            w.u8(if *full_range { 0x80 } else { 0x00 });
        }
        Colr::Icc(icc) => {
            w.fourcc(b"prof");
            w.bytes(icc);
        }
        Colr::Unknown(t) => {
            return Err(Error::unsupported(format!(
                "avif mux: cannot emit colr of unknown type '{}'",
                String::from_utf8_lossy(t)
            )));
        }
    }
    Ok(PropBox {
        bytes: boxed(b"colr", &w.into_vec()),
        essential: false,
    })
}

fn prop_pasp(pasp: &Pasp) -> PropBox {
    let mut w = W::default();
    w.u32(pasp.h_spacing);
    w.u32(pasp.v_spacing);
    PropBox {
        bytes: boxed(b"pasp", &w.into_vec()),
        essential: false,
    }
}

fn prop_clap(clap: &Clap) -> PropBox {
    let mut w = W::default();
    for v in [
        clap.clean_aperture_width_n,
        clap.clean_aperture_width_d,
        clap.clean_aperture_height_n,
        clap.clean_aperture_height_d,
        clap.horiz_off_n,
        clap.horiz_off_d,
        clap.vert_off_n,
        clap.vert_off_d,
    ] {
        w.u32(v as u32);
    }
    PropBox {
        bytes: boxed(b"clap", &w.into_vec()),
        // Transformative properties are marked essential per MIAF.
        essential: true,
    }
}

fn prop_irot(irot: &Irot) -> PropBox {
    PropBox {
        bytes: boxed(b"irot", &[irot.angle & 0x03]),
        essential: true,
    }
}

fn prop_imir(imir: &Imir) -> PropBox {
    PropBox {
        bytes: boxed(b"imir", &[imir.axis & 0x01]),
        essential: true,
    }
}

/// AVIF alpha auxiliary URN (av1-avif §4.1 / HEIF §6.5.8).
fn prop_auxc(urn: &str) -> PropBox {
    let mut w = W::default();
    w.cstr(urn);
    PropBox {
        bytes: full_boxed(b"auxC", 0, 0, &w.into_vec()),
        essential: false,
    }
}

/// `mdcv` MasteringDisplayColourVolumeBox (ISO/IEC 14496-12 §12.1.5.3) —
/// a plain box, no FullBox header. 6×u16 primaries + 2×u16 white point +
/// 2×u32 luminance.
fn prop_mdcv(m: &Mdcv) -> PropBox {
    let mut w = W::default();
    for (x, y) in m.display_primaries_xy {
        w.u16(x);
        w.u16(y);
    }
    w.u16(m.white_point_xy.0);
    w.u16(m.white_point_xy.1);
    w.u32(m.max_display_mastering_luminance);
    w.u32(m.min_display_mastering_luminance);
    PropBox {
        bytes: boxed(b"mdcv", &w.into_vec()),
        essential: false,
    }
}

/// `clli` ContentLightLevelBox (ISO/IEC 14496-12 §12.1.5.4) — plain box,
/// two u16 (MaxCLL, MaxFALL).
fn prop_clli(c: &Clli) -> PropBox {
    let mut w = W::default();
    w.u16(c.max_content_light_level);
    w.u16(c.max_pic_average_light_level);
    PropBox {
        bytes: boxed(b"clli", &w.into_vec()),
        essential: false,
    }
}

/// `amve` AmbientViewingEnvironmentBox (AVIF §6.5.36) — plain box, u32
/// illuminance + 2×u16 CIE 1931 chromaticity.
fn prop_amve(a: &Amve) -> PropBox {
    let mut w = W::default();
    w.u32(a.ambient_illuminance);
    w.u16(a.ambient_light_x);
    w.u16(a.ambient_light_y);
    PropBox {
        bytes: boxed(b"amve", &w.into_vec()),
        essential: false,
    }
}

// ───────────────────────────── item model ─────────────────────────────

/// One item to be muxed: an entry in `iinf`/`infe`, `iloc`, and (via its
/// property list) `ipco`/`ipma`.
struct MuxItem {
    id: u32,
    item_type: [u8; 4],
    name: String,
    hidden: bool,
    /// MIME `content_type` for a `mime` item (e.g. XMP). Emitted in the
    /// `infe` v2 tail per ISO-BMFF §8.11.6.2; `None` for every other
    /// item type.
    content_type: Option<String>,
    /// Bytes placed in `mdat`. Every item this muxer emits is a
    /// single-extent, file-offset (`construction_method == 0`) item.
    payload: Vec<u8>,
    props: Vec<PropBox>,
}

/// One typed item reference emitted into `iref`.
struct MuxIref {
    reference_type: [u8; 4],
    from_id: u32,
    to_ids: Vec<u32>,
}

// ───────────────────────────── public API ─────────────────────────────

/// Builder for a single-image AVIF file wrapping one coded AV1 primary
/// item, with optional colour / transform properties and an optional
/// alpha auxiliary.
pub struct AvifMuxer {
    width: u32,
    height: u32,
    primary_payload: Vec<u8>,
    av1c: Vec<u8>,
    pixi: Option<Vec<u8>>,
    colr: Option<Colr>,
    pasp: Option<Pasp>,
    clap: Option<Clap>,
    irot: Option<Irot>,
    imir: Option<Imir>,
    mdcv: Option<Mdcv>,
    clli: Option<Clli>,
    amve: Option<Amve>,
    alpha: Option<AlphaImage>,
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    advanced_profile: bool,
}

/// An AV1-coded alpha plane, carried as a monochrome auxiliary image item
/// (`auxC` URN + `auxl` iref to the primary).
struct AlphaImage {
    payload: Vec<u8>,
    av1c: Vec<u8>,
    pixi: Option<Vec<u8>>,
    premultiplied: bool,
}

impl AvifMuxer {
    /// Start a muxer for a `width × height` primary image whose coded AV1
    /// Image Item Data is `payload` and whose configuration record is
    /// `av1c` (the `AV1CodecConfigurationRecord` bytes — the same bytes
    /// the reader surfaces as [`crate::AvifImage::av1c`]).
    pub fn new(width: u32, height: u32, payload: Vec<u8>, av1c: Vec<u8>) -> Self {
        Self {
            width,
            height,
            primary_payload: payload,
            av1c,
            pixi: None,
            colr: None,
            pasp: None,
            clap: None,
            irot: None,
            imir: None,
            mdcv: None,
            clli: None,
            amve: None,
            alpha: None,
            exif: None,
            xmp: None,
            advanced_profile: false,
        }
    }

    /// Set the `pixi` per-channel bit depths (e.g. `[8, 8, 8]` for 8-bit
    /// colour, `[8]` for monochrome).
    pub fn with_pixi(mut self, bits: Vec<u8>) -> Self {
        self.pixi = Some(bits);
        self
    }

    /// Attach a colour-information (`colr`) property.
    pub fn with_colr(mut self, colr: Colr) -> Self {
        self.colr = Some(colr);
        self
    }

    /// Attach a pixel-aspect-ratio (`pasp`) property.
    pub fn with_pasp(mut self, pasp: Pasp) -> Self {
        self.pasp = Some(pasp);
        self
    }

    /// Attach a clean-aperture (`clap`) transformative property.
    pub fn with_clap(mut self, clap: Clap) -> Self {
        self.clap = Some(clap);
        self
    }

    /// Attach an image-rotation (`irot`) transformative property.
    pub fn with_irot(mut self, angle: u8) -> Self {
        self.irot = Some(Irot {
            angle: angle & 0x03,
        });
        self
    }

    /// Attach an image-mirror (`imir`) transformative property.
    pub fn with_imir(mut self, axis: u8) -> Self {
        self.imir = Some(Imir { axis: axis & 0x01 });
        self
    }

    /// Attach a mastering-display-colour-volume (`mdcv`) HDR property.
    pub fn with_mdcv(mut self, mdcv: Mdcv) -> Self {
        self.mdcv = Some(mdcv);
        self
    }

    /// Attach a content-light-level (`clli`) HDR property.
    pub fn with_clli(mut self, clli: Clli) -> Self {
        self.clli = Some(clli);
        self
    }

    /// Attach an ambient-viewing-environment (`amve`) HDR property.
    pub fn with_amve(mut self, amve: Amve) -> Self {
        self.amve = Some(amve);
        self
    }

    /// Attach an Exif metadata item, linked to the primary via a `cdsc`
    /// item reference (av1-avif §5.2 / HEIF §A.2.1). `payload` is the full
    /// `ExifDataBlock` — a 4-byte `exif_tiff_header_offset` followed by
    /// the TIFF-structured Exif bytes; it is stored verbatim.
    pub fn with_exif(mut self, payload: Vec<u8>) -> Self {
        self.exif = Some(payload);
        self
    }

    /// Attach an XMP metadata item (a `mime` item with content type
    /// `application/rdf+xml`), linked to the primary via a `cdsc` item
    /// reference (av1-avif §5.3).
    pub fn with_xmp(mut self, payload: Vec<u8>) -> Self {
        self.xmp = Some(payload);
        self
    }

    /// Declare the AVIF Advanced Profile (`MA1A`) in `ftyp` instead of
    /// the default Baseline Profile (`MA1B`) (av1-avif §8.2 / §8.3). The
    /// muxer does not itself validate that the AV1 bitstream meets the
    /// profile constraints — that is the encoder's responsibility.
    pub fn advanced_profile(mut self) -> Self {
        self.advanced_profile = true;
        self
    }

    /// Attach an AV1-coded alpha plane as an auxiliary image item. The
    /// alpha item is emitted as a hidden monochrome `av01` item carrying
    /// an `auxC` (alpha URN) property and linked to the primary via an
    /// `auxl` item reference (av1-avif §4.1). When `premultiplied` is
    /// true a `prem` iref is also emitted (HEIF §6.10.1.1).
    pub fn with_alpha(mut self, payload: Vec<u8>, av1c: Vec<u8>, premultiplied: bool) -> Self {
        self.alpha = Some(AlphaImage {
            payload,
            av1c,
            pixi: Some(vec![8]),
            premultiplied,
        });
        self
    }

    /// Override the alpha item's `pixi` bit depth (default `[8]`).
    pub fn with_alpha_pixi(mut self, bits: Vec<u8>) -> Self {
        if let Some(a) = self.alpha.as_mut() {
            a.pixi = Some(bits);
        }
        self
    }

    /// Build the AVIF file bytes.
    pub fn build(self) -> Result<Vec<u8>> {
        if self.av1c.len() < 4 {
            return Err(Error::invalid(
                "avif mux: av1C configuration record must be at least 4 bytes",
            ));
        }
        // Item 1 = primary colour image.
        let mut primary = MuxItem {
            id: 1,
            item_type: *b"av01",
            name: String::new(),
            hidden: false,
            content_type: None,
            payload: self.primary_payload,
            props: vec![prop_av1c(&self.av1c), prop_ispe(self.width, self.height)],
        };
        if let Some(bits) = &self.pixi {
            primary.props.push(prop_pixi(bits));
        }
        if let Some(colr) = &self.colr {
            primary.props.push(prop_colr(colr)?);
        }
        if let Some(pasp) = &self.pasp {
            primary.props.push(prop_pasp(pasp));
        }
        // HDR descriptive properties.
        if let Some(mdcv) = &self.mdcv {
            primary.props.push(prop_mdcv(mdcv));
        }
        if let Some(clli) = &self.clli {
            primary.props.push(prop_clli(clli));
        }
        if let Some(amve) = &self.amve {
            primary.props.push(prop_amve(amve));
        }
        // Transformative properties come last (they apply after the
        // descriptive ones); MIAF constrains their relative order.
        if let Some(clap) = &self.clap {
            primary.props.push(prop_clap(clap));
        }
        if let Some(irot) = &self.irot {
            primary.props.push(prop_irot(irot));
        }
        if let Some(imir) = &self.imir {
            primary.props.push(prop_imir(imir));
        }

        let mut items = vec![primary];
        let mut irefs = Vec::new();
        let mut next_id = 2u32;

        if let Some(alpha) = self.alpha {
            if alpha.av1c.len() < 4 {
                return Err(Error::invalid(
                    "avif mux: alpha av1C configuration record must be at least 4 bytes",
                ));
            }
            let alpha_id = next_id;
            next_id += 1;
            let mut aprops = vec![
                prop_av1c(&alpha.av1c),
                prop_ispe(self.width, self.height),
                prop_auxc(crate::alpha::ALPHA_URN_PREFIX),
            ];
            if let Some(bits) = &alpha.pixi {
                aprops.push(prop_pixi(bits));
            }
            items.push(MuxItem {
                id: alpha_id,
                item_type: *b"av01",
                name: "Alpha".to_string(),
                hidden: true,
                content_type: None,
                payload: alpha.payload,
                props: aprops,
            });
            // `auxl`: alpha item -> primary (HEIF §6.5.8 / av1-avif §4.1).
            irefs.push(MuxIref {
                reference_type: *b"auxl",
                from_id: alpha_id,
                to_ids: vec![1],
            });
            if alpha.premultiplied {
                irefs.push(MuxIref {
                    reference_type: *b"prem",
                    from_id: alpha_id,
                    to_ids: vec![1],
                });
            }
        }

        // Exif / XMP metadata items, each linked to the primary via a
        // `cdsc` (content-describes) iref (av1-avif §5.2 / §5.3).
        if let Some(exif) = self.exif {
            let id = next_id;
            next_id += 1;
            items.push(MuxItem {
                id,
                item_type: *b"Exif",
                name: String::new(),
                hidden: false,
                content_type: None,
                payload: exif,
                props: Vec::new(),
            });
            irefs.push(MuxIref {
                reference_type: *b"cdsc",
                from_id: id,
                to_ids: vec![1],
            });
        }
        if let Some(xmp) = self.xmp {
            let id = next_id;
            items.push(MuxItem {
                id,
                item_type: *b"mime",
                name: String::new(),
                hidden: false,
                content_type: Some("application/rdf+xml".to_string()),
                payload: xmp,
                props: Vec::new(),
            });
            irefs.push(MuxIref {
                reference_type: *b"cdsc",
                from_id: id,
                to_ids: vec![1],
            });
        }

        assemble(&items, 1, &irefs, self.advanced_profile)
    }
}

/// Convenience: mux a still AVIF from a coded AV1 payload + config record
/// with an `ispe` of `width × height` and the given `pixi` bit depths.
/// Equivalent to [`AvifMuxer::new`] + [`AvifMuxer::with_pixi`] +
/// [`AvifMuxer::build`].
pub fn encode_still_av1(
    width: u32,
    height: u32,
    payload: Vec<u8>,
    av1c: Vec<u8>,
    pixi_bits: Vec<u8>,
) -> Result<Vec<u8>> {
    AvifMuxer::new(width, height, payload, av1c)
        .with_pixi(pixi_bits)
        .build()
}

/// One coded AV1 tile for a grid image.
pub struct GridTile {
    /// Tile width in pixels (its own `ispe`).
    pub width: u32,
    /// Tile height in pixels.
    pub height: u32,
    /// Coded AV1 Image Item Data for the tile.
    pub payload: Vec<u8>,
    /// The tile's `av1C` configuration record.
    pub av1c: Vec<u8>,
}

/// Builder for a tiled (grid-derived) AVIF image. The primary item is a
/// `grid` derived item (HEIF §6.6.2); its inputs are `rows × columns`
/// hidden `av01` tile items linked via a `dimg` item reference.
pub struct AvifGridMuxer {
    rows: u16,
    columns: u16,
    output_width: u32,
    output_height: u32,
    tiles: Vec<GridTile>,
    pixi: Option<Vec<u8>>,
    colr: Option<Colr>,
}

impl AvifGridMuxer {
    /// Start a grid muxer. `rows × columns` tiles compose an
    /// `output_width × output_height` canvas. Tiles are supplied in
    /// row-major order via [`Self::tile`].
    pub fn new(rows: u16, columns: u16, output_width: u32, output_height: u32) -> Self {
        Self {
            rows,
            columns,
            output_width,
            output_height,
            tiles: Vec::new(),
            pixi: None,
            colr: None,
        }
    }

    /// Append a tile (row-major order).
    pub fn tile(mut self, tile: GridTile) -> Self {
        self.tiles.push(tile);
        self
    }

    /// Attach a `pixi` property to the grid item.
    pub fn with_pixi(mut self, bits: Vec<u8>) -> Self {
        self.pixi = Some(bits);
        self
    }

    /// Attach a `colr` property to the grid item.
    pub fn with_colr(mut self, colr: Colr) -> Self {
        self.colr = Some(colr);
        self
    }

    /// Build the AVIF grid file bytes.
    pub fn build(self) -> Result<Vec<u8>> {
        let expected = self.rows as usize * self.columns as usize;
        if self.tiles.len() != expected {
            return Err(Error::invalid(format!(
                "avif mux: {}×{} grid needs {expected} tiles, got {}",
                self.rows,
                self.columns,
                self.tiles.len()
            )));
        }
        if self.tiles.is_empty() {
            return Err(Error::invalid("avif mux: grid needs at least one tile"));
        }

        // Grid item = id 1 (the primary); tiles = ids 2.. .
        let grid_payload = build_grid_descriptor(
            self.rows,
            self.columns,
            self.output_width,
            self.output_height,
        );
        let mut grid_props = vec![prop_ispe(self.output_width, self.output_height)];
        if let Some(bits) = &self.pixi {
            grid_props.push(prop_pixi(bits));
        }
        if let Some(colr) = &self.colr {
            grid_props.push(prop_colr(colr)?);
        }
        let mut items = vec![MuxItem {
            id: 1,
            item_type: *b"grid",
            name: String::new(),
            hidden: false,
            content_type: None,
            payload: grid_payload,
            props: grid_props,
        }];

        let mut tile_ids = Vec::with_capacity(self.tiles.len());
        for (i, tile) in self.tiles.into_iter().enumerate() {
            if tile.av1c.len() < 4 {
                return Err(Error::invalid(
                    "avif mux: grid tile av1C must be at least 4 bytes",
                ));
            }
            let id = 2 + i as u32;
            tile_ids.push(id);
            items.push(MuxItem {
                id,
                item_type: *b"av01",
                name: String::new(),
                hidden: true,
                content_type: None,
                payload: tile.payload,
                props: vec![prop_av1c(&tile.av1c), prop_ispe(tile.width, tile.height)],
            });
        }

        let irefs = vec![MuxIref {
            reference_type: *b"dimg",
            from_id: 1,
            to_ids: tile_ids,
        }];
        assemble(&items, 1, &irefs, false)
    }
}

/// Encode a HEIF `grid` descriptor (HEIF §6.6.2.3). Output dims ≤ 65535
/// use the compact 16-bit form; larger canvases set the `flags` LSB and
/// emit 32-bit dims.
fn build_grid_descriptor(
    rows: u16,
    columns: u16,
    output_width: u32,
    output_height: u32,
) -> Vec<u8> {
    let wide = output_width > u16::MAX as u32 || output_height > u16::MAX as u32;
    let mut w = W::default();
    w.u8(0); // version
    w.u8(if wide { 1 } else { 0 }); // flags: bit 0 => 32-bit output dims
    w.u8((rows - 1) as u8);
    w.u8((columns - 1) as u8);
    if wide {
        w.u32(output_width);
        w.u32(output_height);
    } else {
        w.u16(output_width as u16);
        w.u16(output_height as u16);
    }
    w.into_vec()
}

// ───────────────────────────── assembly ───────────────────────────────

/// Assemble the full AVIF file from an item list, the primary item id,
/// and the item-reference list. `advanced_profile` selects the `MA1A`
/// brand instead of `MA1B` in `ftyp`.
fn assemble(
    items: &[MuxItem],
    primary_id: u32,
    irefs: &[MuxIref],
    advanced_profile: bool,
) -> Result<Vec<u8>> {
    if items.len() > u16::MAX as usize {
        return Err(Error::unsupported("avif mux: too many items for v0 boxes"));
    }
    // 1. Lay out mdat: record each item's offset relative to the start of
    //    the mdat payload.
    let mut rel_offsets = Vec::with_capacity(items.len());
    let mut mdat_payload = Vec::new();
    for it in items {
        rel_offsets.push(mdat_payload.len() as u64);
        mdat_payload.extend_from_slice(&it.payload);
    }

    // 2. Build a global ipco property table (dedup identical property
    //    boxes) and per-item 1-based association lists.
    let mut ipco_props: Vec<Vec<u8>> = Vec::new();
    let mut item_assocs: Vec<Vec<(u16, bool)>> = Vec::with_capacity(items.len());
    for it in items {
        let mut assocs = Vec::with_capacity(it.props.len());
        for p in &it.props {
            let idx = match ipco_props.iter().position(|b| b == &p.bytes) {
                Some(i) => i,
                None => {
                    ipco_props.push(p.bytes.clone());
                    ipco_props.len() - 1
                }
            };
            let one_based = (idx + 1) as u16;
            if one_based > 0x7f {
                return Err(Error::unsupported(
                    "avif mux: >127 distinct properties need the large-index ipma form",
                ));
            }
            assocs.push((one_based, p.essential));
        }
        item_assocs.push(assocs);
    }

    let ftyp = build_ftyp(advanced_profile);
    // 3. Measure the meta box length with placeholder offsets, then
    //    rebuild with absolute offsets. Offset field width is fixed, so
    //    the length is stable across the two builds.
    let probe_meta = build_meta(
        items,
        primary_id,
        irefs,
        &ipco_props,
        &item_assocs,
        &rel_offsets,
        0,
    );
    let mdat_data_start = (ftyp.len() + probe_meta.len() + 8) as u64;
    let meta = build_meta(
        items,
        primary_id,
        irefs,
        &ipco_props,
        &item_assocs,
        &rel_offsets,
        mdat_data_start,
    );
    debug_assert_eq!(meta.len(), probe_meta.len());

    let mut out = Vec::with_capacity(ftyp.len() + meta.len() + 8 + mdat_payload.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&meta);
    out.extend_from_slice(&boxed(b"mdat", &mdat_payload));
    Ok(out)
}

/// `ftyp`: AVIF brand set (av1-avif §6.2 / §8.2 / §8.3). Baseline
/// (`MA1B`) by default; `advanced_profile` selects `MA1A`.
fn build_ftyp(advanced_profile: bool) -> Vec<u8> {
    let mut w = W::default();
    w.fourcc(b"avif"); // major_brand
    w.u32(0); // minor_version
    w.fourcc(b"avif");
    w.fourcc(b"mif1");
    w.fourcc(b"miaf");
    w.fourcc(if advanced_profile { b"MA1A" } else { b"MA1B" });
    boxed(b"ftyp", &w.into_vec())
}

#[allow(clippy::too_many_arguments)]
fn build_meta(
    items: &[MuxItem],
    primary_id: u32,
    irefs: &[MuxIref],
    ipco_props: &[Vec<u8>],
    item_assocs: &[Vec<(u16, bool)>],
    rel_offsets: &[u64],
    mdat_data_start: u64,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&build_hdlr());
    body.extend_from_slice(&build_pitm(primary_id));
    body.extend_from_slice(&build_iinf(items));
    if !irefs.is_empty() {
        body.extend_from_slice(&build_iref(irefs));
    }
    body.extend_from_slice(&build_iprp(ipco_props, items, item_assocs));
    body.extend_from_slice(&build_iloc(items, rel_offsets, mdat_data_start));
    full_boxed(b"meta", 0, 0, &body)
}

fn build_hdlr() -> Vec<u8> {
    let mut w = W::default();
    w.u32(0); // pre_defined
    w.fourcc(b"pict"); // handler_type
    w.u32(0); // reserved[0]
    w.u32(0); // reserved[1]
    w.u32(0); // reserved[2]
    w.cstr(""); // name
    full_boxed(b"hdlr", 0, 0, &w.into_vec())
}

fn build_pitm(primary_id: u32) -> Vec<u8> {
    let mut w = W::default();
    w.u16(primary_id as u16);
    full_boxed(b"pitm", 0, 0, &w.into_vec())
}

fn build_iinf(items: &[MuxItem]) -> Vec<u8> {
    let mut infe_all = Vec::new();
    for it in items {
        infe_all.extend_from_slice(&build_infe(it));
    }
    let mut w = W::default();
    w.u16(items.len() as u16); // entry_count
    w.bytes(&infe_all);
    full_boxed(b"iinf", 0, 0, &w.into_vec())
}

fn build_infe(it: &MuxItem) -> Vec<u8> {
    let mut w = W::default();
    w.u16(it.id as u16);
    w.u16(0); // item_protection_index
    w.fourcc(&it.item_type);
    w.cstr(&it.name);
    // `mime` items carry a content_type (and optional content_encoding,
    // which we always omit) after the name (ISO-BMFF §8.11.6.2).
    if let Some(ct) = &it.content_type {
        w.cstr(ct);
    }
    // FullBox flags bit 0 = hidden-image-item signal (HEIF §6.4.2).
    let flags = if it.hidden { 1 } else { 0 };
    full_boxed(b"infe", 2, flags, &w.into_vec())
}

fn build_iref(irefs: &[MuxIref]) -> Vec<u8> {
    let mut children = Vec::new();
    for r in irefs {
        let mut w = W::default();
        w.u16(r.from_id as u16);
        w.u16(r.to_ids.len() as u16);
        for &to in &r.to_ids {
            w.u16(to as u16);
        }
        children.extend_from_slice(&boxed(&r.reference_type, &w.into_vec()));
    }
    // iref version 0 => 16-bit item ids.
    full_boxed(b"iref", 0, 0, &children)
}

fn build_iprp(
    ipco_props: &[Vec<u8>],
    items: &[MuxItem],
    item_assocs: &[Vec<(u16, bool)>],
) -> Vec<u8> {
    // ipco: concatenated property boxes.
    let mut ipco_body = Vec::new();
    for p in ipco_props {
        ipco_body.extend_from_slice(p);
    }
    let ipco = boxed(b"ipco", &ipco_body);

    // ipma: one entry per item.
    let mut w = W::default();
    w.u32(items.len() as u32); // entry_count
    for (it, assocs) in items.iter().zip(item_assocs) {
        w.u16(it.id as u16);
        w.u8(assocs.len() as u8);
        for &(idx, essential) in assocs {
            // Small form: bit 7 = essential, low 7 bits = 1-based index.
            let byte = (if essential { 0x80 } else { 0 }) | (idx as u8 & 0x7f);
            w.u8(byte);
        }
    }
    let ipma = full_boxed(b"ipma", 0, 0, &w.into_vec());

    let mut body = Vec::new();
    body.extend_from_slice(&ipco);
    body.extend_from_slice(&ipma);
    boxed(b"iprp", &body)
}

fn build_iloc(items: &[MuxItem], rel_offsets: &[u64], mdat_data_start: u64) -> Vec<u8> {
    let mut w = W::default();
    // offset_size=4, length_size=4, base_offset_size=0, index_size(reserved v0)=0.
    w.u8(0x44);
    w.u8(0x00);
    w.u16(items.len() as u16); // item_count
    for (it, &rel) in items.iter().zip(rel_offsets) {
        w.u16(it.id as u16);
        w.u16(0); // data_reference_index
                  // base_offset omitted (base_offset_size == 0).
        w.u16(1); // extent_count
        let abs = mdat_data_start + rel;
        w.u32(abs as u32); // extent_offset
        w.u32(it.payload.len() as u32); // extent_length
    }
    full_boxed(b"iloc", 0, 0, &w.into_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse, parse_header};

    /// A synthetic 4-byte av1C: marker=1 version=1, seq_profile=0
    /// level=12, 4:2:0, no presentation delay.
    fn synth_av1c() -> Vec<u8> {
        vec![0x81, 0x0c, 0x0c, 0x00]
    }

    #[test]
    fn still_round_trips_through_parse() {
        let payload = b"\x12\x00\x0a\x0bfake-av1-obu-bytes".to_vec();
        let bytes = AvifMuxer::new(64, 48, payload.clone(), synth_av1c())
            .with_pixi(vec![8, 8, 8])
            .with_colr(Colr::Nclx {
                colour_primaries: 1,
                transfer_characteristics: 13,
                matrix_coefficients: 6,
                full_range: true,
            })
            .with_pasp(Pasp {
                h_spacing: 1,
                v_spacing: 1,
            })
            .build()
            .expect("mux");

        let img = parse(&bytes).expect("parse muxed avif");
        assert_eq!(&img.major_brand, b"avif");
        assert!(img.compatible_brands.iter().any(|b| b == b"mif1"));
        assert!(img.compatible_brands.iter().any(|b| b == b"MA1B"));
        assert_eq!(img.primary_item_id, 1);
        assert_eq!(&img.primary_item.item_type, b"av01");
        // Coded payload round-trips byte-for-byte.
        assert_eq!(&*img.primary_item_data, payload.as_slice());
        // av1C round-trips.
        assert_eq!(img.av1c.as_deref(), Some(synth_av1c().as_slice()));
        let ispe = img.ispe.expect("ispe");
        assert_eq!((ispe.width, ispe.height), (64, 48));
        assert_eq!(img.pixi.unwrap().bits_per_channel, vec![8, 8, 8]);
        match img.colr.unwrap() {
            Colr::Nclx {
                colour_primaries,
                transfer_characteristics,
                matrix_coefficients,
                full_range,
            } => {
                assert_eq!(colour_primaries, 1);
                assert_eq!(transfer_characteristics, 13);
                assert_eq!(matrix_coefficients, 6);
                assert!(full_range);
            }
            _ => panic!("expected nclx"),
        }
        let pasp = img.pasp.unwrap();
        assert_eq!((pasp.h_spacing, pasp.v_spacing), (1, 1));
    }

    #[test]
    fn transforms_round_trip() {
        let payload = b"obu".to_vec();
        let bytes = AvifMuxer::new(10, 20, payload, synth_av1c())
            .with_irot(1)
            .with_imir(1)
            .build()
            .expect("mux");
        let hdr = parse_header(&bytes).expect("parse header");
        let irot = hdr.meta.property_for(1, b"irot").expect("irot");
        assert!(matches!(irot, crate::meta::Property::Irot(i) if i.angle == 1));
        let imir = hdr.meta.property_for(1, b"imir").expect("imir");
        assert!(matches!(imir, crate::meta::Property::Imir(i) if i.axis == 1));
    }

    #[test]
    fn alpha_auxiliary_round_trips() {
        let color = b"color-obu".to_vec();
        let alpha = b"alpha-obu".to_vec();
        let bytes = AvifMuxer::new(32, 32, color, synth_av1c())
            .with_pixi(vec![8, 8, 8])
            .with_alpha(alpha.clone(), synth_av1c(), false)
            .build()
            .expect("mux");
        let hdr = parse_header(&bytes).expect("parse");
        // Alpha item is id 2, hidden, linked via auxl -> primary (id 1).
        let alpha_id = crate::alpha::find_alpha_item_id(&hdr.meta, 1).expect("alpha id");
        assert_eq!(alpha_id, 2);
        let alpha_item = hdr.meta.item_by_id(2).expect("alpha item");
        assert!(alpha_item.is_hidden());
        // auxC URN classifies as alpha.
        let auxc = hdr.meta.property_for(2, b"auxC").expect("auxC");
        assert!(matches!(auxc, crate::meta::Property::AuxC(a) if a.is_alpha()));
        // Alpha payload resolvable and byte-exact.
        let loc = hdr.meta.location_by_id(2).expect("alpha iloc");
        let got = crate::parser::item_bytes(&bytes, loc).expect("alpha bytes");
        assert_eq!(got, alpha.as_slice());
    }

    #[test]
    fn premultiplied_alpha_sets_prem_iref() {
        let bytes = AvifMuxer::new(8, 8, b"c".to_vec(), synth_av1c())
            .with_alpha(b"a".to_vec(), synth_av1c(), true)
            .build()
            .expect("mux");
        let hdr = parse_header(&bytes).expect("parse");
        assert!(hdr.meta.is_alpha_premultiplied_for(1));
    }

    #[test]
    fn grid_round_trips_through_parse_header() {
        let av1c = synth_av1c();
        let mk = |n: u8| GridTile {
            width: 16,
            height: 16,
            payload: vec![n; 5],
            av1c: av1c.clone(),
        };
        let bytes = AvifGridMuxer::new(2, 2, 32, 32)
            .tile(mk(1))
            .tile(mk(2))
            .tile(mk(3))
            .tile(mk(4))
            .with_pixi(vec![8, 8, 8])
            .build()
            .expect("mux grid");
        let hdr = parse_header(&bytes).expect("parse grid header");
        assert_eq!(hdr.meta.primary_item_id, Some(1));
        let grid_item = hdr.meta.item_by_id(1).expect("grid item");
        assert_eq!(&grid_item.item_type, b"grid");
        // dimg links the grid to 4 tiles.
        let tiles = hdr.meta.iref_targets_of(b"dimg", 1);
        assert_eq!(tiles, vec![2, 3, 4, 5]);
        // Grid descriptor decodes to a 2×2 / 32×32 grid.
        let loc = hdr.meta.location_by_id(1).expect("grid iloc");
        let payload = crate::parser::item_bytes(&bytes, loc).expect("grid bytes");
        let g = crate::grid::ImageGrid::parse(payload).expect("grid parse");
        assert_eq!((g.rows, g.columns), (2, 2));
        assert_eq!((g.output_width, g.output_height), (32, 32));
        // Each tile item is hidden and carries its own av1C + ispe.
        for id in [2u32, 3, 4, 5] {
            assert!(hdr.meta.item_by_id(id).unwrap().is_hidden());
            assert!(hdr.meta.property_for(id, b"av1C").is_some());
            assert!(hdr.meta.property_for(id, b"ispe").is_some());
        }
    }

    #[test]
    fn rejects_short_av1c() {
        let err = AvifMuxer::new(1, 1, vec![0], vec![0x81])
            .build()
            .unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    #[test]
    fn grid_rejects_tile_count_mismatch() {
        let err = AvifGridMuxer::new(2, 2, 32, 32)
            .tile(GridTile {
                width: 16,
                height: 16,
                payload: vec![0],
                av1c: synth_av1c(),
            })
            .build()
            .unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    #[test]
    fn hdr_metadata_round_trips() {
        let bytes = AvifMuxer::new(8, 8, b"obu".to_vec(), synth_av1c())
            .with_pixi(vec![10, 10, 10])
            .with_mdcv(Mdcv {
                display_primaries_xy: [(34000, 16000), (13250, 34500), (7500, 3000)],
                white_point_xy: (15635, 16450),
                max_display_mastering_luminance: 10_000_000,
                min_display_mastering_luminance: 50,
            })
            .with_clli(Clli {
                max_content_light_level: 1000,
                max_pic_average_light_level: 400,
            })
            .with_amve(Amve {
                ambient_illuminance: 100_000,
                ambient_light_x: 15635,
                ambient_light_y: 16450,
            })
            .build()
            .expect("mux hdr");
        let img = parse(&bytes).expect("parse");
        let mdcv = img.mdcv.expect("mdcv");
        assert_eq!(mdcv.white_point_xy, (15635, 16450));
        assert_eq!(mdcv.max_display_mastering_luminance, 10_000_000);
        assert_eq!(mdcv.display_primaries_xy[0], (34000, 16000));
        let clli = img.clli.expect("clli");
        assert_eq!(clli.max_content_light_level, 1000);
        assert_eq!(clli.max_pic_average_light_level, 400);
        let amve = img.amve.expect("amve");
        assert_eq!(amve.ambient_illuminance, 100_000);
        assert_eq!((amve.ambient_light_x, amve.ambient_light_y), (15635, 16450));
    }

    #[test]
    fn exif_and_xmp_metadata_items_round_trip() {
        let exif = b"\x00\x00\x00\x00II*\x00exif-tiff".to_vec();
        let xmp = br#"<?xpacket?><x:xmpmeta/>"#.to_vec();
        let bytes = AvifMuxer::new(16, 16, b"obu".to_vec(), synth_av1c())
            .with_pixi(vec![8, 8, 8])
            .with_exif(exif.clone())
            .with_xmp(xmp.clone())
            .build()
            .expect("mux metadata");
        let info = crate::inspect::inspect(&bytes).expect("inspect");
        assert!(info.has_descriptive_metadata());
        let exif_id = info.exif_item_id.expect("exif item");
        let xmp_id = info.xmp_item_id.expect("xmp item");
        // Payloads resolve byte-for-byte through the cdsc-linked items.
        let got_exif = crate::inspect::item_payload_bytes(&bytes, exif_id).expect("exif");
        assert_eq!(got_exif, exif);
        let got_xmp = crate::inspect::item_payload_bytes(&bytes, xmp_id).expect("xmp");
        assert_eq!(got_xmp, xmp);
    }

    #[test]
    fn advanced_profile_sets_ma1a_brand() {
        let bytes = AvifMuxer::new(8, 8, b"obu".to_vec(), synth_av1c())
            .advanced_profile()
            .build()
            .expect("mux");
        let img = parse(&bytes).expect("parse");
        assert!(img.compatible_brands.iter().any(|b| b == b"MA1A"));
        assert!(!img.compatible_brands.iter().any(|b| b == b"MA1B"));
    }

    #[test]
    fn wide_grid_emits_32bit_descriptor() {
        let av1c = synth_av1c();
        let mut m = AvifGridMuxer::new(1, 2, 70_000, 8);
        for _ in 0..2 {
            m = m.tile(GridTile {
                width: 35_000,
                height: 8,
                payload: vec![7; 4],
                av1c: av1c.clone(),
            });
        }
        let bytes = m.build().expect("mux wide grid");
        let hdr = parse_header(&bytes).expect("parse");
        let loc = hdr.meta.location_by_id(1).expect("grid iloc");
        let payload = crate::parser::item_bytes(&bytes, loc).expect("grid bytes");
        let g = crate::grid::ImageGrid::parse(payload).expect("grid parse");
        assert_eq!((g.output_width, g.output_height), (70_000, 8));
        assert_eq!((g.rows, g.columns), (1, 2));
    }
}
