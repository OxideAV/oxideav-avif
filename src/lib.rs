//! AVIF (AV1 Image File Format) — pure-Rust container parser with
//! AV1 hand-off to [`oxideav_av1`].
//!
//! # Status
//!
//! * HEIF / ISOBMFF box walker: `ftyp`, `meta`, `hdlr`, `pitm`, `iinf`
//!   (v0/v1) + `infe` (v2/v3), `iloc` (v0/v1/v2), `iprp` / `ipco` /
//!   `ipma` (v0/v1, small + large indices), plus item properties
//!   `av1C`, `ispe`, `colr` (nclx + ICC), `pixi`, `pasp`.
//! * Primary item resolution via `pitm`, file-offset extent reads via
//!   `iloc`, brand check accepting `avif` / `avis` / `mif1` / `msf1` /
//!   `miaf`.
//! * Primary item's AV1 OBU bitstream is handed to
//!   [`oxideav_av1::Av1Decoder`] with `av1C` plumbed through as
//!   `CodecParameters::extradata`. The inner decoder parses sequence
//!   header + frame header through `tile_info()`, captures per-tile
//!   byte ranges, and then stops at the tile body with
//!   `Error::Unsupported` — expected, since AV1 pixel reconstruction
//!   is still out of scope in that crate.
//!
//! [`AvifDecoder::receive_frame`] therefore returns
//! `Error::Unsupported("avif pixel decode blocked by av1 decoder
//! limitations: <av1 specific message>")` until the AV1 decoder gains
//! partition / transform / prediction / loop-filter paths. Until then,
//! [`AvifDecoder::info`] and [`inspect`] give callers dimensions, bit
//! depth, colour info, and the extracted OBU bytes.
//!
//! # Encoder
//!
//! Not implemented — [`make_encoder`] returns `Error::Unsupported`.
//! Writing an AVIF encoder requires an AV1 encoder, which oxideav
//! does not have.
//!
//! [`inspect`]: decoder::inspect

pub mod box_parser;
pub mod decoder;
pub mod meta;
pub mod parser;

pub use decoder::{inspect, make_decoder, AvifDecoder, AvifInfo};
pub use meta::{Colr, Ispe, ItemInfo, ItemLocation, Meta, Pasp, Pixi, Property};
pub use parser::{parse, AvifImage};

use oxideav_codec::{CodecInfo, CodecRegistry, Encoder};
use oxideav_core::{CodecCapabilities, CodecId, CodecParameters, Error, Result};

/// Public codec id string. Matches the aggregator-crate Cargo feature `avif`.
pub const CODEC_ID_STR: &str = "avif";

/// Register the AVIF decoder + encoder factories with a registry. The
/// decoder is declared `avif_heif_av1_parse` — we parse the HEIF
/// container end to end and hand the AV1 bitstream to oxideav-av1,
/// which today stops before pixel reconstruction.
pub fn register(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("avif_heif_av1_parse")
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
        // parse and stops at the AV1 tile decode gate.
        let _ = reg.make_decoder(&params).expect("decoder factory");
    }
}
