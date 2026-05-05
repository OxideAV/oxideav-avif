//! AVIF (AV1 Image File Format) — pure-Rust container parser with
//! AV1 pixel decode delegated to [`oxideav_av1`].
//!
//! # Status
//!
//! * HEIF / ISOBMFF box walker: `ftyp`, `meta`, `hdlr`, `pitm`, `iinf`
//!   (v0/v1) + `infe` (v2/v3), `iloc` (v0/v1/v2), `iref`, `iprp` /
//!   `ipco` / `ipma` (v0/v1, small + large indices), plus item
//!   properties `av1C`, `ispe`, `colr` (nclx + ICC), `pixi`, `pasp`,
//!   `irot`, `imir`, `clap`, `auxC`, `mdcv`, `clli`, `cclv`.
//! * Primary item resolution via `pitm`, file-offset extent reads via
//!   `iloc`, brand check accepting `avif` / `avis` / `mif1` / `msf1` /
//!   `miaf`.
//! * Primary item's AV1 OBU bitstream is handed to
//!   [`oxideav_av1::Av1Decoder`] (when the default-on `registry` feature
//!   is enabled), which now returns real frames for every intra still
//!   plus single-reference inter clips. [`AvifDecoder::receive_frame`]
//!   composites the result:
//!   * Grid items (HEIF §6.6.2) — decode each tile via `dimg` iref
//!     and paste into the declared output rectangle (see [`grid`]).
//!   * Alpha auxiliary — AV1-coded monochrome item referenced via
//!     `auxl` + `auxC` URN (see [`alpha`]).
//!   * `irot` / `imir` / `clap` post-transforms (see [`transform`]).
//! * AVIS image sequences — sample table walk via [`avis::parse_avis`]
//!   produces a flat frame-offset list with `(timescale, display_dims,
//!   samples)`. [`avis::sample_bytes`] resolves a sample's byte slice
//!   inside the source file; pair with [`oxideav_av1`] to decode frames
//!   sequentially.
//! * `pixi` (HEIF §6.5.6) and `pasp` (HEIF §6.5.4 / ISO/IEC 14496-12
//!   §8.5.2.1.1) are surfaced through [`AvifInfo`] — see
//!   [`AvifInfo::num_channels`], [`AvifInfo::max_bit_depth`],
//!   [`AvifInfo::is_monochrome`] and [`AvifInfo::has_square_pixels`].
//!
//! # Encoder
//!
//! Not implemented — [`make_encoder`] (registry-only) returns
//! `Error::Unsupported`. Writing an AVIF encoder requires an AV1
//! encoder, which oxideav does not have.
//!
//! # Standalone vs registry-integrated
//!
//! The crate's default-on `registry` Cargo feature pulls in
//! `oxideav-core` + `oxideav-av1` and exposes the
//! `oxideav_core::Decoder` trait surface plus the [`register`] entry
//! point. Disable the feature (`default-features = false`) for an
//! `oxideav-core`-free build that still exposes:
//!
//! * The HEIF box walker + meta parser ([`box_parser`], [`meta`],
//!   [`parser`], [`parse`], [`parse_header`]).
//! * The AVIS sample-table walker ([`avis::parse_avis`]).
//! * The grid descriptor + composition layer ([`grid`]) operating on
//!   crate-local [`image::AvifFrame`] / [`image::AvifPixelFormat`].
//! * The alpha + transform composition helpers ([`alpha`],
//!   [`transform`]) on the same crate-local image types.
//! * Container-side inspection: [`inspect::inspect`],
//!   [`inspect::AvifInfo`], [`inspect::transforms_for`].
//! * The CICP signalling helpers ([`cicp`]).
//!
//! Standalone callers that want pixel decode must pair this surface
//! with their own AV1 decoder — the in-tree one ([`oxideav_av1`]) is
//! pulled in only when `registry` is on.

pub mod alpha;
pub mod avis;
pub mod box_parser;
pub mod cicp;
pub mod error;
pub mod grid;
pub mod image;
pub mod inspect;
pub mod meta;
pub mod parser;
pub mod transform;

#[cfg(feature = "registry")]
pub mod decoder;

pub use alpha::{composite_alpha, find_alpha_item_id, ALPHA_URN_PREFIX};
pub use avis::{parse_avis, sample_bytes, sample_table, AvisMeta, Sample};
pub use cicp::{
    effective_cicp, is_matrix_reserved, is_primaries_reserved, is_transfer_reserved, matrix_name,
    primaries_name, transfer_name, CicpTriple,
};
pub use error::{AvifError, Result};
pub use grid::{composite_grid, ImageGrid};
pub use image::{AvifFrame, AvifPixelFormat, AvifPlane};
pub use inspect::{inspect, transforms_for, AvifInfo};
pub use meta::{
    AuxC, Cclv, Clap, Clli, Colr, Imir, IrefEntry, Irot, Ispe, ItemInfo, ItemLocation, Mdcv, Meta,
    Pasp, Pixi, Property,
};
pub use parser::{
    classify_brands, item_bytes_owned, parse, parse_header, AvifHeader, AvifImage, BrandClass,
    BRAND_AVIF, BRAND_AVIO, BRAND_AVIS, BRAND_MA1A, BRAND_MA1B, BRAND_MIAF, BRAND_MIF1, BRAND_MSF1,
};
pub use transform::{apply_clap, apply_imir, apply_irot, crop_top_left};

#[cfg(feature = "registry")]
pub use decoder::{make_decoder, AvifDecoder};

/// Public codec id string. Matches the aggregator-crate Cargo feature `avif`.
pub const CODEC_ID_STR: &str = "avif";

