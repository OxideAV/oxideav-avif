//! AVIF container muxer — the AV1-profile item layout over the
//! container crate's writer ([`oxideav_heif::HeifWriter`]).
//!
//! Given already-coded AV1 Image Item Data payloads plus their `av1C`
//! records, [`AvifMuxer`] / [`AvifGridMuxer`] / [`AvifOverlayMuxer`] /
//! [`encode_still_av1`] emit a conformant AVIF file. What this module
//! decides is the AVIF side of the layout: the `ftyp` brands
//! (`avif` / `mif1` / `miaf` + the av1-avif §8 profile brand `MA1B` /
//! `MA1A`), the item kinds and their order (primary `av01` first, so
//! the primary is item 1; a `grid` / `iovl` before its inputs; the
//! alpha auxiliary right after its master), the property set of every
//! item (`av1C` essential, `ispe`, `pixi`, `colr`, `pasp`, the HDR
//! metadata, `a1op` / `lsel` / `a1lx`, `clap` / `irot` / `imir`), the
//! alpha / depth `auxC` URNs, and the descriptor bodies. Box
//! serialisation — `meta` tree, `iloc` / `idat` placement (derived
//! item bodies ride `idat`, construction method 1), `ipco`
//! de-duplication, `ipma`, `iref`, MIAF `mdat` ordering — is the
//! container's.
//!
//! The output round-trips through this crate's own [`crate::parse`]
//! path byte-for-byte on every coded payload and property (see the
//! tests) and passes [`crate::audit_mif1`]. A `prem` reference is
//! written from the master to its alpha auxiliary (HEIF 3rd ed.
//! §6.9.1); the reader accepts either direction.

use oxideav_heif::derived::{GridDescriptor, OverlayDescriptor};
use oxideav_heif::props::{self as hprops, Property as HProp};
use oxideav_heif::{Av1Config, HeifWriter, RawProperty};

use crate::error::{AvifError as Error, Result};
use crate::meta::{Amve, Clap, Clli, Colr, Imir, Irot, Mdcv, Pasp};

/// One property with its `essential` flag, as the container writer
/// takes it.
type Prop = (HProp, bool);

fn prop_av1c(av1c: &[u8], what: &str) -> Result<Prop> {
    if av1c.len() < 4 {
        return Err(Error::invalid(format!(
            "avif mux: {what}av1C configuration record must be at least 4 bytes"
        )));
    }
    Ok((HProp::Av1C(Av1Config::parse(av1c)?), true))
}

fn prop_ispe(width: u32, height: u32) -> Prop {
    (HProp::Ispe(hprops::Ispe { width, height }), false)
}

fn prop_pixi(bits: &[u8]) -> Prop {
    (
        HProp::Pixi(hprops::Pixi {
            bits_per_channel: bits.to_vec(),
        }),
        false,
    )
}

fn prop_colr(colr: &Colr) -> Result<Prop> {
    if let Colr::Unknown(t) = colr {
        return Err(Error::unsupported(format!(
            "avif mux: cannot emit colr of unknown type '{}'",
            String::from_utf8_lossy(t)
        )));
    }
    Ok((HProp::Colr(hprops::Colr::from(colr)), false))
}

fn prop_pasp(pasp: &Pasp) -> Prop {
    (
        HProp::Pasp(hprops::Pasp {
            h_spacing: pasp.h_spacing,
            v_spacing: pasp.v_spacing,
        }),
        false,
    )
}

fn prop_clap(clap: &Clap) -> Prop {
    (HProp::Clap(hprops::Clap::from(clap)), true)
}

fn prop_irot(irot: &Irot) -> Prop {
    (
        HProp::Irot(hprops::Irot {
            angle: irot.angle & 0x03,
        }),
        true,
    )
}

fn prop_imir(imir: &Imir) -> Prop {
    (
        HProp::Imir(hprops::Imir {
            axis: imir.axis & 0x01,
        }),
        true,
    )
}

fn prop_auxc(urn: &str) -> Prop {
    (
        HProp::AuxC(hprops::AuxC {
            aux_type: urn.to_string(),
            aux_subtype: Vec::new(),
        }),
        false,
    )
}

fn prop_a1lx(layer_size: [u32; 3]) -> Prop {
    let large_size = layer_size.iter().any(|&v| v > u32::from(u16::MAX));
    (
        HProp::A1lx(hprops::A1lx {
            large_size,
            layer_size,
        }),
        false,
    )
}

fn prop_lsel(layer_id: u16) -> Prop {
    (HProp::Lsel(hprops::Lsel { layer_id }), true)
}

fn prop_a1op(op_index: u8) -> Prop {
    (HProp::A1op(hprops::A1op { op_index }), true)
}

