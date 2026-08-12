//! AVIF `Encoder` trait wiring — registry-gated.
//!
//! Each frame sent through [`Encoder::send_frame`] encodes into one
//! **complete AVIF file** (AVIF is a still-image container: the
//! packet boundary is the file boundary), produced by the
//! [`crate::still`] pipeline — AV1 KEY-frame encode via `oxideav_av1`
//! plus this crate's container muxer. [`Encoder::receive_packet`]
//! hands the file bytes back as a data packet carrying the frame's
//! `pts`.
//!
//! # Input mapping
//!
//! [`CodecParameters`] must declare `width`, `height` and
//! `pixel_format`. Supported formats:
//!
//! * Planar YUV, 8-bit: `Yuv420P` / `Yuv422P` / `Yuv444P` / `Gray8`,
//!   plus the full-range `YuvJ420P` / `YuvJ422P` / `YuvJ444P`
//!   variants (same plane layout; a full-range `colr` `nclx` is
//!   attached and the coded stream signals §5.5.2 `color_range = 1`).
//! * Planar YUV, high bit depth: `Yuv420P10Le` / `Yuv422P10Le` /
//!   `Yuv444P10Le` / `Yuv420P12Le` / `Yuv422P12Le` / `Yuv444P12Le` /
//!   `Gray10Le` / `Gray12Le` (little-endian 2-byte samples).
//! * Packed RGB(A): `Rgb24` / `Rgba` / `Bgr24` / `Bgra` — coded via
//!   the H.273 identity matrix in 4:4:4 (lossless-capable; see
//!   [`crate::still::StillImage::rgb8`]); the alpha channel becomes
//!   an alpha auxiliary item.
//! * Alpha-carrying planar: `Yuva420P` (four planes, full-resolution
//!   alpha) and `Ya8` (interleaved luma + alpha).
//!
//! # Options
//!
//! Codec options (string map on [`CodecParameters::options`]):
//!
//! * `q` — AV1 `base_q_idx` for the colour planes, `0..=255`
//!   (default `0` = lossless).
//! * `alpha_q` — `base_q_idx` for the alpha auxiliary (default `0`).
//! * `premultiplied` — `true` to emit the `prem` iref declaring
//!   premultiplied alpha (default `false`).

use oxideav_core::frame::VideoFrame;
use oxideav_core::{
    CodecId, CodecParameters, Encoder, Error, Frame, Packet, PixelFormat, Result, TimeBase,
};

use crate::still::{encode_still, StillChroma, StillEncodeOptions, StillImage};

/// Frame-to-AVIF encoder: every video frame becomes one complete AVIF
/// file packet. See the module docs for the input-format mapping and
/// the option surface.
pub struct AvifEncoder {
    params: CodecParameters,
    opts: StillEncodeOptions,
    pending: Vec<Packet>,
    flushed: bool,
}

impl AvifEncoder {
    /// Build an encoder announcing `codec_id` in its output parameters.
    pub fn new(codec_id: CodecId) -> Self {
        Self::with_params(CodecParameters::video(codec_id))
    }

    /// Build an encoder from an explicit parameter set. Option parsing
    /// happens here (construction time), never on the frame path.
    pub fn with_params(params: CodecParameters) -> Self {
        let get_q = |key: &str| -> u8 {
            params
                .options
                .get(key)
                .and_then(|v| v.parse::<u8>().ok())
                .unwrap_or(0)
        };
        let opts = StillEncodeOptions {
            base_q_idx: get_q("q"),
            alpha_q_idx: get_q("alpha_q"),
            premultiplied_alpha: params
                .options
                .get("premultiplied")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
        };
        Self {
            params,
            opts,
            pending: Vec::new(),
            flushed: false,
        }
    }

