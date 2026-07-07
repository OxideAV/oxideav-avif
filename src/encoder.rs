//! AVIF `Encoder` trait wiring — registry-gated.
//!
//! AVIF is a container that carries an AV1-coded image. The
//! container-muxing half of "encode an AVIF" is fully implemented in
//! [`crate::mux`] and works on a black-box AV1 payload (an already-coded
//! AV1 Image Item Data plus its `av1C` record). The other half — turning
//! decoded pixels into that AV1 bitstream — needs an **AV1 encoder**,
//! which oxideav does not yet ship (the `oxideav-av1` crate is mid
//! clean-room rebuild and exposes no encode path).
//!
//! This module bridges the gap the trait way: [`make_encoder`] returns a
//! live [`AvifEncoder`] so the codec registry exposes a real
//! `Box<dyn Encoder>` (rather than failing at factory-construction time).
//! Its [`Encoder::send_frame`] surfaces a precise `Unsupported` error
//! naming the missing AV1-encode dependency — the honest capability
//! boundary. Callers that already hold a coded AV1 payload should mux it
//! directly through [`crate::AvifMuxer`] / [`crate::encode_still_av1`],
//! which need no AV1 encoder at all.

use oxideav_core::{CodecId, CodecParameters, Encoder, Error, Frame, Packet, Result};

/// Message used by [`AvifEncoder::send_frame`] to report the missing
/// AV1-encode dependency. Shared so tests can assert on it verbatim.
pub(crate) const NO_AV1_ENCODER_MSG: &str =
    "avif: pixel-to-AV1 encoding requires an AV1 encoder (not yet available in oxideav-av1); \
     mux an already-coded AV1 payload via oxideav_avif::AvifMuxer instead";

/// Frame-to-AVIF encoder.
///
/// The container muxing is available today via [`crate::mux`]; this trait
/// object exists so the registry surface is complete. Because there is no
/// AV1 pixel encoder yet, [`Encoder::send_frame`] returns `Unsupported`.
pub struct AvifEncoder {
    params: CodecParameters,
}

impl AvifEncoder {
    /// Build an encoder announcing `codec_id` in its output parameters.
    pub fn new(codec_id: CodecId) -> Self {
        Self {
            params: CodecParameters::video(codec_id),
        }
    }

    /// Build an encoder from an explicit parameter set.
    pub fn with_params(params: CodecParameters) -> Self {
        Self { params }
    }
}

impl Encoder for AvifEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.params
    }

    fn send_frame(&mut self, _frame: &Frame) -> Result<()> {
        Err(Error::unsupported(NO_AV1_ENCODER_MSG))
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        Err(Error::unsupported(NO_AV1_ENCODER_MSG))
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Direct factory endpoint (matches the crate's dual-API convention):
/// build a boxed AVIF [`Encoder`] from a parameter set.
pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(AvifEncoder::with_params(params.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_reports_missing_av1_encoder_on_send_frame() {
        let params = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
        let mut enc = make_encoder(&params).expect("make encoder");
        assert_eq!(enc.codec_id().as_str(), crate::CODEC_ID_STR);
        // Any frame is refused with the precise cross-crate message.
        let frame = Frame::Video(oxideav_core::frame::VideoFrame {
            pts: Some(0),
            planes: vec![],
        });
        match enc.send_frame(&frame) {
            Err(Error::Unsupported(msg)) => {
                assert!(msg.contains("AV1 encoder"), "message: {msg}");
                assert!(
                    msg.contains("AvifMuxer"),
                    "message points at the muxer: {msg}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // flush is a clean no-op.
        enc.flush().expect("flush ok");
    }
}