/// `mdcv` in the layout this crate reads (ISO/IEC 14496-12
/// MasteringDisplayColourVolumeBox: three `(x, y)` primaries pairs in
/// file order, the white point, then the max / min luminance) — kept
/// as raw bytes because the container types the primaries in a
/// different field order.
fn prop_mdcv(m: &Mdcv) -> Prop {
    let mut body = Vec::with_capacity(24);
    for (x, y) in m.display_primaries_xy {
        body.extend_from_slice(&x.to_be_bytes());
        body.extend_from_slice(&y.to_be_bytes());
    }
    body.extend_from_slice(&m.white_point_xy.0.to_be_bytes());
    body.extend_from_slice(&m.white_point_xy.1.to_be_bytes());
    body.extend_from_slice(&m.max_display_mastering_luminance.to_be_bytes());
    body.extend_from_slice(&m.min_display_mastering_luminance.to_be_bytes());
    (
        HProp::Unknown(RawProperty {
            box_type: *b"mdcv",
            user_type: None,
            box_size: body.len() + 8,
            body,
        }),
        false,
    )
}

fn prop_clli(c: &Clli) -> Prop {
    (HProp::Clli(hprops::Clli::from(c)), false)
}

fn prop_amve(a: &Amve) -> Prop {
    (HProp::Amve(hprops::Amve::from(a)), false)
}

/// The av1-avif §8 profile brand carried in `ftyp`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileBrand {
    Baseline,
    Advanced,
    Bare,
}

/// `ftyp` of an AVIF image: major `avif`, compatible `avif` / `mif1` /
/// `miaf` and the elected profile brand.
fn still_brands(profile: ProfileBrand) -> ([u8; 4], Vec<[u8; 4]>) {
    let mut compat = vec![*b"avif", *b"mif1", *b"miaf"];
    match profile {
        ProfileBrand::Baseline => compat.push(*b"MA1B"),
        ProfileBrand::Advanced => compat.push(*b"MA1A"),
        ProfileBrand::Bare => {}
    }
    (*b"avif", compat)
}

/// Add an Exif metadata item from its item body (HEIF Annex A.2.1: a
/// 4-byte `exif_tiff_header_offset` followed by the Exif payload). The
/// container writer takes the payload and prepends a zero offset
/// itself, so only a body whose offset word is zero can be expressed.
fn add_exif_item(w: &mut HeifWriter, image: u32, payload: &[u8]) -> Result<u32> {
    match payload.split_first_chunk::<4>() {
        Some(([0, 0, 0, 0], tiff)) => Ok(w.add_exif(image, tiff)),
        _ => Err(Error::unsupported(
            "avif mux: Exif item body must start with a zero exif_tiff_header_offset word \
             (the container writer prepends its own)",
        )),
    }
}

/// Add an XMP metadata item (`mime` / `application/rdf+xml`).
fn add_xmp_item(w: &mut HeifWriter, image: u32, payload: &[u8]) -> Result<u32> {
    let xmp = std::str::from_utf8(payload)
        .map_err(|_| Error::unsupported("avif mux: XMP packet must be UTF-8"))?;
    Ok(w.add_xmp(image, xmp))
}

/// Guard: the container assigns item ids in insertion order; the AVIF
/// layout predicts them (a derived primary names its inputs before
/// they exist). A mismatch is a bug, never silent.
fn expect_id(got: u32, want: u32, what: &str) -> Result<()> {
    if got != want {
        return Err(Error::invalid(format!(
            "avif mux: {what} got item id {got}, expected {want}"
        )));
    }
    Ok(())
}

/// Builder for a single-item AVIF file: one `av01` primary plus the
/// optional alpha / depth auxiliaries, metadata items, an `iden`
/// wrapper and the AVIF profile brand.
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
    depth: Option<AuxCoded>,
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    identity: Option<IdentityDerivation>,
    layered_index: Option<[u32; 3]>,
    layer_selector: Option<u16>,
    operating_point: Option<u8>,
    profile_brand: ProfileBrand,
}

/// Make the primary an `iden` derived item (HEIF §6.6.2.1) over the
/// coded `av01` item, carrying these transformative properties.
#[derive(Clone, Debug, Default)]
pub struct IdentityDerivation {
    /// `clap` on the identity item.
    pub clap: Option<Clap>,
    /// `irot` angle on the identity item.
    pub irot: Option<u8>,
    /// `imir` axis on the identity item.
    pub imir: Option<u8>,
}

struct AuxCoded {
    payload: Vec<u8>,
    av1c: Vec<u8>,
    pixi: Option<Vec<u8>>,
}

struct AlphaImage {
    payload: Vec<u8>,
    av1c: Vec<u8>,
    pixi: Option<Vec<u8>>,
    premultiplied: bool,
}

