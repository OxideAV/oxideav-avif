#![no_main]

//! The registry encoder's option and blob paths on arbitrary bytes:
//! the bytes pick one of the declared input layouts, a small geometry
//! (odd extents included, so the sub-sampled layouts' typed refusal is
//! exercised), every codec option (`quality` / `q` / `alpha_q` /
//! `chroma` / `premultiplied` / `speed` / `threads` / `grid`, in and
//! out of range, malformed), stream- and frame-level metadata blobs
//! (well-known and custom kinds, empty and sized payloads) and the
//! picture's samples. The invariant: construction and encoding never
//! panic (a bad option or geometry is a typed `Err`), a produced file
//! decodes through the registry decoder to the encoder's announced
//! label and geometry, and the ICC / Exif / XMP the frame carried
//! (refined over the stream's) come back as the frame's blobs — no
//! other kind does.

use libfuzzer_sys::fuzz_target;
use oxideav_avif::{ENCODER_PIXEL_FORMATS, make_decoder, make_encoder};
use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{
    BlobKind, CodecId, CodecOptions, CodecParameters, Frame, MetadataBlob, Packet, PixelFormat,
    TimeBase,
};

/// A byte cursor over the input; runs out gracefully (zeros).
struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }
    fn take(&mut self, n: usize) -> &'a [u8] {
        let start = self.pos.min(self.data.len());
        let end = (start + n).min(self.data.len());
        self.pos = end;
        &self.data[start..end]
    }
}

/// Plane geometry of a declared layout: `(subsampling per plane,
/// bytes per sample, channels per sample)` as the encoder reads it.
fn geometry(fmt: PixelFormat) -> (Vec<(u32, u32)>, usize, usize) {
    use PixelFormat as P;
    match fmt {
        P::Yuv420P | P::YuvJ420P => (vec![(0, 0), (1, 1), (1, 1)], 1, 1),
        P::Yuv422P | P::YuvJ422P => (vec![(0, 0), (1, 0), (1, 0)], 1, 1),
        P::Yuv444P | P::YuvJ444P | P::Gbrp8 => (vec![(0, 0), (0, 0), (0, 0)], 1, 1),
        P::Gray8 => (vec![(0, 0)], 1, 1),
        P::Yuv420P10Le | P::Yuv420P12Le => (vec![(0, 0), (1, 1), (1, 1)], 2, 1),
        P::Yuv422P10Le | P::Yuv422P12Le => (vec![(0, 0), (1, 0), (1, 0)], 2, 1),
        P::Yuv444P10Le | P::Yuv444P12Le | P::Gbrp10Le | P::Gbrp12Le => {
            (vec![(0, 0), (0, 0), (0, 0)], 2, 1)
        }
        P::Gray10Le | P::Gray12Le => (vec![(0, 0)], 2, 1),
        P::Rgb24 | P::Bgr24 => (vec![(0, 0)], 1, 3),
        P::Rgba | P::Bgra => (vec![(0, 0)], 1, 4),
        P::Gbrap8 => (vec![(0, 0), (0, 0), (0, 0), (0, 0)], 1, 1),
        P::Gbrap10Le | P::Gbrap12Le => (vec![(0, 0), (0, 0), (0, 0), (0, 0)], 2, 1),
        P::Yuva420P => (vec![(0, 0), (1, 1), (1, 1), (0, 0)], 1, 1),
        P::Ya8 => (vec![(0, 0)], 1, 2),
        _ => (vec![(0, 0)], 1, 1),
    }
}

fn depth_of(fmt: PixelFormat) -> u32 {
    use PixelFormat as P;
    match fmt {
        P::Yuv420P10Le
        | P::Yuv422P10Le
        | P::Yuv444P10Le
        | P::Gray10Le
        | P::Gbrp10Le
        | P::Gbrap10Le => 10,
        P::Yuv420P12Le
        | P::Yuv422P12Le
        | P::Yuv444P12Le
        | P::Gray12Le
        | P::Gbrp12Le
        | P::Gbrap12Le => 12,
        _ => 8,
    }
}

const OPTION_WORDS: &[&str] = &[
    "", "0", "1", "60", "100", "101", "255", "256", "auto", "none", "single", "64", "63", "4096",
    "4097", "512", "420", "422", "444", "fast", "balanced", "true", "false", "yes", "-1", "x",
];

fn blob_of(bytes: &mut Bytes<'_>) -> MetadataBlob {
    let kind = match bytes.u8() % 7 {
        0 => BlobKind::ICC,
        1 => BlobKind::EXIF,
        2 => BlobKind::XMP,
        3 => BlobKind::IPTC,
        4 => BlobKind::COVER_ART,
        5 => BlobKind::custom("avif-fuzz"),
        _ => BlobKind::custom(""),
    };
    let len = usize::from(bytes.u8()) % 48;
    let data = bytes.take(len).to_vec();
    let blob = MetadataBlob::new(kind, data);
    if bytes.u8() & 1 != 0 {
        blob.with_mime("application/octet-stream")
    } else {
        blob
    }
}