    /// Convert one framework video frame into a [`StillImage`] per the
    /// declared `pixel_format`.
    fn frame_to_still(&self, vf: &VideoFrame) -> Result<StillImage> {
        let width = self
            .params
            .width
            .ok_or_else(|| Error::invalid("avif encode: CodecParameters.width required"))?;
        let height = self
            .params
            .height
            .ok_or_else(|| Error::invalid("avif encode: CodecParameters.height required"))?;
        let fmt = self
            .params
            .pixel_format
            .ok_or_else(|| Error::invalid("avif encode: CodecParameters.pixel_format required"))?;

        let img = match fmt {
            PixelFormat::Yuv420P | PixelFormat::YuvJ420P => planar8(
                vf,
                width,
                height,
                StillChroma::Yuv420,
                fmt == PixelFormat::YuvJ420P,
            )?,
            PixelFormat::Yuv422P | PixelFormat::YuvJ422P => planar8(
                vf,
                width,
                height,
                StillChroma::Yuv422,
                fmt == PixelFormat::YuvJ422P,
            )?,
            PixelFormat::Yuv444P | PixelFormat::YuvJ444P => planar8(
                vf,
                width,
                height,
                StillChroma::Yuv444,
                fmt == PixelFormat::YuvJ444P,
            )?,
            PixelFormat::Gray8 => planar8(vf, width, height, StillChroma::Monochrome, false)?,
            PixelFormat::Yuv420P10Le => planar16(vf, width, height, 10, StillChroma::Yuv420)?,
            PixelFormat::Yuv422P10Le => planar16(vf, width, height, 10, StillChroma::Yuv422)?,
            PixelFormat::Yuv444P10Le => planar16(vf, width, height, 10, StillChroma::Yuv444)?,
            PixelFormat::Yuv420P12Le => planar16(vf, width, height, 12, StillChroma::Yuv420)?,
            PixelFormat::Yuv422P12Le => planar16(vf, width, height, 12, StillChroma::Yuv422)?,
            PixelFormat::Yuv444P12Le => planar16(vf, width, height, 12, StillChroma::Yuv444)?,
            PixelFormat::Gray10Le => planar16(vf, width, height, 10, StillChroma::Monochrome)?,
            PixelFormat::Gray12Le => planar16(vf, width, height, 12, StillChroma::Monochrome)?,
            PixelFormat::Rgb24 => packed_rgb(vf, width, height, 3, [0, 1, 2])?,
            PixelFormat::Bgr24 => packed_rgb(vf, width, height, 3, [2, 1, 0])?,
            PixelFormat::Rgba => packed_rgb(vf, width, height, 4, [0, 1, 2])?,
            PixelFormat::Bgra => packed_rgb(vf, width, height, 4, [2, 1, 0])?,
            PixelFormat::Yuva420P => yuva420(vf, width, height)?,
            PixelFormat::Ya8 => ya8(vf, width, height)?,
            other => {
                return Err(Error::unsupported(format!(
                    "avif encode: pixel format {other:?} is not supported \
                     (planar YUV 8/10/12-bit, Gray, RGB(A)/BGR(A), Yuva420P, Ya8)"
                )))
            }
        };
        Ok(img)
    }
}

/// Extract one tightly-packed plane (`w × h` samples, 1 byte each)
/// from a frame plane whose stride may exceed the row width.
fn pack8(plane: &oxideav_core::frame::VideoPlane, w: usize, h: usize) -> Result<Vec<u16>> {
    if plane.data.len() < plane.stride * (h - 1) + w || plane.stride < w {
        return Err(Error::invalid(format!(
            "avif encode: plane too short (stride {}, need {w}x{h})",
            plane.stride
        )));
    }
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        let s = row * plane.stride;
        out.extend(plane.data[s..s + w].iter().map(|&b| b as u16));
    }
    Ok(out)
}

/// Extract one tightly-packed plane of little-endian 2-byte samples.
fn pack16le(plane: &oxideav_core::frame::VideoPlane, w: usize, h: usize) -> Result<Vec<u16>> {
    let row_bytes = w * 2;
    if plane.data.len() < plane.stride * (h - 1) + row_bytes || plane.stride < row_bytes {
        return Err(Error::invalid(format!(
            "avif encode: 16-bit plane too short (stride {}, need {w}x{h}x2)",
            plane.stride
        )));
    }
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        let s = row * plane.stride;
        out.extend(
            plane.data[s..s + row_bytes]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]])),
        );
    }
    Ok(out)
}

