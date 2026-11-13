//! Every picture size codes on every path: stills, image sequences
//! (KEY + P groups — inter prediction, not an all-intra fallback) and
//! spatially layered items at extents that are odd, a multiple of 2
//! but not 4, of 4 but not 8, and off the 8-sample grid on one axis
//! only — 4:2:0 / 4:2:2 / 4:4:4 / monochrome, 8 and 10 bits, with and
//! without alpha. Sub-sampled chroma at an odd extent has the
//! rounded-up plane `(extent + 1) >> 1`. Everything is lossless here,
//! so the decoded planes equal the source; the black-box legs hand
//! the same files to the readers installed on the host.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use oxideav_avif::{
    AvifImage, AvifPixelFormat, AvifPlane, DecodeOptions, EncodeOptions, Frame,
    SequenceEncodeOptions, StillChroma, StillImage, decode, decode_all, decode_with, encode,
    encode_all, encode_sequence, encode_still_layered, inspect, parse_avis,
};

/// The extents of the matrix: 1 × 1, odd × odd, ×2 (not ×4), ×4 (not
/// ×8), ×8, one axis off the grid, and sizes past one 64-sample
/// superblock.
const EXTENTS: &[(u32, u32)] = &[
    (1, 1),
    (3, 5),
    (17, 13),
    (34, 18),
    (36, 20),
    (40, 24),
    (40, 21),
    (23, 32),
    (67, 35),
    (130, 66),
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    Yuv420,
    Yuv422,
    Yuv444,
    Gray,
}

impl Layout {
    fn shifts(self) -> (u32, u32) {
        match self {
            Layout::Yuv420 => (1, 1),
            Layout::Yuv422 => (1, 0),
            Layout::Yuv444 | Layout::Gray => (0, 0),
        }
    }

    fn format(self, depth: u8, alpha: bool) -> AvifPixelFormat {
        use AvifPixelFormat::*;
        match (self, depth, alpha) {
            (Layout::Yuv420, 8, false) => Yuv420P,
            (Layout::Yuv420, 8, true) => Yuva420P,
            (Layout::Yuv420, _, false) => Yuv420P10Le,
            (Layout::Yuv420, _, true) => Yuva420P10Le,
            (Layout::Yuv422, 8, false) => Yuv422P,
            (Layout::Yuv422, 8, true) => Yuva422P,
            (Layout::Yuv422, _, false) => Yuv422P10Le,
            (Layout::Yuv422, _, true) => Yuva422P10Le,
            (Layout::Yuv444, 8, false) => Yuv444P,
            (Layout::Yuv444, 8, true) => Yuva444P,
            (Layout::Yuv444, _, false) => Yuv444P10Le,
            (Layout::Yuv444, _, true) => Yuva444P10Le,
            (Layout::Gray, 8, _) => Gray8,
            (Layout::Gray, _, _) => Gray10Le,
        }
    }

    fn still(self) -> StillChroma {
        match self {
            Layout::Yuv420 => StillChroma::Yuv420,
            Layout::Yuv422 => StillChroma::Yuv422,
            Layout::Yuv444 => StillChroma::Yuv444,
            Layout::Gray => StillChroma::Monochrome,
        }
    }
}

fn noise(n: usize, seed: u32) -> Vec<u16> {
    let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(12_345);
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 16) as u16
        })
        .collect()
}

/// Frame `t` of a `pw × ph` plane: a fixed textured background (so a
/// P frame has something to predict from) under a block that moves
/// two samples per frame, values in `0..1 << depth`.
fn plane(pw: usize, ph: usize, depth: u8, seed: u32, t: u32) -> Vec<u16> {
    textured_plane(pw, ph, depth, seed, t, 0x7)
}