impl AvifMuxer {
    /// A primary `av01` item of `width × height` with its coded payload
    /// and `av1C` record.
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
            depth: None,
            exif: None,
            xmp: None,
            identity: None,
            layered_index: None,
            layer_selector: None,
            operating_point: None,
            profile_brand: ProfileBrand::Baseline,
        }
    }

    /// `pixi` bits per channel.
    pub fn with_pixi(mut self, bits: Vec<u8>) -> Self {
        self.pixi = Some(bits);
        self
    }

    /// `colr` colour information.
    pub fn with_colr(mut self, colr: Colr) -> Self {
        self.colr = Some(colr);
        self
    }

    /// `pasp` pixel aspect ratio.
    pub fn with_pasp(mut self, pasp: Pasp) -> Self {
        self.pasp = Some(pasp);
        self
    }

    /// `clap` clean aperture (essential).
    pub fn with_clap(mut self, clap: Clap) -> Self {
        self.clap = Some(clap);
        self
    }

    /// `irot` rotation (essential).
    pub fn with_irot(mut self, angle: u8) -> Self {
        self.irot = Some(Irot {
            angle: angle & 0x03,
        });
        self
    }

    /// `imir` mirror (essential).
    pub fn with_imir(mut self, axis: u8) -> Self {
        self.imir = Some(Imir { axis: axis & 0x01 });
        self
    }

    /// `mdcv` mastering display colour volume.
    pub fn with_mdcv(mut self, mdcv: Mdcv) -> Self {
        self.mdcv = Some(mdcv);
        self
    }

    /// `clli` content light level.
    pub fn with_clli(mut self, clli: Clli) -> Self {
        self.clli = Some(clli);
        self
    }

    /// `amve` ambient viewing environment.
    pub fn with_amve(mut self, amve: Amve) -> Self {
        self.amve = Some(amve);
        self
    }

    /// An `Exif` metadata item (item body: 4-byte offset word + payload).
    pub fn with_exif(mut self, payload: Vec<u8>) -> Self {
        self.exif = Some(payload);
        self
    }

    /// An XMP metadata item (`mime` / `application/rdf+xml`).
    pub fn with_xmp(mut self, payload: Vec<u8>) -> Self {
        self.xmp = Some(payload);
        self
    }

    /// Declare the AVIF Advanced profile (`MA1A`) instead of Baseline.
    pub fn advanced_profile(mut self) -> Self {
        self.profile_brand = ProfileBrand::Advanced;
        self
    }

    /// Declare no AVIF profile brand (general brands only).
    pub fn no_profile_brand(mut self) -> Self {
        self.profile_brand = ProfileBrand::Bare;
        self
    }

    /// An AV1-coded alpha auxiliary (`auxC` alpha URN + `auxl`; `prem`
    /// when `premultiplied`).
    pub fn with_alpha(mut self, payload: Vec<u8>, av1c: Vec<u8>, premultiplied: bool) -> Self {
        self.alpha = Some(AlphaImage {
            payload,
            av1c,
            pixi: Some(vec![8]),
            premultiplied,
        });
        self
    }

    /// `pixi` of the alpha auxiliary (default one 8-bit channel).
    pub fn with_alpha_pixi(mut self, bits: Vec<u8>) -> Self {
        if let Some(a) = self.alpha.as_mut() {
            a.pixi = Some(bits);
        }
        self
    }

    /// An AV1-coded depth-map auxiliary.
    pub fn with_depth(mut self, payload: Vec<u8>, av1c: Vec<u8>) -> Self {
        self.depth = Some(AuxCoded {
            payload,
            av1c,
            pixi: Some(vec![8]),
        });
        self
    }

    /// `a1lx` layer sizes of a layered (progressive) item.
    pub fn with_layered_index(mut self, layer_size: [u32; 3]) -> Self {
        self.layered_index = Some(layer_size);
        self
    }

    /// `lsel` layer selector (essential).
    pub fn with_layer_selector(mut self, layer_id: u16) -> Self {
        self.layer_selector = Some(layer_id);
        self
    }

    /// `a1op` operating point selector (essential).
    pub fn with_operating_point(mut self, op_index: u8) -> Self {
        self.operating_point = Some(op_index);
        self
    }

    /// Make the primary an `iden` item over the coded item.
    pub fn with_identity_derivation(mut self, derivation: IdentityDerivation) -> Self {
        self.identity = Some(derivation);
        self
    }

    /// Serialise the file.
    pub fn build(self) -> Result<Vec<u8>> {
        let (major, compat) = still_brands(self.profile_brand);
        let mut w = HeifWriter::new().with_brands(major, compat);
        let mut props = vec![
            prop_av1c(&self.av1c, "")?,
            prop_ispe(self.width, self.height),
        ];
        if let Some(op) = self.operating_point {
            props.push(prop_a1op(op));
        }
        if let Some(layer) = self.layer_selector {
            props.push(prop_lsel(layer));
        }
        if let Some(sizes) = self.layered_index {
            props.push(prop_a1lx(sizes));
        }
        if let Some(bits) = &self.pixi {
            props.push(prop_pixi(bits));
        }
        if let Some(colr) = &self.colr {
            props.push(prop_colr(colr)?);
        }
        if let Some(pasp) = &self.pasp {
            props.push(prop_pasp(pasp));
        }
        if let Some(mdcv) = &self.mdcv {
            props.push(prop_mdcv(mdcv));
        }
        if let Some(clli) = &self.clli {
            props.push(prop_clli(clli));
        }
        if let Some(amve) = &self.amve {
            props.push(prop_amve(amve));
        }
        if let Some(clap) = &self.clap {
            props.push(prop_clap(clap));
        }
        if let Some(irot) = &self.irot {
            props.push(prop_irot(irot));
        }
        if let Some(imir) = &self.imir {
            props.push(prop_imir(imir));
        }
        let coded = w.add_coded_item(*b"av01", self.primary_payload, props);
        expect_id(coded, 1, "primary")?;
        let mut primary_id = coded;
        if let Some(iden) = self.identity {
            // The identity item's `ispe` is the coded item's output size.
            let (mut iw, mut ih) = (self.width, self.height);
            if let Some(clap) = &self.clap {
                (iw, ih) = crate::derived::DimTransform::Crop {
                    width_n: clap.clean_aperture_width_n,
                    width_d: clap.clean_aperture_width_d,
                    height_n: clap.clean_aperture_height_n,
                    height_d: clap.clean_aperture_height_d,
                }
                .apply_dims(iw, ih);
            }
            if let Some(irot) = &self.irot {
                (iw, ih) =
                    crate::derived::DimTransform::Rotate { angle: irot.angle }.apply_dims(iw, ih);
            }
            let mut iprops = vec![prop_ispe(iw, ih)];
            if let Some(bits) = &self.pixi {
                iprops.push(prop_pixi(bits));
            }
            if let Some(clap) = &iden.clap {
                iprops.push(prop_clap(clap));
            }
            if let Some(angle) = iden.irot {
                iprops.push(prop_irot(&Irot {
                    angle: angle & 0x03,
                }));
            }
            if let Some(axis) = iden.imir {
                iprops.push(prop_imir(&Imir { axis: axis & 0x01 }));
            }
            primary_id = w.add_identity(coded, iprops);
        }
        if let Some(alpha) = self.alpha {
            let mut aprops = vec![
                prop_av1c(&alpha.av1c, "alpha ")?,
                prop_ispe(self.width, self.height),
                prop_auxc(crate::alpha::ALPHA_URN_PREFIX),
            ];
            if let Some(bits) = &alpha.pixi {
                aprops.push(prop_pixi(bits));
            }
            let id = w.add_alpha(coded, *b"av01", alpha.payload, aprops, alpha.premultiplied);
            w.set_hidden(id, true);
        }
        if let Some(depth) = self.depth {
            let mut dprops = vec![
                prop_av1c(&depth.av1c, "depth ")?,
                prop_ispe(self.width, self.height),
                prop_auxc(crate::meta::AUX_URN_DEPTH_MPEG),
            ];
            if let Some(bits) = &depth.pixi {
                dprops.push(prop_pixi(bits));
            }
            let id = w.add_depth(coded, *b"av01", depth.payload, dprops);
            w.set_hidden(id, true);
        }
        if let Some(exif) = &self.exif {
            add_exif_item(&mut w, coded, exif)?;
        }
        if let Some(xmp) = &self.xmp {
            add_xmp_item(&mut w, coded, xmp)?;
        }
        w.set_primary(primary_id);
        Ok(w.write_to_vec()?)
    }
}

