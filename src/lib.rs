//! AVIF (AV1 Image File Format) — the AV1 profile of HEIF / MIAF, with
//! the ISOBMFF / HEIF container served by [`oxideav_heif`] and AV1
//! pixel decode delegated to [`oxideav_av1`].
//!
//! # Layering
//!
//! * **Container (`oxideav-heif`)**: the box reader ([`box_parser`] is
//!   a thin surface over it), the `meta` item model — `hdlr`, `pitm`,
//!   `iinf` + `infe`, `iloc` (every construction method), `iref`,
//!   `iprp` / `ipco` / `ipma`, `idat`, `grpl` — item payload
//!   resolution, the typed `ispe` / `pixi` / `colr` / `pasp` / `clap` /
//!   `irot` / `imir` / `iscl` / `auxC` / `clli` / `amve` / `rloc` /
//!   `lsel` / `a1op` / `a1lx` / `rref` / `crtt` / `mdft` / `udes` /
//!   `altt` properties, the derived-image descriptors, pixel
//!   composition (grid / `iovl` / `iden`, alpha attachment, `clap` /
//!   `irot` / `imir`), the `moov` / `trak` / `stbl` sample tables and
//!   the still-image writer.
//! * **AVIF profile (this crate)**: [`parse`] / [`parse_header`] build
//!   this crate's [`Meta`] from the container's, keep `av1C` raw for
//!   the AV1 layer, read `mdcv` / `cclv` and the HEIF-extension
//!   properties (`aebr` … `cmin`, the slideshow transition effects,
//!   `prdi`, `sstr`, `txlo`, `elng`, `fnch`, `mskC`) from the raw
//!   property bytes, and enforce the AVIF brand rule; the av1-avif
//!   §2–§8 audits ([`audit_avif_profile_compliance`],
//!   [`audit_alpha_bit_depth`], [`audit_sequence_header_obu`],
//!   [`audit_tone_map`], [`audit_avis_sequence`] …), the `sato`
//!   descriptor / evaluator (§4.2.3) and `tmap` detection (§4.2.2),
//!   the AV1 decode of every coded item with `lsel` / `a1op` layer
//!   selection, the AVIF-side composition rules (av1-avif §4.1
//!   same-depth alpha, nearest-neighbour alpha resize, the `ispe` /
//!   grid trim with AV1's ceiling chroma extents, monochrome overlays
//!   staying monochrome), the `avis` sample handling, the writer's
//!   AVIF layout (brands, item order, property sets, `auxC` URNs) and
//!   the pixel encoder's tile election.
//!
//! # Status
//!
//! * [`AvifDecoder`]'s `receive_frame` composites the result:
//!   * Grid items (HEIF §6.6.2) — decode each tile via `dimg` iref
//!     and stitch into the declared output rectangle (see [`grid`]).
//!   * Alpha auxiliary — AV1-coded monochrome item referenced via
//!     `auxl` + `auxC` URN (see [`alpha`]). The av1-avif v1.2.0 §4.1
//!     `shall` "AV1 Alpha Image Item shall be encoded with the same
//!     bit depth as the associated master AV1 Image Item" is audited
//!     at the container layer via [`audit_alpha_bit_depth`] /
//!     [`AlphaBitDepthAudit`], surfaced through
//!     [`AvifInfo::alpha_bit_depth_compliance`].
//!   * `irot` / `imir` / `clap` post-transforms (see [`transform`]).
//! * AVIS image sequences — [`avis::parse_avis`] walks the first
//!   track through the container's sample-table parser and produces a
//!   flat frame-offset list with `(timescale, display_dims, samples,
//!   av1_codec_config, handler, sample_description_types)`.
//!   [`avis::sample_bytes`] resolves a sample's byte slice inside the
//!   source file. The registry-gated decoder
//!   ([`decoder::AvifDecoder::decode_avis_file`]) walks the table end
//!   to end, lifts the `AV1CodecConfigurationRecord` from `stsd` →
//!   `av01` → `av1C`, and fans every sample through a shared
//!   `oxideav_av1` registry decoder so inter-frame state is preserved.
//!   `Decoder::send_packet` dispatches to this path automatically
//!   when the file's brand classification flags it as a sequence. The
//!   av1-avif v1.2.0 §3 `shall`s on an AV1 Image Sequence track
//!   (handler `'pict'`, single `'av01'` sample description, identical
//!   Sequence Header OBUs across samples) are audited at the
//!   container layer via [`audit_avis_sequence`] /
//!   [`AvisSequenceCompliance`].
//! * `pixi` (HEIF §6.5.6) and `pasp` (HEIF §6.5.4 / ISO/IEC 14496-12
//!   §8.5.2.1.1) are surfaced through [`AvifInfo`] — see
//!   [`AvifInfo::num_channels`], [`AvifInfo::max_bit_depth`],
//!   [`AvifInfo::is_monochrome`] and [`AvifInfo::has_square_pixels`].
//!
//! # Encoder
//!
//! The **container muxer** is implemented in [`mux`]: [`AvifMuxer`] /
//! [`AvifGridMuxer`] / [`AvifOverlayMuxer`] / [`encode_still_av1`] emit
//! a conformant AVIF file around an already-coded AV1 Image Item Data
//! payload plus its `av1C` record — this crate decides the brands,
//! item order and property sets, [`oxideav_heif::HeifWriter`]
//! serialises the `meta` tree and `mdat` — with `ispe` / `pixi` /
//! `colr` / `pasp` / `clap` / `irot` / `imir` item properties, an
//! optional AV1-coded alpha auxiliary (`auxC` + `auxl`), and optional
//! `grid` tiling (`dimg`). The output round-trips through this crate's
//! own [`parse`] path pixel-consistently (the coded AV1 stream is copied
//! verbatim).
//!
//! Turning raw pixels *into* that AV1 bitstream is the job of the
//! [`still`] module ([`StillImage`] + [`encode_still`] /
//! [`encode_still_grid`]): it drives `oxideav_av1`'s conformance-grade
//! KEY-frame encoder across the full (bit depth × chroma format)
//! matrix — including RGB(A) via the H.273 identity matrix, alpha
//! auxiliaries, arbitrary extents (pad + `clap`), and grid tiling —
//! then wraps the coded payload through the muxer. The registry
//! [`Encoder`](encoder) surface ([`make_encoder`] / [`AvifEncoder`])
//! rides the same pipeline: every video frame sent through
//! `send_frame` yields one complete AVIF file packet. Callers holding
//! an already-coded payload keep muxing it directly with
//! [`AvifMuxer`].
//!
//! # Standalone vs registry-integrated
//!
//! The crate's default-on `registry` Cargo feature pulls in
//! `oxideav-core` + `oxideav-av1` and exposes the
//! `oxideav_core::Decoder` trait surface plus the [`register`] entry
//! point. Disable the feature (`default-features = false`) for an
//! `oxideav-core`-free build that still exposes:
//!
//! * The container parse ([`parser`], [`parse`], [`parse_header`],
//!   [`meta`]; [`box_parser`] over `oxideav_heif::boxes`).
//! * The AVIS sample-table walker ([`avis::parse_avis`]).
//! * The grid / overlay / alpha / transform composition entry points
//!   ([`grid`], [`overlay`], [`alpha`], [`transform`]) over the
//!   container's `compose` layer, operating on crate-local
//!   [`image::AvifFrame`] / [`image::AvifPixelFormat`].
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
pub mod derived;
pub mod error;
mod frame_bridge;
pub mod grid;
pub mod image;
pub mod inspect;
pub mod meta;
pub mod mux;
pub mod overlay;
pub mod parser;
pub mod region;
pub mod sample_group;
pub mod transform;