/// [`plane`] with `grain` masking the background's noise bits (`0` =
/// a clean ramp).
fn textured_plane(pw: usize, ph: usize, depth: u8, seed: u32, t: u32, grain: u32) -> Vec<u16> {
    let max = (1u32 << depth) - 1;
    let bg = noise(pw * ph, seed);
    let mut out = Vec::with_capacity(pw * ph);
    for y in 0..ph {
        for x in 0..pw {
            let ramp = (x as u32 * max / pw.max(1) as u32 + y as u32 * 3) & max;
            let moving = (x + 2 * t as usize) % 11 < 4 && (y + t as usize) % 9 < 5;
            out.push(if moving {
                (max - ramp) as u16
            } else {
                (ramp ^ (u32::from(bg[y * pw + x]) & grain)) as u16
            });
        }
    }
    out
}

fn bytes_of(samples: &[u16], depth: u8) -> Vec<u8> {
    if depth == 8 {
        samples.iter().map(|&s| s as u8).collect()
    } else {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }
}

/// The source planes of one picture: Y, Cb, Cr (empty for gray), alpha.
struct Source {
    w: u32,
    h: u32,
    depth: u8,
    layout: Layout,
    y: Vec<u16>,
    u: Vec<u16>,
    v: Vec<u16>,
    a: Option<Vec<u16>>,
}

fn source(w: u32, h: u32, layout: Layout, depth: u8, alpha: bool, t: u32) -> Source {
    let (sx, sy) = layout.shifts();
    let (cw, ch) = (((w + sx) >> sx) as usize, ((h + sy) >> sy) as usize);
    let (wu, hu) = (w as usize, h as usize);
    let (u, v) = if layout == Layout::Gray {
        (Vec::new(), Vec::new())
    } else {
        (plane(cw, ch, depth, 2, t), plane(cw, ch, depth, 3, t))
    };
    Source {
        w,
        h,
        depth,
        layout,
        y: plane(wu, hu, depth, 1, t),
        u,
        v,
        a: alpha.then(|| plane(wu, hu, depth, 4, t + 1)),
    }
}

impl Source {
    fn image(&self) -> AvifImage {
        let bps = if self.depth == 8 { 1 } else { 2 };
        let (sx, _) = self.layout.shifts();
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
            self.layout.format(self.depth, self.a.is_some()),
            planes,
        )
        .expect("source geometry")
        .with_bit_depth(self.depth)
    }

    fn still(&self) -> StillImage {
        let img = StillImage::yuv(
            self.w,
            self.h,
            self.depth,
            self.layout.still(),
            self.y.clone(),
            self.u.clone(),
            self.v.clone(),
        )
        .expect("still geometry");
        match &self.a {
            Some(a) => img.with_alpha(a.clone()).expect("alpha geometry"),
            None => img,
        }
    }
}

/// Gray + alpha is a packed layout on the contract (`Ya8`), covered by
/// its own tests; the matrix carries alpha on the colour layouts.
fn cells() -> Vec<(Layout, u8, bool)> {
    let mut out = Vec::new();
    for layout in [Layout::Yuv420, Layout::Yuv422, Layout::Yuv444, Layout::Gray] {
        for depth in [8u8, 10] {
            for alpha in [false, true] {
                if layout == Layout::Gray && alpha {
                    continue;
                }
                out.push((layout, depth, alpha));
            }
        }
    }
    out
}

#[test]
fn stills_round_trip_at_every_extent() {
    let mut count = 0;
    for &(w, h) in EXTENTS {
        for (layout, depth, alpha) in cells() {
            let label = format!("{w}x{h} {layout:?} {depth}-bit alpha={alpha}");
            let src = source(w, h, layout, depth, alpha, 0).image();
            let file = encode(&src, &EncodeOptions::default()).unwrap_or_else(|e| {
                panic!("{label}: encode: {e}");
            });
            let info = inspect(&file).expect("inspect");
            assert_eq!((info.width, info.height), (w, h), "{label}: ispe");
            assert!(
                !file.windows(4).any(|b| b == b"clap"),
                "{label}: no clean aperture"
            );
            let back = decode(&file).unwrap_or_else(|e| panic!("{label}: decode: {e}"));
            assert_eq!((back.width, back.height), (w, h), "{label}");
            assert_eq!(back.format, src.format, "{label}: layout");
            assert!(back.planes == src.planes, "{label}: planes not exact");
            count += 1;
        }
    }
    assert_eq!(count, EXTENTS.len() * 14);
}