fn chroma_shift(chroma: StillChroma) -> (u32, u32) {
    match chroma {
        StillChroma::Yuv420 => (1, 1),
        StillChroma::Yuv422 => (1, 0),
        StillChroma::Yuv444 | StillChroma::Monochrome => (0, 0),
    }
}

fn planar8(
    vf: &VideoFrame,
    width: u32,
    height: u32,
    chroma: StillChroma,
    full_range: bool,
) -> Result<StillImage> {
    let planes_needed = if chroma == StillChroma::Monochrome {
        1
    } else {
        3
    };
    if vf.planes.len() != planes_needed {
        return Err(Error::invalid(format!(
            "avif encode: {chroma:?} expects {planes_needed} planes, got {}",
            vf.planes.len()
        )));
    }
    let (sx, sy) = chroma_shift(chroma);
    let y = pack8(&vf.planes[0], width as usize, height as usize)?;
    let (u, v) = if planes_needed == 3 {
        let (cw, ch) = ((width >> sx) as usize, (height >> sy) as usize);
        (pack8(&vf.planes[1], cw, ch)?, pack8(&vf.planes[2], cw, ch)?)
    } else {
        (Vec::new(), Vec::new())
    };
    let mut img = StillImage::yuv(width, height, 8, chroma, y, u, v)?;
    if full_range {
        // The J-variants declare full-range samples with unspecified
        // CICP; carry that through as an nclx colr (H.273 code 2 =
        // unspecified for each of the three fields).
        img = img.with_colr(crate::meta::Colr::Nclx {
            colour_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            full_range: true,
        });
    }
    Ok(img)
}