/// One-shot: a single `av01` primary with `pixi`.
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

/// One coded tile of a grid.
pub struct GridTile {
    /// Coded tile width.
    pub width: u32,
    /// Coded tile height.
    pub height: u32,
    /// AV1 Image Item Data.
    pub payload: Vec<u8>,
    /// `av1C` record.
    pub av1c: Vec<u8>,
}

/// Builder for a `grid` primary (HEIF §6.6.2.3) over hidden `av01`
/// tiles, with an optional alpha grid of the same geometry.
pub struct AvifGridMuxer {
    rows: u16,
    columns: u16,
    output_width: u32,
    output_height: u32,
    tiles: Vec<GridTile>,
    pixi: Option<Vec<u8>>,
    colr: Option<Colr>,
    pasp: Option<Pasp>,
    irot: Option<Irot>,
    imir: Option<Imir>,
    mdcv: Option<Mdcv>,
    clli: Option<Clli>,
    amve: Option<Amve>,
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    alpha_tiles: Vec<GridTile>,
    alpha_pixi: Option<Vec<u8>>,
    premultiplied: bool,
    profile_brand: ProfileBrand,
}

impl AvifGridMuxer {
    /// A `rows × columns` grid whose output image is
    /// `output_width × output_height`.
    pub fn new(rows: u16, columns: u16, output_width: u32, output_height: u32) -> Self {
        Self {
            rows,
            columns,
            output_width,
            output_height,
            tiles: Vec::new(),
            pixi: None,
            colr: None,
            pasp: None,
            irot: None,
            imir: None,
            mdcv: None,
            clli: None,
            amve: None,
            exif: None,
            xmp: None,
            alpha_tiles: Vec::new(),
            alpha_pixi: None,
            premultiplied: false,
            profile_brand: ProfileBrand::Baseline,
        }
    }