/// Timed frames go through KEY + P groups at every size: one sync
/// sample, the rest predicted, the sample entry / `tkhd` at the
/// pictures' own size with no clean aperture, and — lossless — every
/// decoded frame equal to its source.
#[test]
fn sequences_predict_between_frames_at_every_extent() {
    for &(w, h) in EXTENTS {
        for (layout, depth, alpha) in [
            (Layout::Yuv420, 8u8, false),
            (Layout::Yuv420, 8, true),
            (Layout::Yuv444, 8, false),
            (Layout::Yuv420, 10, false),
            (Layout::Yuv444, 10, true),
            (Layout::Yuv422, 8, false),
            (Layout::Gray, 8, false),
        ] {
            let label = format!("{w}x{h} {layout:?} {depth}-bit alpha={alpha}");
            let frames: Vec<Frame> = (0..4)
                .map(|t| {
                    Frame::new(
                        source(w, h, layout, depth, alpha, t).image(),
                        Some(Duration::from_millis(40)),
                    )
                })
                .collect();
            let file = encode_all(&frames, &EncodeOptions::default())
                .unwrap_or_else(|e| panic!("{label}: encode_all: {e}"));
            let avis = parse_avis(&file).expect("parse_avis");
            assert_eq!(avis.samples.len(), 4, "{label}");
            assert_eq!(avis.display_dims, Some((w, h)), "{label}: tkhd");
            assert!(avis.samples[0].is_sync, "{label}");
            assert!(
                avis.samples[1..].iter().all(|s| !s.is_sync),
                "{label}: samples 1.. are inter-predicted"
            );
            assert!(
                !file.windows(4).any(|b| b == b"clap"),
                "{label}: no clean aperture"
            );
            let info = oxideav_avif::info(&file).expect("info");
            assert_eq!((info.width, info.height), (w, h), "{label}: info");
            let back = decode_all(&file).unwrap_or_else(|e| panic!("{label}: decode_all: {e}"));
            assert_eq!(back.len(), 4, "{label}");
            for (i, (got, want)) in back.iter().zip(&frames).enumerate() {
                assert_eq!(got.delay, want.delay, "{label} frame {i}");
                assert_eq!(got.image.format, want.image.format, "{label} frame {i}");
                assert!(
                    got.image.planes == want.image.planes,
                    "{label} frame {i}: planes not exact"
                );
            }
        }
    }
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// Off the 8-sample grid a lossy sequence is smaller as KEY + P groups
/// than as KEY frames only (the fallback such sizes used to take), at
/// a comparable quality. The content is noise-free: on white noise the
/// AV1 group coder may code a denoised picture plus film-grain
/// synthesis parameters, which no sample-wise measure describes.
#[test]
fn off_grid_lossy_sequence_is_smaller_with_inter_prediction() {
    let (w, h) = (67u32, 45u32);
    let sources: Vec<Source> = (0..8)
        .map(|t| {
            let mut src = source(w, h, Layout::Yuv420, 8, false, t);
            src.y = textured_plane(w as usize, h as usize, 8, 1, t, 0);
            src
        })
        .collect();
    let stills: Vec<StillImage> = sources.iter().map(Source::still).collect();
    let opts = SequenceEncodeOptions::default().with_base_q_idx(80);
    let inter = encode_sequence(&stills, &opts).expect("KEY + P");
    let intra = encode_sequence(&stills, &opts.with_all_intra(true)).expect("all-intra");
    assert!(
        inter.len() < intra.len(),
        "KEY + P {} B vs all-intra {} B",
        inter.len(),
        intra.len()
    );
    for (name, file) in [("KEY + P", &inter), ("all-intra", &intra)] {
        let back = decode_all(file).expect("decode_all");
        assert_eq!(back.len(), sources.len());
        for (i, (got, want)) in back.iter().zip(&sources).enumerate() {
            assert_eq!((got.image.width, got.image.height), (w, h));
            let db = psnr(&got.image.planes[0].data, &bytes_of(&want.y, 8));
            assert!(db > 30.0, "{name} frame {i}: luma {db:.2} dB");
        }
    }
}

/// Spatially layered items at any size: every layer is coded at its
/// own extents (odd, sub-sampled, no `clap`), the unpinned item
/// renders the top layer and a pinned one its layer, all exact.
#[test]
fn layered_items_code_every_layer_at_its_own_extents() {
    for (layout, depth, alpha, sizes) in [
        (
            Layout::Yuv420,
            8u8,
            false,
            &[(9u32, 7u32), (19, 13), (37, 27)][..],
        ),
        (Layout::Yuv420, 10, false, &[(17, 9), (35, 19)][..]),
        (Layout::Yuv444, 8, true, &[(10, 6), (21, 12), (42, 25)][..]),
        (Layout::Yuv422, 8, false, &[(13, 20), (27, 41)][..]),
        (Layout::Gray, 10, false, &[(5, 3), (11, 7)][..]),
        (
            Layout::Yuv420,
            8,
            true,
            &[(33, 17), (66, 34), (131, 67)][..],
        ),
    ] {
        let label = format!("{layout:?} {depth}-bit alpha={alpha} {sizes:?}");
        let sources: Vec<Source> = sizes
            .iter()
            .enumerate()
            .map(|(i, &(w, h))| source(w, h, layout, depth, alpha, i as u32))
            .collect();
        let stills: Vec<StillImage> = sources.iter().map(Source::still).collect();
        let layers: Vec<&StillImage> = stills.iter().collect();
        let file = encode_still_layered(&layers, None, &EncodeOptions::default())
            .unwrap_or_else(|e| panic!("{label}: layered encode: {e}"));
        let top = sources.last().unwrap();
        let info = inspect(&file).expect("inspect");
        assert_eq!((info.width, info.height), (top.w, top.h), "{label}: ispe");
        assert!(
            !file.windows(4).any(|b| b == b"clap"),
            "{label}: no clean aperture"
        );
        let back = decode(&file).unwrap_or_else(|e| panic!("{label}: decode: {e}"));
        assert!(
            back.planes == top.image().planes,
            "{label}: top layer not exact"
        );
        // Each layer through the decode option (alpha belongs to the
        // top layer only, so the lower ones compare their colour).
        for (i, src) in sources.iter().enumerate() {
            let got = decode_with(&file, &DecodeOptions::default().with_layer(Some(i as u16)))
                .unwrap_or_else(|e| panic!("{label}: layer {i}: {e}"));
            assert_eq!((got.width, got.height), (src.w, src.h), "{label} layer {i}");
            let want = src.image();
            let colour = if layout == Layout::Gray { 1 } else { 3 };
            assert!(
                got.planes[..colour] == want.planes[..colour],
                "{label}: layer {i} not exact"
            );
        }
        // Pinned to the base layer: `ispe` is that layer's size.
        let pinned = encode_still_layered(&layers, Some(0), &EncodeOptions::default())
            .unwrap_or_else(|e| panic!("{label}: pinned encode: {e}"));
        let info = inspect(&pinned).expect("inspect");
        assert_eq!(
            (info.width, info.height),
            (sources[0].w, sources[0].h),
            "{label}: pinned ispe"
        );
        let got = decode(&pinned).expect("pinned decode");
        let colour = if layout == Layout::Gray { 1 } else { 3 };
        assert!(
            got.planes[..colour] == sources[0].image().planes[..colour],
            "{label}: pinned base layer not exact"
        );
    }
}

// ───────────────────────── black-box readers ─────────────────────────

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `None` when `program` is not installed, else its output.
fn run(program: &str, args: &[String]) -> Option<std::process::Output> {
    match Command::new(program).args(args).output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("{program}: spawn failed: {e}"),
        Ok(out) => Some(out),
    }
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxideav-avif-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    dir
}