#[cfg(feature = "registry")]
mod av1_config;

#[cfg(feature = "registry")]
pub mod decoder;

#[cfg(feature = "registry")]
pub mod encoder;

#[cfg(feature = "registry")]
pub mod sequence;
#[cfg(feature = "registry")]
pub mod still;

pub use alpha::{composite_alpha, find_alpha_item_id, ALPHA_URN_PREFIX};
pub use avis::{
    audit_avis_profile_compliance, audit_avis_sequence, audit_edit_list, inspect_avis, parse_avis,
    parse_prft, parse_producer_reference_times, parse_ssix, parse_subsegment_indexes, sample_bytes,
    AvisInfo, AvisMeta, AvisProfileCompliance, AvisSequenceCompliance, EditListCompliance,
    EditListEntry, ProducerReferenceTime, Sample, SubsegmentIndex, SubsegmentRange, HANDLER_PICT,
    NTP_UNIX_EPOCH_OFFSET_SECONDS,
};
pub use cicp::{
    effective_cicp, is_matrix_reserved, is_primaries_reserved, is_transfer_reserved, matrix_name,
    primaries_name, transfer_name, CicpTriple,
};
pub use derived::{
    audit_alpha_bit_depth, audit_avif_profile_compliance, audit_iden_derivations, audit_pred_brand,
    audit_sequence_header_obu, audit_tone_map, build_derivation_graph, normalize_full_range_plane,
    output_dims_from_reconstructed, parse_grpl, reconstructed_dims, resolve_grids,
    resolve_iden_derivations, resolve_overlays, resolve_tone_maps, transform_chain,
    AlphaBitDepthAudit, AvifProfile, AvifProfileCompliance, BracketingKind, DerivationGraph,
    DerivationKind, DerivationNode, DimTransform, EntityGroup, GainMapChannel, GainMapMetadata,
    GainMapRational, GridResolution, GridTilePlacement, IdenCompliance, IdenResolution,
    ImageOverlay, Mif1Compliance, OverlayEntry, OverlayPlacement, OverlayResolution,
    PredBrandCompliance, SampleTransform, SequenceHeaderObuAudit, Token, ToneMapCompliance,
    ToneMapResolution, MAX_DERIVATION_DEPTH,
};
pub use error::{AvifError, Result};
pub use grid::{composite_grid, ImageGrid};
pub use image::{AvifFrame, AvifPixelFormat, AvifPlane};
pub use inspect::{
    coded_item_dependencies, derivation_graph, entity_groups, gain_map_metadata, inspect,
    is_font_item, item_payload_bytes, region_items, region_items_for, text_items, text_items_for,
    transforms_for, AvifInfo, CodedItemDependencies, ResolvedRegionItem, ResolvedTextItem,
};
pub use meta::{
    A1lx, A1op, Aebr, Afbr, Altt, AuxC, AuxKind, Cclv, Clap, Clli, Cmex, Cmin, Colr, Crtt, Dobr,
    Elng, Fade, Fnch, Fobr, Imir, IrefEntry, Irot, Iscl, Ispe, ItemInfo, ItemLocation, Lsel, MaskC,
    Mdcv, Mdft, Meta, Pano, PanoGrid, Pasp, Pixi, Prdi, Property, Rloc, Rref, Splt, Ssld, Sstr,
    Stpe, Subs, SubsEntry, Tols, Txlo, Udes, Wbbr, Wipe, Zoom, AUX_URN_ALPHA_HEVC,
    AUX_URN_ALPHA_MPEG, AUX_URN_DEPTH_HEVC, AUX_URN_DEPTH_MPEG, AUX_URN_HDR_GAINMAP,
    ITEM_TYPE_EXIF, ITEM_TYPE_IDEN, ITEM_TYPE_IOVL, ITEM_TYPE_MIME, ITEM_TYPE_SATO, ITEM_TYPE_TMAP,
    ITEM_TYPE_URI,
};
pub use mux::{
    encode_still_av1, AvifGridMuxer, AvifMuxer, AvifOverlayMuxer, GridTile, IdentityDerivation,
    OverlayLayer,
};
pub use overlay::{composite_overlay, OverlayInput, MAX_OVERLAY_CANVAS_PIXELS};
pub use parser::{
    audit_mif1, classify_brands, item_bytes, item_bytes_owned, item_bytes_owned_full,
    item_bytes_owned_with_idat, item_bytes_with_idat, parse, parse_header, AvifHeader, AvifImage,
    BrandClass, BRAND_AVIF, BRAND_AVIO, BRAND_AVIS, BRAND_MA1A, BRAND_MA1B, BRAND_MIAF, BRAND_MIF1,
    BRAND_MSF1, ITEM_TYPE_AV01, ITEM_TYPE_GRID,
};
pub use region::{
    resolve_derived_region_items, DerivedRegionItem, RegionGeometry, RegionItem, ITEM_TYPE_MSKI,
    ITEM_TYPE_RGAN, REF_TYPE_DRGN,
};
pub use sample_group::{
    parse_csgp, parse_sample_group_descriptions, parse_sample_to_groups, parse_sbgp, parse_sgpd,
    BracketingEntry, SampleGroupDescription, SampleToGroup, SampleToGroupKind, SampleToGroupRun,
    VisualEquivalenceEntry,
};
pub use transform::{apply_clap, apply_imir, apply_irot, crop_top_left};