    /// Append a colour tile (row-major order).
    pub fn tile(mut self, tile: GridTile) -> Self {
        self.tiles.push(tile);
        self
    }

    /// Append an alpha tile (row-major order, same geometry as the
    /// colour tiles).
    pub fn alpha_tile(mut self, tile: GridTile) -> Self {
        self.alpha_tiles.push(tile);
        self
    }

    /// `pixi` of the alpha grid.
    pub fn with_alpha_pixi(mut self, bits: Vec<u8>) -> Self {
        self.alpha_pixi = Some(bits);
        self
    }

    /// Signal the colour grid as pre-multiplied by its alpha (`prem`).
    pub fn premultiplied_alpha(mut self, premultiplied: bool) -> Self {
        self.premultiplied = premultiplied;
        self
    }

    /// `pasp` on the grid item.
    pub fn with_pasp(mut self, pasp: Pasp) -> Self {
        self.pasp = Some(pasp);
        self
    }

    /// `irot` on the grid item (essential).
    pub fn with_irot(mut self, angle: u8) -> Self {
        self.irot = Some(Irot {
            angle: angle & 0x03,
        });
        self
    }

    /// `imir` on the grid item (essential).
    pub fn with_imir(mut self, axis: u8) -> Self {
        self.imir = Some(Imir { axis: axis & 0x01 });
        self
    }

    /// `mdcv` on the grid item.
    pub fn with_mdcv(mut self, mdcv: Mdcv) -> Self {
        self.mdcv = Some(mdcv);
        self
    }

    /// `clli` on the grid item.
    pub fn with_clli(mut self, clli: Clli) -> Self {
        self.clli = Some(clli);
        self
    }

    /// `amve` on the grid item.
    pub fn with_amve(mut self, amve: Amve) -> Self {
        self.amve = Some(amve);
        self
    }

    /// An `Exif` metadata item describing the grid.
    pub fn with_exif(mut self, payload: Vec<u8>) -> Self {
        self.exif = Some(payload);
        self
    }

    /// An XMP metadata item describing the grid.
    pub fn with_xmp(mut self, payload: Vec<u8>) -> Self {
        self.xmp = Some(payload);
        self
    }

    /// `pixi` on the grid item.
    pub fn with_pixi(mut self, bits: Vec<u8>) -> Self {
        self.pixi = Some(bits);
        self
    }

    /// `colr` on the grid item.
    pub fn with_colr(mut self, colr: Colr) -> Self {
        self.colr = Some(colr);
        self
    }

    /// Declare the AVIF Advanced profile (`MA1A`).
    pub fn advanced_profile(mut self) -> Self {
        self.profile_brand = ProfileBrand::Advanced;
        self
    }

    /// Declare no AVIF profile brand.
    pub fn no_profile_brand(mut self) -> Self {
        self.profile_brand = ProfileBrand::Bare;
        self
    }