fn planar16(
    vf: &VideoFrame,
    width: u32,
    height: u32,
    bit_depth: u8,
    chroma: StillChroma,
) -> Result<StillImage> {
    let planes_needed = if chroma == StillChroma::Monochrome {
        1
    } else {
        3
    };
    if vf.planes.len() != planes_needed {
        return Err(Error::invalid(format!(
            "avif encode: {chroma:?} {bit_depth}-bit expects {planes_needed} planes, got {}",
            vf.planes.len()
        )));
    }
    let (sx, sy) = chroma_shift(chroma);
    let y = pack16le(&vf.planes[0], width as usize, height as usize)?;
    let (u, v) = if planes_needed == 3 {
        let (cw, ch) = ((width >> sx) as usize, (height >> sy) as usize);
        (
            pack16le(&vf.planes[1], cw, ch)?,
            pack16le(&vf.planes[2], cw, ch)?,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(StillImage::yuv(width, height, bit_depth, chroma, y, u, v)?)
}

/// Packed RGB(A)/BGR(A): `rgb_at` gives the byte offsets of R, G, B
/// within each `bpp`-byte pixel; a 4th byte (when `bpp == 4`) is the
/// alpha channel.
fn packed_rgb(
    vf: &VideoFrame,
    width: u32,
    height: u32,
    bpp: usize,
    rgb_at: [usize; 3],
) -> Result<StillImage> {
    if vf.planes.len() != 1 {
        return Err(Error::invalid(format!(
            "avif encode: packed RGB expects 1 plane, got {}",
            vf.planes.len()
        )));
    }
    let plane = &vf.planes[0];
    let (w, h) = (width as usize, height as usize);
    let row_bytes = w * bpp;
    if plane.data.len() < plane.stride * (h - 1) + row_bytes || plane.stride < row_bytes {
        return Err(Error::invalid(format!(
            "avif encode: packed plane too short (stride {}, need {w}x{h}x{bpp})",
            plane.stride
        )));
    }
    let mut rgb = Vec::with_capacity(w * h * 3);
    let mut alpha = if bpp == 4 {
        Vec::with_capacity(w * h)
    } else {
        Vec::new()
    };
    for row in 0..h {
        let s = row * plane.stride;
        for px in plane.data[s..s + row_bytes].chunks_exact(bpp) {
            rgb.push(px[rgb_at[0]]);
            rgb.push(px[rgb_at[1]]);
            rgb.push(px[rgb_at[2]]);
            if bpp == 4 {
                alpha.push(px[3] as u16);
            }
        }
    }
    let img = StillImage::rgb8(width, height, &rgb)?;
    if bpp == 4 {
        Ok(img.with_alpha(alpha)?)
    } else {
        Ok(img)
    }
}

fn yuva420(vf: &VideoFrame, width: u32, height: u32) -> Result<StillImage> {
    if vf.planes.len() != 4 {
        return Err(Error::invalid(format!(
            "avif encode: Yuva420P expects 4 planes, got {}",
            vf.planes.len()
        )));
    }
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (width.div_ceil(2) as usize, height.div_ceil(2) as usize);
    let img = StillImage::yuv(
        width,
        height,
        8,
        StillChroma::Yuv420,
        pack8(&vf.planes[0], w, h)?,
        pack8(&vf.planes[1], cw, ch)?,
        pack8(&vf.planes[2], cw, ch)?,
    )?;
    Ok(img.with_alpha(pack8(&vf.planes[3], w, h)?)?)
}

fn ya8(vf: &VideoFrame, width: u32, height: u32) -> Result<StillImage> {
    if vf.planes.len() != 1 {
        return Err(Error::invalid(format!(
            "avif encode: Ya8 expects 1 interleaved plane, got {}",
            vf.planes.len()
        )));
    }
    let plane = &vf.planes[0];
    let (w, h) = (width as usize, height as usize);
    let row_bytes = w * 2;
    if plane.data.len() < plane.stride * (h - 1) + row_bytes || plane.stride < row_bytes {
        return Err(Error::invalid(format!(
            "avif encode: Ya8 plane too short (stride {}, need {w}x{h}x2)",
            plane.stride
        )));
    }
    let mut y = Vec::with_capacity(w * h);
    let mut a = Vec::with_capacity(w * h);
    for row in 0..h {
        let s = row * plane.stride;
        for px in plane.data[s..s + row_bytes].chunks_exact(2) {
            y.push(px[0] as u16);
            a.push(px[1] as u16);
        }
    }
    let img = StillImage::yuv(
        width,
        height,
        8,
        StillChroma::Monochrome,
        y,
        Vec::new(),
        Vec::new(),
    )?;
    Ok(img.with_alpha(a)?)
}

impl Encoder for AvifEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            other => {
                return Err(Error::invalid(format!(
                    "avif encode: expected a video frame, got {other:?}"
                )))
            }
        };
        let img = self.frame_to_still(vf)?;
        let bytes = encode_still(&img, &self.opts)?;
        let mut pkt = Packet::new(0, TimeBase::new(1, 90_000), bytes);
        if let Some(pts) = vf.pts {
            pkt = pkt.with_pts(pts);
        }
        pkt = pkt.with_keyframe(true);
        self.pending.push(pkt);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if self.pending.is_empty() {
            return if self.flushed {
                Err(Error::Eof)
            } else {
                Err(Error::NeedMore)
            };
        }
        Ok(self.pending.remove(0))
    }

    fn flush(&mut self) -> Result<()> {
        self.flushed = true;
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
    use oxideav_core::frame::{VideoFrame, VideoPlane};
    use oxideav_core::Decoder;

    fn video_params(fmt: PixelFormat, w: u32, h: u32) -> CodecParameters {
        let mut p = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
        p.width = Some(w);
        p.height = Some(h);
        p.pixel_format = Some(fmt);
        p
    }

    fn planar_frame(dims: &[(usize, usize)], seed: u8) -> Frame {
        Frame::Video(VideoFrame {
            pts: Some(7),
            planes: dims
                .iter()
                .enumerate()
                .map(|(i, &(w, h))| VideoPlane {
                    stride: w,
                    data: (0..w * h)
                        .map(|j| (j as u8).wrapping_mul(31) ^ seed.wrapping_add(i as u8))
                        .collect(),
                })
                .collect(),
        })
    }

    /// Trait-level end-to-end: send a Yuv420P frame, receive one
    /// complete AVIF file packet, decode it back pixel-exact through
    /// the crate's own decoder.
    #[test]
    fn encoder_trait_round_trips_yuv420_frame() {
        let params = video_params(PixelFormat::Yuv420P, 16, 16);
        let mut enc = make_encoder(&params).expect("make encoder");
        let frame = planar_frame(&[(16, 16), (8, 8), (8, 8)], 3);
        enc.send_frame(&frame).expect("send_frame");
        let pkt = enc.receive_packet().expect("receive_packet");
        assert_eq!(pkt.pts, Some(7), "pts carried through");
        assert!(pkt.flags.keyframe, "still images are sync samples");

        // NeedMore until the next frame, Eof after flush.
        assert!(matches!(enc.receive_packet(), Err(Error::NeedMore)));
        enc.flush().expect("flush");
        assert!(matches!(enc.receive_packet(), Err(Error::Eof)));

        // The packet is a complete AVIF file that decodes back to the
        // input planes exactly (lossless default).
        let mut dec = crate::decoder::AvifDecoder::new(CodecId::new(crate::CODEC_ID_STR));
        dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), pkt.data.clone()))
            .expect("decode muxed file");
        let vf = match dec.receive_frame().expect("frame") {
            Frame::Video(v) => v,
            other => panic!("expected VideoFrame, got {other:?}"),
        };
        let src = match &frame {
            Frame::Video(v) => v,
            _ => unreachable!(),
        };
        assert_eq!(vf.planes.len(), 3);
        for i in 0..3 {
            assert_eq!(vf.planes[i].data, src.planes[i].data, "plane {i}");
        }
    }

    /// Rgba input: the trait path routes through the identity-matrix
    /// 4:4:4 encode with an alpha auxiliary; a lossy `q` option
    /// produces a smaller file than lossless.
    #[test]
    fn encoder_trait_rgba_and_q_option() {
        let (w, h) = (16u32, 16u32);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| ((i * 13) & 0xff) as u8).collect();
        let mk_frame = || {
            Frame::Video(VideoFrame {
                pts: Some(0),
                planes: vec![VideoPlane {
                    stride: (w * 4) as usize,
                    data: rgba.clone(),
                }],
            })
        };
        let params = video_params(PixelFormat::Rgba, w, h);
        let mut enc = make_encoder(&params).expect("encoder");
        enc.send_frame(&mk_frame()).expect("send");
        let lossless = enc.receive_packet().expect("packet");
        let info = crate::inspect::inspect(&lossless.data).expect("inspect");
        assert!(info.has_alpha, "alpha auxiliary present");

        let mut lossy_params = video_params(PixelFormat::Rgba, w, h);
        lossy_params.options = oxideav_core::CodecOptions::new().set("q", "120");
        let mut enc = make_encoder(&lossy_params).expect("encoder");
        enc.send_frame(&mk_frame()).expect("send");
        let lossy = enc.receive_packet().expect("packet");
        assert!(
            lossy.data.len() < lossless.data.len(),
            "q=120 ({}) must be smaller than lossless ({})",
            lossy.data.len(),
            lossless.data.len()
        );
    }

    /// Missing parameters and unsupported formats surface precise
    /// errors instead of panics.
    #[test]
    fn encoder_trait_rejects_bad_setup() {
        // No width/height/pixel_format.
        let params = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
        let mut enc = make_encoder(&params).expect("encoder");
        let frame = planar_frame(&[(8, 8)], 0);
        match enc.send_frame(&frame) {
            Err(Error::InvalidData(msg)) => assert!(msg.contains("width"), "{msg}"),
            other => panic!("expected InvalidData, got {other:?}"),
        }
        // Unsupported pixel format.
        let params = video_params(PixelFormat::Pal8, 8, 8);
        let mut enc = make_encoder(&params).expect("encoder");
        match enc.send_frame(&frame) {
            Err(Error::Unsupported(msg)) => assert!(msg.contains("Pal8"), "{msg}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
