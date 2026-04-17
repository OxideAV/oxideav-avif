//! AVIF `Decoder` implementation. Parses the HEIF container, extracts
//! the primary item's AV1 OBU stream + `av1C` config, feeds everything
//! to [`oxideav_av1::Av1Decoder`] and surfaces its `Unsupported` error
//! unchanged. Once the AV1 crate grows pixel decode, this file should
//! not need changes — it already forwards the `receive_frame` call.

use oxideav_codec::Decoder;
use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};

use oxideav_av1::{Av1CodecConfig, Av1Decoder};

use crate::meta::{Ispe, Pasp, Pixi};
use crate::parser::{parse, AvifImage};

/// High-level view of an AVIF file after the HEIF pass — useful for
/// callers that want to inspect dimensions + colour info without
/// constructing a full `Decoder`.
#[derive(Clone, Debug)]
pub struct AvifInfo {
    pub width: u32,
    pub height: u32,
    pub bits_per_channel: Vec<u8>,
    pub pasp: Option<Pasp>,
    pub av1c: Vec<u8>,
    pub obu_bytes: Vec<u8>,
}

pub fn inspect(file: &[u8]) -> Result<AvifInfo> {
    let img = parse(file)?;
    build_info(&img)
}

fn build_info(img: &AvifImage<'_>) -> Result<AvifInfo> {
    let av1c = img
        .av1c
        .clone()
        .ok_or_else(|| Error::invalid("avif: primary item missing av1C property"))?;
    let Ispe { width, height } = img
        .ispe
        .ok_or_else(|| Error::invalid("avif: primary item missing ispe property"))?;
    let bits_per_channel = img
        .pixi
        .as_ref()
        .map(|Pixi { bits_per_channel }| bits_per_channel.clone())
        .unwrap_or_default();
    Ok(AvifInfo {
        width,
        height,
        bits_per_channel,
        pasp: img.pasp,
        av1c,
        obu_bytes: img.primary_item_data.to_vec(),
    })
}

/// `Decoder` trait impl registered under codec id `avif`.
pub struct AvifDecoder {
    codec_id: CodecId,
    inner: Option<Av1Decoder>,
    /// AV1CodecConfigurationRecord captured from `av1C` — also used as
    /// `CodecParameters::extradata` for the inner decoder so its
    /// sequence-header bootstrap matches the container metadata.
    av1c: Option<Vec<u8>>,
    info: Option<AvifInfo>,
}

impl AvifDecoder {
    pub fn new(codec_id: CodecId) -> Self {
        Self {
            codec_id,
            inner: None,
            av1c: None,
            info: None,
        }
    }

    /// Parse an AVIF file and prime the inner AV1 decoder with its
    /// configuration record. `receive_frame()` will then error with
    /// the AV1 decoder's `Unsupported` until AV1 pixel decode lands.
    pub fn decode_file(&mut self, file: &[u8]) -> Result<AvifInfo> {
        let img = parse(file)?;
        let info = build_info(&img)?;

        // Build CodecParameters for the AV1 decoder. The av1C record
        // lands verbatim in extradata — oxideav-av1's
        // `Av1Decoder::new` consumes it and parses the embedded
        // sequence header so the first frame_header OBU parses cleanly.
        let mut params = CodecParameters::video(CodecId::new("av1"));
        params.width = Some(info.width);
        params.height = Some(info.height);
        params.extradata = info.av1c.clone();
        let mut av1 = Av1Decoder::new(params);

        // Eagerly validate av1C — mirrors what the AV1 decoder does on
        // construction but gives us a crisp error before we start
        // feeding packets.
        let _cfg = Av1CodecConfig::parse(&info.av1c)?;

        // Send the primary item's OBU stream as a single packet. The
        // AV1 decoder walks OBUs, parses the frame_header through
        // tile_info, records tile payload boundaries, and then
        // surfaces `Unsupported` on reaching the tile body — that's
        // the expected stopping point for a parse-only AV1 build.
        let pkt = Packet::new(0, TimeBase::new(1, 90_000), info.obu_bytes.clone());
        match av1.send_packet(&pkt) {
            Ok(()) => {}
            Err(Error::Unsupported(_)) => {
                // Expected for the current AV1 crate state — headers
                // parsed, pixel decode unavailable. Keep the decoder so
                // `receive_frame()` can re-surface the same error.
            }
            Err(other) => return Err(other),
        }

        self.inner = Some(av1);
        self.av1c = Some(info.av1c.clone());
        self.info = Some(info.clone());
        Ok(info)
    }

    pub fn info(&self) -> Option<&AvifInfo> {
        self.info.as_ref()
    }
}

impl Decoder for AvifDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        // Every AVIF packet is a complete file. Feed it to the HEIF
        // parser and stash the inner AV1 decoder.
        self.decode_file(&packet.data).map(|_| ())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        // We reached AV1 land but stopped at the tile decode gate. The
        // AV1 decoder's own `receive_frame()` already returns its
        // canonical unsupported message; preserve that chain so the
        // error points at the real blocker.
        match self.inner.as_mut() {
            Some(av1) => match av1.receive_frame() {
                Err(Error::Unsupported(s)) => Err(Error::Unsupported(format!(
                    "avif pixel decode blocked by av1 decoder limitations: {s}"
                ))),
                other => other,
            },
            None => Err(Error::unsupported(
                "avif pixel decode blocked by av1 decoder limitations",
            )),
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.inner = None;
        self.av1c = None;
        self.info = None;
        Ok(())
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(AvifDecoder::new(params.codec_id.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/monochrome.avif");

    #[test]
    fn decoder_surfaces_av1_unsupported() {
        let mut d = AvifDecoder::new(CodecId::new(crate::CODEC_ID_STR));
        let pkt = Packet::new(0, TimeBase::new(1, 1), FIXTURE.to_vec());
        d.send_packet(&pkt).expect("send_packet");
        let info = d.info().cloned().expect("info");
        assert!(info.width > 0 && info.height > 0);
        assert!(!info.av1c.is_empty());
        assert!(!info.obu_bytes.is_empty());

        match d.receive_frame() {
            Err(Error::Unsupported(s)) => {
                assert!(
                    s.contains("avif pixel decode blocked by av1 decoder limitations"),
                    "got: {s}"
                );
            }
            Err(other) => panic!("expected Unsupported, got {other:?}"),
            Ok(_) => panic!("expected Unsupported, got a frame"),
        }
    }

    #[test]
    fn inspect_extracts_primary_item() {
        let info = inspect(FIXTURE).expect("inspect");
        assert!(info.width > 0 && info.height > 0);
        // av1C always starts with the marker/version byte 0x81.
        assert_eq!(info.av1c[0], 0x81);
    }
}
