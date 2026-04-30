//! AVIF (AV1 Image File Format) — pure-Rust container parser with
//! AV1 pixel decode delegated to [`oxideav_av1`].
//!
//! # Status
//!
//! * HEIF / ISOBMFF box walker: `ftyp`, `meta`, `hdlr`, `pitm`, `iinf`
//!   (v0/v1) + `infe` (v2/v3), `iloc` (v0/v1/v2), `iref`, `iprp` /
//!   `ipco` / `ipma` (v0/v1, small + large indices), plus item
//!   properties `av1C`, `ispe`, `colr` (nclx + ICC), `pixi`, `pasp`,
//!   `irot`, `imir`, `clap`, `auxC`.
//! * Primary item resolution via `pitm`, file-offset extent reads via
//!   `iloc`, brand check accepting `avif` / `avis` / `mif1` / `msf1` /
//!   `miaf`.
//! * Primary item's AV1 OBU bitstream is handed to
//!   [`oxideav_av1::Av1Decoder`], which now returns real frames for
//!   every intra still plus single-reference inter clips.
//!   [`AvifDecoder::receive_frame`] composites the result:
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
//! Not implemented — [`make_encoder`] returns `Error::Unsupported`.
//! Writing an AVIF encoder requires an AV1 encoder, which oxideav does
//! not have.

pub mod alpha;
pub mod avis;
pub mod box_parser;
pub mod cicp;
pub mod decoder;
pub mod grid;
pub mod meta;
pub mod parser;
pub mod transform;

pub use alpha::{composite_alpha, find_alpha_item_id, ALPHA_URN_PREFIX};
pub use avis::{parse_avis, sample_bytes, sample_table, AvisMeta, Sample};
pub use cicp::{
    effective_cicp, is_matrix_reserved, is_primaries_reserved, is_transfer_reserved, matrix_name,
    primaries_name, transfer_name, CicpTriple,
};
pub use decoder::{inspect, make_decoder, AvifDecoder, AvifInfo};
pub use grid::{composite_grid, ImageGrid};
pub use meta::{
    AuxC, Clap, Colr, Imir, IrefEntry, Irot, Ispe, ItemInfo, ItemLocation, Meta, Pasp, Pixi,
    Property,
};
pub use parser::{
    classify_brands, parse, parse_header, AvifHeader, AvifImage, BrandClass, BRAND_AVIF,
    BRAND_AVIO, BRAND_AVIS, BRAND_MA1A, BRAND_MA1B, BRAND_MIAF, BRAND_MIF1, BRAND_MSF1,
};
pub use transform::{apply_clap, apply_imir, apply_irot, crop_top_left};

use oxideav_core::{CodecCapabilities, CodecId, CodecParameters, Error, Result};
use oxideav_core::{CodecInfo, CodecRegistry, Encoder};

/// Public codec id string. Matches the aggregator-crate Cargo feature `avif`.
pub const CODEC_ID_STR: &str = "avif";

/// Register the AVIF decoder + encoder factories with a registry. The
/// decoder is declared `avif_heif_av1_decode` — we parse the HEIF
/// container end to end, hand the AV1 bitstream to oxideav-av1, and
/// composite grid / alpha / transform properties on the resulting
/// frames.
pub fn register(reg: &mut CodecRegistry) {
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

/// AVIF encoder factory — always errors. Writing AVIF requires an AV1
/// encoder, which oxideav does not currently ship.
pub fn make_encoder(_params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    Err(Error::unsupported(
        "avif: encoder not implemented — requires an AV1 encoder (not available in oxideav)",
    ))
}

/// Convenience: register AVIF + its underlying AV1 decoder in one
/// call. Useful when the registry is being built from scratch and the
/// caller only wants AVIF — they don't have to remember that AVIF
/// delegates to the AV1 codec.
pub fn register_with_av1(reg: &mut CodecRegistry) {
    register(reg);
    oxideav_av1::register(reg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_installs_factories() {
        let mut reg = CodecRegistry::new();
        register(&mut reg);
        let id = CodecId::new(CODEC_ID_STR);
        let params = CodecParameters::video(id);
        // Encoder stays Unsupported.
        match reg.make_encoder(&params) {
            Err(Error::Unsupported(_)) => {}
            Err(e) => panic!("encoder factory: expected Unsupported, got {e:?}"),
            Ok(_) => panic!("encoder factory: expected Unsupported, got live encoder"),
        }
        // Decoder factory succeeds; `send_packet` exercises the HEIF
        // parse + AV1 decode pipeline.
        let _ = reg.make_decoder(&params).expect("decoder factory");
    }
}