/// The frames of a Y4M stream: `(width, height, colour-space tag,
/// frame payloads)`.
fn parse_y4m(bytes: &[u8]) -> Option<(u32, u32, String, Vec<Vec<u8>>)> {
    let nl = bytes.iter().position(|&b| b == b'\n')?;
    let header = std::str::from_utf8(&bytes[..nl]).ok()?;
    if !header.starts_with("YUV4MPEG2") {
        return None;
    }
    let (mut w, mut h, mut tag) = (0u32, 0u32, "420".to_string());
    for tok in header.split(' ').skip(1) {
        if let Some(v) = tok.strip_prefix('W') {
            w = v.parse().ok()?;
        } else if let Some(v) = tok.strip_prefix('H') {
            h = v.parse().ok()?;
        } else if let Some(v) = tok.strip_prefix('C') {
            tag = v.to_string();
        }
    }
    let (cw, ch) = if tag.starts_with("420") {
        (w.div_ceil(2), h.div_ceil(2))
    } else if tag.starts_with("422") {
        (w.div_ceil(2), h)
    } else if tag.starts_with("444") {
        (w, h)
    } else {
        (0, 0)
    };
    let bps = if tag.contains("p10") || tag.contains("p12") {
        2
    } else {
        1
    };
    let frame_len = (w as usize * h as usize + 2 * cw as usize * ch as usize) * bps;
    let mut rest = &bytes[nl + 1..];
    let mut frames = Vec::new();
    while rest.starts_with(b"FRAME") {
        let fnl = rest.iter().position(|&b| b == b'\n')?;
        let data = rest.get(fnl + 1..fnl + 1 + frame_len)?;
        frames.push(data.to_vec());
        rest = &rest[fnl + 1 + frame_len..];
    }
    Some((w, h, tag, frames))
}