    /// Serialise the file.
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
        if !self.alpha_tiles.is_empty() && self.alpha_tiles.len() != expected {
            return Err(Error::invalid(format!(
                "avif mux: {}×{} alpha grid needs {expected} tiles, got {}",
                self.rows,
                self.columns,
                self.alpha_tiles.len()
            )));
        }
        let (major, compat) = still_brands(self.profile_brand);
        let mut w = HeifWriter::new().with_brands(major, compat);
        let desc = GridDescriptor {
            rows: self.rows,
            columns: self.columns,
            output_width: self.output_width,
            output_height: self.output_height,
        };
        let mut grid_props = vec![prop_ispe(self.output_width, self.output_height)];
        if let Some(bits) = &self.pixi {
            grid_props.push(prop_pixi(bits));
        }
        if let Some(colr) = &self.colr {
            grid_props.push(prop_colr(colr)?);
        }
        if let Some(pasp) = &self.pasp {
            grid_props.push(prop_pasp(pasp));
        }
        if let Some(mdcv) = &self.mdcv {
            grid_props.push(prop_mdcv(mdcv));
        }
        if let Some(clli) = &self.clli {
            grid_props.push(prop_clli(clli));
        }
        if let Some(amve) = &self.amve {
            grid_props.push(prop_amve(amve));
        }
        if let Some(irot) = &self.irot {
            grid_props.push(prop_irot(irot));
        }
        if let Some(imir) = &self.imir {
            grid_props.push(prop_imir(imir));
        }
        // Layout: grid = 1, tiles 2..=n+1, then the alpha grid and its
        // tiles, then the metadata items.
        let n = self.tiles.len() as u32;
        let tile_ids: Vec<u32> = (2..2 + n).collect();
        let grid_id = w.add_grid(desc, &tile_ids, grid_props)?;
        expect_id(grid_id, 1, "grid")?;
        for (i, tile) in self.tiles.into_iter().enumerate() {
            let id = w.add_coded_item(
                *b"av01",
                tile.payload,
                vec![
                    prop_av1c(&tile.av1c, "grid tile ")?,
                    prop_ispe(tile.width, tile.height),
                ],
            );
            expect_id(id, tile_ids[i], "grid tile")?;
            w.set_hidden(id, true);
        }
        if !self.alpha_tiles.is_empty() {
            let alpha_grid_id = 2 + n;
            let alpha_tile_ids: Vec<u32> = (alpha_grid_id + 1..alpha_grid_id + 1 + n).collect();
            let mut aprops = vec![
                prop_ispe(self.output_width, self.output_height),
                prop_auxc(crate::alpha::ALPHA_URN_PREFIX),
            ];
            if let Some(bits) = &self.alpha_pixi {
                aprops.push(prop_pixi(bits));
            }
            let ag = w.add_grid(desc, &alpha_tile_ids, aprops)?;
            expect_id(ag, alpha_grid_id, "alpha grid")?;
            w.set_hidden(ag, true);
            for (i, tile) in self.alpha_tiles.into_iter().enumerate() {
                let id = w.add_coded_item(
                    *b"av01",
                    tile.payload,
                    vec![
                        prop_av1c(&tile.av1c, &format!("alpha grid tile {i} "))?,
                        prop_ispe(tile.width, tile.height),
                    ],
                );
                expect_id(id, alpha_tile_ids[i], "alpha grid tile")?;
                w.set_hidden(id, true);
            }
            w.add_reference(*b"auxl", ag, vec![grid_id]);
            if self.premultiplied {
                w.add_reference(*b"prem", grid_id, vec![ag]);
            }
        }
        if let Some(exif) = &self.exif {
            add_exif_item(&mut w, grid_id, exif)?;
        }
        if let Some(xmp) = &self.xmp {
            add_xmp_item(&mut w, grid_id, xmp)?;
        }
        w.set_primary(grid_id);
        Ok(w.write_to_vec()?)
    }
}

/// One layer of an overlay: a hidden `av01` item placed at
/// `(offset_x, offset_y)`, with an optional alpha auxiliary.
pub struct OverlayLayer {
    /// Coded width.
    pub width: u32,
    /// Coded height.
    pub height: u32,
    /// AV1 Image Item Data.
    pub payload: Vec<u8>,
    /// `av1C` record.
    pub av1c: Vec<u8>,
    /// `pixi` of the layer.
    pub pixi: Option<Vec<u8>>,
    /// `colr` of the layer.
    pub colr: Option<Colr>,
    /// `clap` on the layer (essential).
    pub clap: Option<Clap>,
    /// Alpha auxiliary `(payload, av1C)`.
    pub alpha: Option<(Vec<u8>, Vec<u8>)>,
    /// `pixi` of the alpha auxiliary.
    pub alpha_pixi: Option<Vec<u8>>,
    /// `prem`: the layer is pre-multiplied by its alpha.
    pub premultiplied: bool,
    /// Horizontal offset on the canvas.
    pub offset_x: i32,
    /// Vertical offset on the canvas.
    pub offset_y: i32,
}

/// Builder for an `iovl` primary (HEIF §6.6.2.2) over hidden `av01`
/// layers.
pub struct AvifOverlayMuxer {
    output_width: u32,
    output_height: u32,
    fill: [u16; 4],
    layers: Vec<OverlayLayer>,
    pixi: Option<Vec<u8>>,
    colr: Option<Colr>,
    irot: Option<Irot>,
    imir: Option<Imir>,
    profile_brand: ProfileBrand,
}

impl AvifOverlayMuxer {
    /// An overlay canvas of `output_width × output_height` (opaque
    /// black fill by default).
    pub fn new(output_width: u32, output_height: u32) -> Self {
        Self {
            output_width,
            output_height,
            fill: [0, 0, 0, 65535],
            layers: Vec::new(),
            pixi: None,
            colr: None,
            irot: None,
            imir: None,
            profile_brand: ProfileBrand::Baseline,
        }
    }

    /// `canvas_fill_value` (sRGB R, G, B and opacity, 16-bit each).
    pub fn with_fill(mut self, rgba: [u16; 4]) -> Self {
        self.fill = rgba;
        self
    }

    /// Append a layer (bottom-most first).
    pub fn layer(mut self, layer: OverlayLayer) -> Self {
        self.layers.push(layer);
        self
    }

    /// `pixi` on the overlay item.
    pub fn with_pixi(mut self, bits: Vec<u8>) -> Self {
        self.pixi = Some(bits);
        self
    }

    /// `colr` on the overlay item.
    pub fn with_colr(mut self, colr: Colr) -> Self {
        self.colr = Some(colr);
        self
    }

    /// `irot` on the overlay item (essential).
    pub fn with_irot(mut self, angle: u8) -> Self {
        self.irot = Some(Irot {
            angle: angle & 0x03,
        });
        self
    }

