#![no_main]

//! Every encode path at fuzz-chosen extents (1..=48 per axis — odd,
//! off the 8-sample grid, one sample wide): a still, an image
//! sequence (KEY + P groups), a spatially layered item, an image
//! burst through the registry decoder, an explicit `grid` (64..=199
//! per axis, 1..=3 tiles per axis — a sub-sampled layout at an odd
//! extent is tiled over the even canvas and cropped by a `clap`).
//! Layout (4:2:0 / 4:2:2 / 4:4:4 /
//! monochrome), depth (8 / 10 / 12), alpha and the quantiser come from
//! the input, the samples are the remaining bytes. Lossless requests
//! must decode back sample for sample; lossy ones must decode to the
//! same geometry. Nothing may panic.

use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use oxideav_avif::{
    AvifImage, AvifPixelFormat, AvifPlane, DecodeOptions, EncodeOptions, Frame, StillChroma,
    StillImage, decode, decode_all, decode_with, encode, encode_all, encode_still_grid,
    encode_still_layered,
};
use oxideav_core::Decoder;

#[derive(Clone, Copy, PartialEq)]
enum Layout {
    Yuv420,
    Yuv422,
    Yuv444,
    Gray,
}

fn format(layout: Layout, depth: u8, alpha: bool) -> AvifPixelFormat {
    use AvifPixelFormat::*;
    match (layout, depth, alpha) {
        (Layout::Yuv420, 8, false) => Yuv420P,
        (Layout::Yuv420, 8, true) => Yuva420P,
        (Layout::Yuv420, 10, false) => Yuv420P10Le,
        (Layout::Yuv420, 10, true) => Yuva420P10Le,
        (Layout::Yuv420, _, false) => Yuv420P12Le,
        (Layout::Yuv420, _, true) => Yuva420P12Le,
        (Layout::Yuv422, 8, false) => Yuv422P,
        (Layout::Yuv422, 8, true) => Yuva422P,
        (Layout::Yuv422, 10, false) => Yuv422P10Le,
        (Layout::Yuv422, 10, true) => Yuva422P10Le,
        (Layout::Yuv422, _, false) => Yuv422P12Le,
        (Layout::Yuv422, _, true) => Yuva422P12Le,
        (Layout::Yuv444, 8, false) => Yuv444P,
        (Layout::Yuv444, 8, true) => Yuva444P,
        (Layout::Yuv444, 10, false) => Yuv444P10Le,
        (Layout::Yuv444, 10, true) => Yuva444P10Le,
        (Layout::Yuv444, _, false) => Yuv444P12Le,
        (Layout::Yuv444, _, true) => Yuva444P12Le,
        (Layout::Gray, 8, _) => Gray8,
        (Layout::Gray, 10, _) => Gray10Le,
        (Layout::Gray, _, _) => Gray12Le,
    }
}

/// A sample source cycling over the fuzz bytes.
struct Samples<'a> {
    data: &'a [u8],
    at: usize,
    salt: u32,
}

impl Samples<'_> {
    fn next(&mut self, depth: u8) -> u16 {
        let mut word = self.salt;
        for _ in 0..2 {
            let b = if self.data.is_empty() {
                0
            } else {
                self.data[self.at % self.data.len()]
            };
            self.at += 1;
            word = (word << 8) | u32::from(b);
        }
        self.salt = self.salt.wrapping_mul(31).wrapping_add(7) & 0xff;
        (word & ((1u32 << depth) - 1)) as u16
    }

    fn plane(&mut self, n: usize, depth: u8) -> Vec<u16> {
        (0..n).map(|_| self.next(depth)).collect()
    }
}

struct Picture {
    w: u32,
    h: u32,
    depth: u8,
    layout: Layout,
    y: Vec<u16>,
    u: Vec<u16>,
    v: Vec<u16>,
    a: Option<Vec<u16>>,
}

fn picture(
    src: &mut Samples<'_>,
    w: u32,
    h: u32,
    layout: Layout,
    depth: u8,
    alpha: bool,
) -> Picture {
    let (sx, sy) = match layout {
        Layout::Yuv420 => (1, 1),
        Layout::Yuv422 => (1, 0),
        _ => (0, 0),
    };
    let n = (w * h) as usize;
    let c = (((w + sx) >> sx) * ((h + sy) >> sy)) as usize;
    let y = src.plane(n, depth);
    let (u, v) = if layout == Layout::Gray {
        (Vec::new(), Vec::new())
    } else {
        (src.plane(c, depth), src.plane(c, depth))
    };
    let a = alpha.then(|| src.plane(n, depth));
    Picture {
        w,
        h,
        depth,
        layout,
        y,
        u,
        v,
        a,
    }
}