#[cfg(feature = "registry")]
pub use decoder::{make_decoder, AvifDecoder, MAX_GRID_CANVAS_PIXELS, MAX_ITEM_DECODES};

#[cfg(feature = "registry")]
pub use encoder::{make_encoder, AvifEncoder};

#[cfg(feature = "registry")]
pub use sequence::{encode_sequence, SequenceEncodeOptions};

#[cfg(feature = "registry")]
pub use still::{
    elect_grid_tiling, encode_still, encode_still_auto, encode_still_grid, encode_still_layered,
    encode_still_overlay, OverlayCanvas, OverlayLayerImage, StillChroma, StillEncodeOptions,
    StillImage, StillProperties, GRID_MIN_TILE_DIM, STILL_MAX_CODED_DIM,
};

#[cfg(feature = "registry")]
#[doc(hidden)]
pub use registry_glue::__oxideav_entry;

/// Public codec id string. Matches the aggregator-crate Cargo feature `avif`.
pub const CODEC_ID_STR: &str = "avif";

#[cfg(feature = "registry")]
mod registry_glue {
    //! Codec registry + AVIF encoder factory. Gated behind `registry`
    //! because the entire framework integration depends on
    //! `oxideav_core` + `oxideav_av1`.

    use oxideav_core::{
        CodecCapabilities, CodecId, CodecInfo, CodecRegistry, ContainerRegistry, Error,
        RuntimeContext,
    };