    /// `imir` on the overlay item (essential).
    pub fn with_imir(mut self, axis: u8) -> Self {
        self.imir = Some(Imir { axis: axis & 0x01 });
        self
    }

    /// Declare the AVIF Advanced profile (`MA1A`).
    pub fn advanced_profile(mut self) -> Self {
        self.profile_brand = ProfileBrand::Advanced;
        self
    }

    /// Declare no AVIF profile brand.
    pub fn no_profile_brand(mut self) -> Self {
        self.profile_brand = ProfileBrand::Bare;
        self
    }

    /// Serialise the file.
    pub fn build(self) -> Result<Vec<u8>> {
        if self.layers.is_empty() {
            return Err(Error::invalid("avif mux: overlay needs at least one layer"));
        }
        if self.output_width == 0 || self.output_height == 0 {
            return Err(Error::invalid("avif mux: overlay canvas dimensions zero"));
        }
        let (major, compat) = still_brands(self.profile_brand);
        let mut w = HeifWriter::new().with_brands(major, compat);
        let desc = OverlayDescriptor {
            canvas_fill: self.fill,
            output_width: self.output_width,
            output_height: self.output_height,
            offsets: self
                .layers
                .iter()
                .map(|l| (l.offset_x, l.offset_y))
                .collect(),
        };
        let mut props = vec![prop_ispe(self.output_width, self.output_height)];
        if let Some(bits) = &self.pixi {
            props.push(prop_pixi(bits));
        }
        if let Some(colr) = &self.colr {
            props.push(prop_colr(colr)?);
        }
        if let Some(irot) = &self.irot {
            props.push(prop_irot(irot));
        }
        if let Some(imir) = &self.imir {
            props.push(prop_imir(imir));
        }
        // Layout: overlay = 1, then per layer its item and (right
        // after) its alpha auxiliary.
        let mut layer_ids = Vec::with_capacity(self.layers.len());
        let mut next = 2u32;
        for layer in &self.layers {
            layer_ids.push(next);
            next += 1 + u32::from(layer.alpha.is_some());
        }
        let ov = w.add_overlay(desc, &layer_ids, props)?;
        expect_id(ov, 1, "overlay")?;
        for (i, layer) in self.layers.into_iter().enumerate() {
            let mut lprops = vec![
                prop_av1c(&layer.av1c, &format!("overlay layer {i} "))?,
                prop_ispe(layer.width, layer.height),
            ];
            if let Some(bits) = &layer.pixi {
                lprops.push(prop_pixi(bits));
            }
            if let Some(colr) = &layer.colr {
                lprops.push(prop_colr(colr)?);
            }
            if let Some(clap) = &layer.clap {
                lprops.push(prop_clap(clap));
            }
            let id = w.add_coded_item(*b"av01", layer.payload, lprops);
            expect_id(id, layer_ids[i], "overlay layer")?;
            w.set_hidden(id, true);
            if let Some((apayload, aav1c)) = layer.alpha {
                let mut aprops = vec![
                    prop_av1c(&aav1c, &format!("overlay layer {i} alpha "))?,
                    prop_ispe(layer.width, layer.height),
                    prop_auxc(crate::alpha::ALPHA_URN_PREFIX),
                ];
                if let Some(bits) = &layer.alpha_pixi {
                    aprops.push(prop_pixi(bits));
                }
                if let Some(clap) = &layer.clap {
                    aprops.push(prop_clap(clap));
                }
                let aid = w.add_alpha(id, *b"av01", apayload, aprops, layer.premultiplied);
                w.set_hidden(aid, true);
            }
        }
        w.set_primary(ov);
        Ok(w.write_to_vec()?)
    }
}

/// The `ftyp` brand list of an AVIF image sequence (av1-avif §6.3 +
/// AV1-ISOBMFF §2.1): major `avis`, compatible `avis` / `avif` /
/// `mif1` / `msf1` / `miaf` / `av01`, `avio` when every sample is a
/// sync sample, and the profile brand.
#[cfg(feature = "registry")]
pub(crate) fn sequence_brands(profile: ProfileBrand, all_sync: bool) -> ([u8; 4], Vec<[u8; 4]>) {
    let mut compat = vec![*b"avis", *b"avif", *b"mif1", *b"msf1", *b"miaf", *b"av01"];
    if all_sync {
        compat.push(*b"avio");
    }
    match profile {
        ProfileBrand::Baseline => compat.push(*b"MA1B"),
        ProfileBrand::Advanced => compat.push(*b"MA1A"),
        ProfileBrand::Bare => {}
    }
    (*b"avis", compat)
}

/// The cover still of an image sequence: sample 0's coded bytes as one
/// `av01` item with the track's properties.
#[cfg(feature = "registry")]
pub(crate) struct CoverStill<'a> {
    /// `ftyp` brands of the sequence.
    pub brands: ([u8; 4], Vec<[u8; 4]>),
    /// Coded extents.
    pub width: u32,
    /// Coded extents.
    pub height: u32,
    /// Sample 0.
    pub payload: Vec<u8>,
    /// `av1C` record.
    pub av1c: &'a [u8],
    /// `pixi` bits per channel.
    pub pixi: &'a [u8],
    /// `colr`, when the sequence carries one.
    pub colr: Option<&'a Colr>,
    /// `clap`, when the coded extents exceed the picture.
    pub clap: Option<&'a Clap>,
}