fn bytes_of(samples: &[u16], depth: u8) -> Vec<u8> {
    if depth == 8 {
        samples.iter().map(|&s| s as u8).collect()
    } else {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }
}

impl Picture {
    fn image(&self) -> AvifImage {
        let bps = if self.depth == 8 { 1 } else { 2 };
        let sx = u32::from(matches!(self.layout, Layout::Yuv420 | Layout::Yuv422));
        let cw = ((self.w + sx) >> sx) as usize;
        let mut planes = vec![AvifPlane::new(
            self.w as usize * bps,
            bytes_of(&self.y, self.depth),
        )];
        if self.layout != Layout::Gray {
            planes.push(AvifPlane::new(cw * bps, bytes_of(&self.u, self.depth)));
            planes.push(AvifPlane::new(cw * bps, bytes_of(&self.v, self.depth)));
        }
        if let Some(a) = &self.a {
            planes.push(AvifPlane::new(
                self.w as usize * bps,
                bytes_of(a, self.depth),
            ));
        }
        AvifImage::new(
            self.w,
            self.h,
            format(self.layout, self.depth, self.a.is_some()),
            planes,
        )
        .expect("generated geometry")
        .with_bit_depth(self.depth)
    }

    fn still(&self) -> StillImage {
        let chroma = match self.layout {
            Layout::Yuv420 => StillChroma::Yuv420,
            Layout::Yuv422 => StillChroma::Yuv422,
            Layout::Yuv444 => StillChroma::Yuv444,
            Layout::Gray => StillChroma::Monochrome,
        };
        let img = StillImage::yuv(
            self.w,
            self.h,
            self.depth,
            chroma,
            self.y.clone(),
            self.u.clone(),
            self.v.clone(),
        )
        .expect("generated geometry");
        match &self.a {
            Some(a) => img.with_alpha(a.clone()).expect("alpha geometry"),
            None => img,
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 6 {
        return;
    }
    let mode = data[0] % 5;
    let w = u32::from(data[1] % 48) + 1;
    let h = u32::from(data[2] % 48) + 1;
    let layout =
        [Layout::Yuv420, Layout::Yuv422, Layout::Yuv444, Layout::Gray][usize::from(data[3] & 3)];
    let depth = [8u8, 10, 12, 8][usize::from((data[3] >> 2) & 3)];
    // Gray + alpha is a packed layout with its own harness.
    let alpha = data[3] & 0x10 != 0 && layout != Layout::Gray;
    let lossless = data[4] & 1 == 0;
    let q = data[4] | 1;
    let mut src = Samples {
        data: &data[6..],
        at: 0,
        salt: u32::from(data[5]),
    };
    let opts = if lossless {
        EncodeOptions::default().with_lossless(true)
    } else {
        EncodeOptions::default()
            .with_base_q_idx(q)
            .with_alpha_q_idx(q)
    };

    match mode {
        // A still.
        0 => {
            let pic = picture(&mut src, w, h, layout, depth, alpha).image();
            let file = encode(&pic, &opts).expect("still encode");
            let back = decode(&file).expect("still decode");
            assert_eq!((back.width, back.height, back.format), (w, h, pic.format));
            if lossless {
                assert!(back.planes == pic.planes, "lossless still differs");
            }
        }
        // An image sequence: KEY + P groups at the picture's size.
        1 => {
            let count = 2 + usize::from(data[5] % 3);
            let frames: Vec<Frame> = (0..count)
                .map(|_| {
                    Frame::new(
                        picture(&mut src, w, h, layout, depth, alpha).image(),
                        Some(Duration::from_millis(40)),
                    )
                })
                .collect();
            let file = encode_all(&frames, &opts).expect("sequence encode");
            let back = decode_all(&file).expect("sequence decode");
            assert_eq!(back.len(), count);
            for (got, want) in back.iter().zip(&frames) {
                assert_eq!(
                    (got.image.width, got.image.height, got.image.format),
                    (w, h, want.image.format)
                );
                if lossless {
                    assert!(
                        got.image.planes == want.image.planes,
                        "lossless frame differs"
                    );
                }
            }
        }
        // A layered item: the lower layers shrink by fuzz-chosen
        // amounts, every layer at its own extents.
        2 => {
            let count = 2 + usize::from(data[5] & 1);
            let mut sizes = vec![(w, h)];
            for i in 1..count {
                let (pw, ph) = sizes[0];
                let cut = u32::from(data[5] >> (2 * i)) & 3;
                sizes.insert(
                    0,
                    ((pw / 2 + cut).clamp(1, pw), (ph / 2 + cut).clamp(1, ph)),
                );
            }
            let pics: Vec<Picture> = sizes
                .iter()
                .enumerate()
                .map(|(i, &(lw, lh))| {
                    picture(&mut src, lw, lh, layout, depth, alpha && i == count - 1)
                })
                .collect();
            let stills: Vec<StillImage> = pics.iter().map(Picture::still).collect();
            let layers: Vec<&StillImage> = stills.iter().collect();
            let file = encode_still_layered(&layers, None, &opts).expect("layered encode");
            let top = decode(&file).expect("layered decode");
            assert_eq!((top.width, top.height), (w, h));
            if lossless {
                assert!(
                    top.planes == pics[count - 1].image().planes,
                    "lossless top layer differs"
                );
            }
            for (i, pic) in pics.iter().enumerate() {
                let got = decode_with(&file, &DecodeOptions::default().with_layer(Some(i as u16)))
                    .expect("layer decode");
                assert_eq!((got.width, got.height), (pic.w, pic.h));
                if lossless {
                    let colour = if layout == Layout::Gray { 1 } else { 3 };
                    assert!(
                        got.planes[..colour] == pic.image().planes[..colour],
                        "lossless layer differs"
                    );
                }
            }
        }
        // An explicit grid at 64..=199 per axis. A tiling the grid
        // rules refuse (a tile under the 64-sample floor, a fully
        // trimmed tile) is a typed error; an accepted one decodes at
        // the picture's size with the source's luma and alpha (the
        // chroma of an odd sub-sampled picture comes back upsampled).
        4 => {
            let (gw, gh) = (64 + u32::from(data[1]) % 136, 64 + u32::from(data[2]) % 136);
            let (columns, rows) = (
                1 + u16::from(data[5] % 3),
                1 + u16::from((data[5] >> 2) % 3),
            );
            let pic = picture(&mut src, gw, gh, layout, depth, alpha);
            let Ok(file) = encode_still_grid(&pic.still(), &opts, columns, rows) else {
                return;
            };
            let back = decode(&file).expect("grid decode");
            assert_eq!((back.width, back.height), (gw, gh));
            if lossless {
                let want = pic.image();
                assert!(
                    back.planes[0] == want.planes[0],
                    "lossless grid luma differs"
                );
                if alpha {
                    assert!(
                        back.planes.last() == want.planes.last(),
                        "lossless grid alpha differs"
                    );
                }
                if back.format == want.format {
                    assert!(back.planes == want.planes, "lossless grid differs");
                }
            }
        }
        // An image burst: the contract's `decode_all` and the registry
        // decoder return the same members.
        _ => {
            let count = 2 + usize::from(data[5] % 3);
            let frames: Vec<Frame> = (0..count)
                .map(|_| Frame::new(picture(&mut src, w, h, layout, depth, alpha).image(), None))
                .collect();
            let file = encode_all(&frames, &opts).expect("burst encode");
            let back = decode_all(&file).expect("burst decode");
            assert_eq!(back.len(), count);
            if lossless {
                for (got, want) in back.iter().zip(&frames) {
                    assert!(
                        got.image.planes == want.image.planes,
                        "lossless member differs"
                    );
                }
            }
            let mut dec = oxideav_avif::AvifDecoder::new(oxideav_core::CodecId::new(
                oxideav_avif::CODEC_ID_STR,
            ));
            dec.send_packet(&oxideav_core::Packet::new(
                0,
                oxideav_core::TimeBase::new(1, 1),
                file,
            ))
            .expect("registry send_packet");
            let mut seen = 0;
            loop {
                match dec.receive_frame() {
                    Ok(oxideav_core::Frame::Video(v)) => {
                        let want = &back[seen].image;
                        for (got, exp) in v.planes.iter().zip(&want.planes) {
                            assert!(got.data == exp.data, "registry member differs");
                        }
                        seen += 1;
                    }
                    Ok(_) => {}
                    Err(oxideav_core::Error::NeedMore) => break,
                    Err(e) => panic!("registry receive_frame: {e}"),
                }
            }
            assert_eq!(seen, count);
        }
    }
});