/// A source picture as one Y4M frame payload (Y, Cb, Cr; samples as
/// bytes or little-endian words).
fn y4m_payload(src: &Source) -> Vec<u8> {
    let mut out = bytes_of(&src.y, src.depth);
    out.extend(bytes_of(&src.u, src.depth));
    out.extend(bytes_of(&src.v, src.depth));
    out
}

/// The black-box AVIF decoder reproduces lossless stills of every
/// extent sample for sample — odd 4:2:0 / 4:2:2 chroma included — and
/// the other installed readers report the intended size.
#[test]
fn black_box_readers_reproduce_stills_at_every_extent() {
    let dir = tmp_dir("anyext-still");
    let mut compared = 0;
    for &(w, h) in &EXTENTS[1..] {
        for (layout, depth, alpha) in [
            (Layout::Yuv420, 8u8, false),
            (Layout::Yuv420, 10, false),
            (Layout::Yuv444, 8, false),
            (Layout::Yuv422, 8, false),
            (Layout::Yuv420, 8, true),
        ] {
            let label = format!("{w}x{h} {layout:?} {depth}-bit alpha={alpha}");
            let src = source(w, h, layout, depth, alpha, 0);
            let file = encode(&src.image(), &EncodeOptions::default()).expect("encode");
            let path = dir.join("still.avif");
            std::fs::write(&path, &file).expect("write");

            let y4m = dir.join("still.y4m");
            match run("avifdec", &[s(&path), s(&y4m)]) {
                None => eprintln!("{label}: AVIF decoder CLI not installed — leg skipped"),
                Some(out) => {
                    assert!(
                        out.status.success(),
                        "{label}: AVIF decoder CLI refused: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    let bytes = std::fs::read(&y4m).expect("read y4m");
                    let (gw, gh, tag, frames) = parse_y4m(&bytes).expect("y4m");
                    assert_eq!((gw, gh), (w, h), "{label}: size ({tag})");
                    assert_eq!(frames.len(), 1, "{label}");
                    assert!(
                        frames[0] == y4m_payload(&src),
                        "{label}: samples differ ({tag})"
                    );
                    compared += 1;
                }
            }
            for (program, args) in [
                (
                    "magick",
                    vec![
                        "identify".to_string(),
                        "-format".into(),
                        "%w %h".into(),
                        s(&path),
                    ],
                ),
                (
                    "sips",
                    vec![
                        "-g".to_string(),
                        "pixelWidth".into(),
                        "-g".into(),
                        "pixelHeight".into(),
                        s(&path),
                    ],
                ),
                (
                    "ffprobe",
                    vec![
                        "-v".to_string(),
                        "error".into(),
                        "-select_streams".into(),
                        "v:0".into(),
                        "-show_entries".into(),
                        "stream=width,height".into(),
                        "-of".into(),
                        "csv=p=0".into(),
                        s(&path),
                    ],
                ),
            ] {
                let Some(out) = run(program, &args) else {
                    continue;
                };
                assert!(
                    out.status.success(),
                    "{label}: {program} refused: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let text = String::from_utf8_lossy(&out.stdout);
                let numbers: Vec<u32> = text
                    .split(|c: char| !c.is_ascii_digit())
                    .filter_map(|t| t.parse().ok())
                    .collect();
                assert!(
                    numbers.windows(2).any(|p| p == [w, h]),
                    "{label}: {program} reports {text:?}"
                );
            }
        }
    }
    eprintln!("any-extent stills: {compared} files compared sample for sample");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A black-box media framework demuxes the picture track of off-grid
/// KEY + P sequences at the intended size with square pixels and
/// decodes every frame exactly.
#[test]
fn black_box_reader_presents_off_grid_sequences_at_their_size() {
    let dir = tmp_dir("anyext-seq");
    for &(w, h) in &[(17u32, 13u32), (34, 18), (36, 20), (67, 35), (40, 21)] {
        for (layout, depth, pix_fmt) in [
            (Layout::Yuv420, 8u8, "yuv420p"),
            (Layout::Yuv444, 8, "yuv444p"),
            (Layout::Yuv420, 10, "yuv420p10le"),
        ] {
            let label = format!("{w}x{h} {layout:?} {depth}-bit");
            let sources: Vec<Source> = (0..5)
                .map(|t| source(w, h, layout, depth, false, t))
                .collect();
            let stills: Vec<StillImage> = sources.iter().map(Source::still).collect();
            let file = encode_sequence(&stills, &SequenceEncodeOptions::default()).expect("encode");
            let path = dir.join("seq.avif");
            std::fs::write(&path, &file).expect("write");
            // The file lists the cover still and the track; the track
            // is the stream with five frames.
            let Some(probe) = run(
                "ffprobe",
                &[
                    "-v".to_string(),
                    "error".into(),
                    "-count_packets".into(),
                    "-select_streams".into(),
                    "v".into(),
                    "-show_entries".into(),
                    "stream=index,width,height,sample_aspect_ratio,nb_read_packets".into(),
                    "-of".into(),
                    "csv=p=0".into(),
                    s(&path),
                ],
            ) else {
                eprintln!("{label}: media framework CLI not installed — leg skipped");
                continue;
            };
            assert!(probe.status.success(), "{label}: probe refused");
            let text = String::from_utf8_lossy(&probe.stdout).into_owned();
            let track = text
                .lines()
                .map(|l| l.trim().split(',').collect::<Vec<_>>())
                .find(|f| f.last() == Some(&"5"))
                .unwrap_or_else(|| panic!("{label}: no five-frame stream in {text:?}"));
            assert_eq!(
                (track[1], track[2]),
                (w.to_string().as_str(), h.to_string().as_str()),
                "{label}: presented size ({text:?})"
            );
            assert!(
                matches!(track[3], "1:1" | "N/A" | "0:1"),
                "{label}: sample aspect ratio {:?}",
                track[3]
            );
            let raw = dir.join("seq.raw");
            let out = run(
                "ffmpeg",
                &[
                    "-hide_banner".to_string(),
                    "-loglevel".into(),
                    "error".into(),
                    "-y".into(),
                    "-i".into(),
                    s(&path),
                    "-map".into(),
                    format!("0:{}", track[0]),
                    "-f".into(),
                    "rawvideo".into(),
                    "-pix_fmt".into(),
                    pix_fmt.into(),
                    s(&raw),
                ],
            )
            .expect("installed next to its probe");
            assert!(
                out.status.success(),
                "{label}: refused: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let got = std::fs::read(&raw).expect("read raw");
            let want: Vec<u8> = sources.iter().flat_map(y4m_payload).collect();
            assert_eq!(got.len(), want.len(), "{label}: frame bytes");
            assert!(got == want, "{label}: frames differ");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