    use crate::decoder::make_decoder;
    use crate::encoder::make_encoder;
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
    /// on the resulting frames. The encoder factory ([`make_encoder`])
    /// yields the pixel-in AVIF encoder (one complete AVIF file per
    /// frame; see [`crate::encoder`] for the format/option surface).
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

    /// Convenience: register AVIF **and** the underlying AV1 codec in
    /// one call — the historical contract of this entry point,
    /// restored now that `oxideav-av1`'s clean-room rebuild ships its
    /// own registry surface again. Callers that only want the AVIF
    /// factories keep using [`register_codecs`].
    pub fn register_with_av1(reg: &mut CodecRegistry) {
        register_codecs(reg);
        oxideav_av1::registry::register_codecs(reg);
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
pub use registry_glue::{register, register_codecs, register_containers, register_with_av1};

#[cfg(all(test, feature = "registry"))]
mod tests {
    use super::*;
    use oxideav_core::{
        CodecId, CodecParameters, CodecRegistry, ContainerRegistry, Error, Frame, RuntimeContext,
    };

    #[test]
    fn register_installs_factories() {
        let mut reg = CodecRegistry::new();
        register_codecs(&mut reg);
        let id = CodecId::new(CODEC_ID_STR);
        // The encoder factory yields the real pixel-in AVIF encoder:
        // a registry-resolved encode of an 8x8 gray frame produces a
        // parseable AVIF file.
        let mut params = CodecParameters::video(id.clone());
        params.width = Some(8);
        params.height = Some(8);
        params.pixel_format = Some(oxideav_core::PixelFormat::Gray8);
        let mut enc = reg.first_encoder(&params).expect("encoder factory");
        let frame = Frame::Video(oxideav_core::frame::VideoFrame {
            pts: Some(0),
            planes: vec![oxideav_core::frame::VideoPlane {
                stride: 8,
                data: (0..64u8).collect(),
            }],
        });
        enc.send_frame(&frame).expect("send_frame");
        let pkt = enc.receive_packet().expect("receive_packet");
        crate::parser::parse(&pkt.data).expect("emitted packet is a parseable AVIF file");
        // A frame without the required parameters is refused with a
        // precise error, not a panic.
        let bare = reg
            .first_encoder(&CodecParameters::video(id))
            .expect("encoder factory");
        let mut bare = bare;
        let empty = Frame::Video(oxideav_core::frame::VideoFrame {
            pts: Some(0),
            planes: vec![],
        });
        match bare.send_frame(&empty) {
            Err(Error::InvalidData(msg)) => assert!(msg.contains("width"), "{msg}"),
            other => panic!("encoder send_frame: expected InvalidData, got {other:?}"),
        }
        // Decoder factory succeeds; `send_packet` exercises the HEIF
        // parse + AV1 decode pipeline.
        let params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
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