fuzz_target!(|data: &[u8]| {
    let mut bytes = Bytes { data, pos: 0 };
    let fmt = ENCODER_PIXEL_FORMATS[usize::from(bytes.u8()) % ENCODER_PIXEL_FORMATS.len()];
    let w = u32::from(bytes.u8() % 24) + 1;
    let h = u32::from(bytes.u8() % 24) + 1;

    // Options: each present with a byte-chosen word (in or out of
    // range, malformed), the `speed` kept away from the slow
    // searches so a run stays bounded.
    let mut opts = CodecOptions::new();
    let knobs = bytes.u8();
    let word = |b: u8| OPTION_WORDS[usize::from(b) % OPTION_WORDS.len()];
    if knobs & 1 != 0 {
        opts = opts.set("quality", word(bytes.u8()));
    }
    if knobs & 2 != 0 {
        opts = opts.set("q", word(bytes.u8()));
    }
    if knobs & 4 != 0 {
        opts = opts.set("alpha_q", word(bytes.u8()));
    }
    if knobs & 8 != 0 {
        opts = opts.set("chroma", word(bytes.u8()));
    }
    if knobs & 16 != 0 {
        opts = opts.set("premultiplied", word(bytes.u8()));
    }
    if knobs & 32 != 0 {
        opts = opts.set("threads", word(bytes.u8() % 4));
    }
    if knobs & 64 != 0 {
        opts = opts.set("grid", word(bytes.u8()));
    }
    if knobs & 128 != 0 {
        opts = opts.set("speed", "fast");
    }
    let unknown = bytes.u8();
    if unknown.is_multiple_of(5) {
        opts = opts.set("no-such-option", "1");
    }

    let mut params = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
    params.width = Some(w);
    params.height = Some(h);
    params.pixel_format = Some(fmt);
    params.options = opts;
    let stream_blobs = usize::from(bytes.u8() % 4);
    for _ in 0..stream_blobs {
        let b = blob_of(&mut bytes);
        params.blobs.push(b);
    }

    // The picture: the declared geometry, samples from the bytes.
    let (planes, bps, channels) = geometry(fmt);
    let depth = depth_of(fmt);
    let mut out = Vec::new();
    for &(sx, sy) in &planes {
        let (pw, ph) = ((w >> sx) as usize, (h >> sy) as usize);
        let stride = pw * channels * bps;
        let mut data = Vec::with_capacity(stride * ph);
        for _ in 0..stride * ph / bps.max(1) {
            let v8 = bytes.u8();
            if bps == 1 {
                data.push(v8);
            } else {
                let v = (u32::from(v8) << (depth - 8)) | (u32::from(v8) >> (16 - depth));
                data.extend_from_slice(&(v as u16).to_le_bytes());
            }
        }
        out.push(VideoPlane { stride, data });
    }
    let mut vf = VideoFrame {
        pts: Some(0),
        planes: out,
    };
    let frame_blobs = usize::from(bytes.u8() % 4);
    let mut fb = Vec::new();
    for _ in 0..frame_blobs {
        fb.push(blob_of(&mut bytes));
    }
    if !fb.is_empty() {
        vf.set_blobs(fb.clone());
    }

    // Construction refuses bad options by type; encoding refuses bad
    // geometry by type; neither panics.
    let Ok(mut enc) = make_encoder(&params) else {
        return;
    };
    if enc.send_frame(&Frame::Video(vf.clone())).is_err() {
        return;
    }
    let pkt = enc
        .receive_packet()
        .expect("a sent frame yields its packet");
    let label = enc
        .output_params()
        .pixel_format
        .expect("the encoder announces the coded label");

    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(
        oxideav_avif::CODEC_ID_STR,
    )))
    .expect("decoder");
    dec.send_packet(&Packet::new(0, TimeBase::new(1, 1), pkt.data))
        .expect("the encoder's file decodes");
    let decoded = match dec.receive_frame().expect("one frame") {
        Frame::Video(v) => v,
        other => panic!("not a video frame: {other:?}"),
    };
    assert_eq!(dec.output_pixel_format(), Some(label), "{fmt:?}");
    assert!(
        decoded.planes[0].data.len() >= w as usize,
        "{fmt:?}: luma covers the width"
    );

    // Metadata: the frame's ICC / Exif / XMP refined over the stream's
    // come back on the decoded frame, nothing else.
    let want = oxideav_avif::Metadata::from_refined_blobs(params.blobs(), &fb);
    let got = oxideav_avif::Metadata::from_blobs(&decoded.blobs());
    assert_eq!(got.icc, want.icc, "{fmt:?}: ICC");
    assert_eq!(got.exif, want.exif, "{fmt:?}: Exif");
    assert_eq!(got.xmp, want.xmp, "{fmt:?}: XMP");
    assert!(
        decoded
            .blobs()
            .iter()
            .all(|b| b.is(&BlobKind::ICC) || b.is(&BlobKind::EXIF) || b.is(&BlobKind::XMP))
    );
});