#[cfg(feature = "registry")]
mod registry_glue {
    //! Codec registry + AVIF encoder factory. Gated behind `registry`
    //! because the entire framework integration depends on
    //! `oxideav_core` + `oxideav_av1`.

    use oxideav_core::{
        CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, ContainerRegistry,
        Encoder, Error, Result, RuntimeContext,
    };

    use crate::decoder::make_decoder;
    use crate::error::AvifError;
    use crate::CODEC_ID_STR;

    /// Bridge crate-local errors into the framework error type. Mirrors
    /// the conversion the decoder performs internally so external
    /// callers using the standalone container API get the same mapping
    /// when they wrap up their own framework integration.
    impl From<AvifError> for Error {
        fn from(e: AvifError) -> Self {
            match e {
                AvifError::InvalidData(s) => Error::InvalidData(s),
                AvifError::Unsupported(s) => Error::Unsupported(s),
            }
        }
    }

    /// Register the AVIF decoder + encoder factories with a registry.
    /// The decoder is declared `avif_heif_av1_decode` — we parse the
    /// HEIF container end to end, hand the AV1 bitstream to
    /// oxideav-av1, and composite grid / alpha / transform properties
    /// on the resulting frames.
    pub fn register_codecs(reg: &mut CodecRegistry) {
        let caps = CodecCapabilities::video("avif_heif_av1_decode")
            .with_lossy(true)
            .with_intra_only(true);
        reg.register(
            CodecInfo::new(CodecId::new(CODEC_ID_STR))
                .capabilities(caps)
                .decoder(make_decoder)
                .encoder(make_encoder),
        );
    }

    /// Unified registration entry point: install both the AVIF codec
    /// factories and the `.avif` / `.avifs` extension hints into a
    /// [`RuntimeContext`].
    ///
    /// Note this does **not** also register the underlying AV1 codec —
    /// callers that want both in one call should use
    /// [`register_with_av1`] (which itself only touches the codec
    /// sub-registry, since neither AVIF nor AV1 have container hooks
    /// beyond `.avif*`).
    ///
    /// This is the preferred entry point for new code — it matches the
    /// convention every sibling crate now follows. Direct callers that
    /// only need one of the two sub-registries can keep using
    /// [`register_codecs`] / [`register_containers`].
    pub fn register(ctx: &mut RuntimeContext) {
        register_codecs(&mut ctx.codecs);
        register_containers(&mut ctx.containers);
    }

    oxideav_core::register!("avif", register);

    /// AVIF encoder factory — always errors. Writing AVIF requires an
    /// AV1 encoder, which oxideav does not currently ship.
    pub fn make_encoder(_params: &CodecParameters) -> Result<Box<dyn Encoder>> {
        Err(Error::unsupported(
            "avif: encoder not implemented — requires an AV1 encoder (not available in oxideav)",
        ))
    }

    /// Convenience: register AVIF + its underlying AV1 decoder in one
    /// call. Useful when the registry is being built from scratch and
    /// the caller only wants AVIF — they don't have to remember that
    /// AVIF delegates to the AV1 codec.
    pub fn register_with_av1(reg: &mut CodecRegistry) {
        register_codecs(reg);
        oxideav_av1::register_codecs(reg);
    }

    /// Register the `.avif` / `.avifs` extensions against the codec id
    /// `"avif"` so consumers (cli-convert, pipeline output probing) can
    /// resolve a `.avif` output path through the central
    /// [`ContainerRegistry`] without a hard-coded extension list.
    pub fn register_containers(reg: &mut ContainerRegistry) {
        reg.register_extension("avif", CODEC_ID_STR);
        reg.register_extension("avifs", CODEC_ID_STR);
    }
}

#[cfg(feature = "registry")]
pub use registry_glue::{
    make_encoder, register, register_codecs, register_containers, register_with_av1,
};

#[cfg(all(test, feature = "registry"))]
mod tests {
    use super::*;
    use oxideav_core::{
        CodecId, CodecParameters, CodecRegistry, ContainerRegistry, Error, RuntimeContext,
    };

    #[test]
    fn register_installs_factories() {
        let mut reg = CodecRegistry::new();
        register_codecs(&mut reg);
        let id = CodecId::new(CODEC_ID_STR);
        let params = CodecParameters::video(id);
        // Encoder stays Unsupported.
        match reg.first_encoder(&params) {
            Err(Error::Unsupported(_)) => {}
            Err(e) => panic!("encoder factory: expected Unsupported, got {e:?}"),
            Ok(_) => panic!("encoder factory: expected Unsupported, got live encoder"),
        }
        // Decoder factory succeeds; `send_packet` exercises the HEIF
        // parse + AV1 decode pipeline.
        let _ = reg.first_decoder(&params).expect("decoder factory");
    }

    #[test]
    fn avif_extension_resolves_to_avif_container() {
        let mut reg = ContainerRegistry::new();
        register_containers(&mut reg);
        assert_eq!(reg.container_for_extension("avif"), Some(CODEC_ID_STR));
        assert_eq!(reg.container_for_extension("AVIF"), Some(CODEC_ID_STR));
        assert_eq!(reg.container_for_extension("avifs"), Some(CODEC_ID_STR));
    }

    #[test]
    fn register_via_runtime_context_installs_codec_factory() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new(CODEC_ID_STR);
        let params = CodecParameters::video(id);
        let _dec = ctx
            .codecs
            .first_decoder(&params)
            .expect("avif decoder factory");
        // The unified entry point also wires the .avif / .avifs
        // extension hints through the same call.
        assert_eq!(
            ctx.containers.container_for_extension("avif"),
            Some(CODEC_ID_STR)
        );
        assert_eq!(
            ctx.containers.container_for_extension("avifs"),
            Some(CODEC_ID_STR)
        );
    }
}