/// Write the cover still of an image sequence through the container:
/// the sequence writer appends its `moov` and the remaining samples so
/// the primary item aliases sample 0. Returns the file bytes and the
/// primary's payload span inside them.
#[cfg(feature = "registry")]
pub(crate) fn sequence_cover_still(cover: CoverStill<'_>) -> Result<(Vec<u8>, (usize, usize))> {
    let CoverStill {
        brands,
        width,
        height,
        payload,
        av1c,
        pixi,
        colr,
        clap,
    } = cover;
    let mut w = HeifWriter::new().with_brands(brands.0, brands.1);
    let mut props = vec![
        prop_av1c(av1c, "sequence ")?,
        prop_ispe(width, height),
        prop_pixi(pixi),
    ];
    if let Some(c) = colr {
        props.push(prop_colr(c)?);
    }
    if let Some(c) = clap {
        props.push(prop_clap(c));
    }
    let id = w.add_coded_item(*b"av01", payload, props);
    w.set_primary(id);
    let bytes = w.write_to_vec()?;
    let file = oxideav_heif::HeifFile::parse(&bytes)?;
    let spans = file.item_file_spans(id)?;
    match spans.as_slice() {
        [span] => Ok((bytes, *span)),
        _ => Err(Error::invalid(
            "avif mux: sequence cover still is not one contiguous span",
        )),
    }
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
        assert!(hdr.meta.location_by_id(1).is_some(), "grid iloc");
        let payload = hdr.item_data(1).expect("grid bytes");
        let g = crate::grid::ImageGrid::parse(&payload).expect("grid parse");
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
    fn depth_auxiliary_round_trips() {
        let bytes = AvifMuxer::new(24, 24, b"color".to_vec(), synth_av1c())
            .with_pixi(vec![8, 8, 8])
            .with_depth(b"depth-obu".to_vec(), synth_av1c())
            .build()
            .expect("mux depth");
        let info = crate::inspect::inspect(&bytes).expect("inspect");
        assert!(info.has_depth_map());
        let depth_id = info.depth_map_item_id.expect("depth id");
        let hdr = parse_header(&bytes).expect("parse");
        assert!(hdr.meta.item_by_id(depth_id).unwrap().is_hidden());
        let auxc = hdr.meta.property_for(depth_id, b"auxC").expect("auxC");
        assert!(matches!(auxc, crate::meta::Property::AuxC(a) if a.is_depth_map()));
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
    fn no_profile_brand_omits_ma1a_and_ma1b() {
        let bytes = AvifMuxer::new(8, 8, b"obu".to_vec(), synth_av1c())
            .no_profile_brand()
            .build()
            .expect("mux");
        let img = parse(&bytes).expect("parse");
        assert!(!img.compatible_brands.iter().any(|b| b == b"MA1A"));
        assert!(!img.compatible_brands.iter().any(|b| b == b"MA1B"));
        // The general brands stay (av1-avif §8.1 / §6.2).
        assert!(img.compatible_brands.iter().any(|b| b == b"mif1"));
        assert!(img.compatible_brands.iter().any(|b| b == b"miaf"));
        // Still a conformant mif1 file.
        assert!(crate::parser::audit_mif1(&bytes)
            .expect("audit")
            .is_compliant());
    }

    #[test]
    fn grid_muxer_profile_brand_controls() {
        let av1c = synth_av1c();
        let mk = |n: u8| GridTile {
            width: 16,
            height: 16,
            payload: vec![n; 5],
            av1c: av1c.clone(),
        };
        let adv = AvifGridMuxer::new(1, 2, 32, 16)
            .tile(mk(1))
            .tile(mk(2))
            .advanced_profile()
            .build()
            .expect("mux adv grid");
        let img = parse_header(&adv).expect("parse");
        assert!(img.compatible_brands.iter().any(|b| b == b"MA1A"));
        let bare = AvifGridMuxer::new(1, 2, 32, 16)
            .tile(mk(1))
            .tile(mk(2))
            .no_profile_brand()
            .build()
            .expect("mux bare grid");
        let img = parse_header(&bare).expect("parse");
        assert!(!img.compatible_brands.iter().any(|b| b == b"MA1A"));
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
        assert!(hdr.meta.location_by_id(1).is_some(), "grid iloc");
        let payload = hdr.item_data(1).expect("grid bytes");
        let g = crate::grid::ImageGrid::parse(&payload).expect("grid parse");
        assert_eq!((g.output_width, g.output_height), (70_000, 8));
        assert_eq!((g.rows, g.columns), (1, 2));
    }
}
