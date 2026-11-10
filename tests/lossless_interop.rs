//! Black-box proof of the exact round trip: lossless files written by
//! this crate are decoded by the AVIF readers installed on the host
//! (each an opaque process) and the pixels they produce are compared
//! with the source, sample for sample — RGB and RGBA at 8 bits over
//! even, odd, padded and grid-tiled extents, 10 / 12-bit RGB and
//! monochrome. A reader that is not installed is skipped (and said
//! so); an installed one must reproduce every sample it outputs.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_avif::{
    AvifImage, AvifPixelFormat, AvifPlane, ColorInfo, EncodeOptions, encode, encode_rgb8,
    encode_rgba8,
};

fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect()
}

/// Ramps, saturated samples and noise in the low bits.
fn textured(w: u32, h: u32, channels: usize, seed: u32) -> Vec<u8> {
    let n = noise(w as usize * h as usize * channels, seed);
    let mut out = Vec::with_capacity(n.len());
    for y in 0..h as usize {
        for x in 0..w as usize {
            for c in 0..channels {
                let i = (y * w as usize + x) * channels + c;
                let ramp = (x * 255 / (w as usize).max(1) + y * (c + 1) * 3) & 0xff;
                out.push(match (x + y * 3 + c) % 11 {
                    0 => 0,
                    1 => 255,
                    _ => (ramp as u8) ^ (n[i] & 0x0f),
                });
            }
        }
    }
    out
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Run `program args`; `None` when the program is not installed,
/// `Some(false)` when it failed.
fn run(program: &str, args: &[String]) -> Option<bool> {
    match Command::new(program).args(args).output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(false),
        Ok(out) => Some(out.status.success()),
    }
}

fn installed(program: &str) -> bool {
    Command::new(program)
        .arg("-version")
        .output()
        .map(|_| true)
        .unwrap_or(false)
}

/// The readers that only write PNG need one of the raw converters.
fn raw_converter_installed() -> bool {
    installed("magick") || installed("ffmpeg")
}

/// A PNG written by a reader, as tightly packed samples in `layout`
/// (8-bit RGBA / gray, or 16-bit little-endian RGB words) — converted
/// by whichever raw converter is installed.
fn png_to_raw(png: &Path, raw: &Path, layout: Layout) -> Option<Vec<u8>> {
    let ok = if installed("magick") {
        let mut args = vec![s(png)];
        match layout {
            Layout::RgbDeep(_) => {
                args.extend(["-depth".into(), "16".into(), "-endian".into(), "LSB".into()]);
                args.push(format!("rgb:{}", s(raw)));
            }
            Layout::Rgba8 => {
                args.extend(["-depth".into(), "8".into()]);
                args.push(format!("rgba:{}", s(raw)));
            }
            Layout::Gray8 => {
                args.extend(["-depth".into(), "8".into()]);
                args.push(format!("gray:{}", s(raw)));
            }
        }
        run("magick", &args)?
    } else {
        run(
            "ffmpeg",
            &[
                "-v".into(),
                "error".into(),
                "-y".into(),
                "-i".into(),
                s(png),
                "-f".into(),
                "rawvideo".into(),
                "-pix_fmt".into(),
                match layout {
                    Layout::RgbDeep(_) => "rgb48le",
                    Layout::Rgba8 => "rgba",
                    Layout::Gray8 => "gray",
                }
                .into(),
                s(raw),
            ],
        )?
    };
    ok.then(|| std::fs::read(raw).ok()).flatten()
}

/// What a reader produced for one file.
enum Seen {
    /// Not installed (or no raw converter for its output).
    Skipped,
    /// The process refused the file.
    Refused,
    /// Samples in the layout the case asked for.
    Samples(Vec<u8>),
}

/// The layout a case compares in.
#[derive(Clone, Copy, PartialEq)]
enum Layout {
    /// 8-bit RGBA (opaque sources read back with alpha 255).
    Rgba8,
    /// 8-bit gray.
    Gray8,
    /// `depth`-bit RGB as little-endian words.
    RgbDeep(u8),
}

fn read_with(reader: &str, file: &Path, dir: &Path, layout: Layout) -> Seen {
    let stem = file.file_stem().unwrap().to_string_lossy().into_owned();
    let raw = dir.join(format!("{stem}.{reader}.raw"));
    let png = dir.join(format!("{stem}.{reader}.png"));
    let deep = matches!(layout, Layout::RgbDeep(_));
    let out: Option<Option<Vec<u8>>> = match reader {
        "magick" => {
            let mut args = vec![s(file)];
            match layout {
                Layout::Rgba8 => {
                    args.extend(["-depth".into(), "8".into()]);
                    args.push(format!("rgba:{}", s(&raw)));
                }
                Layout::Gray8 => {
                    args.extend(["-depth".into(), "8".into()]);
                    args.push(format!("gray:{}", s(&raw)));
                }
                Layout::RgbDeep(_) => {
                    args.extend(["-depth".into(), "16".into(), "-endian".into(), "LSB".into()]);
                    args.push(format!("rgb:{}", s(&raw)));
                }
            }
            run("magick", &args).map(|ok| ok.then(|| std::fs::read(&raw).ok()).flatten())
        }
        "ffmpeg" => {
            // The colour item only (the alpha auxiliary is a separate
            // stream to this reader): opaque sources and the colour
            // channels of the others.
            let pix = match layout {
                Layout::Rgba8 => "rgba".to_string(),
                Layout::Gray8 => "gray".to_string(),
                Layout::RgbDeep(d) => format!("gbrp{d}le"),
            };
            run(
                "ffmpeg",
                &[
                    "-v".into(),
                    "error".into(),
                    "-y".into(),
                    "-i".into(),
                    s(file),
                    "-frames:v".into(),
                    "1".into(),
                    "-f".into(),
                    "rawvideo".into(),
                    "-pix_fmt".into(),
                    pix,
                    s(&raw),
                ],
            )
            .map(|ok| ok.then(|| std::fs::read(&raw).ok()).flatten())
            .map(|v| {
                v.map(|bytes| match layout {
                    // Planar G, B, R words → interleaved R, G, B.
                    Layout::RgbDeep(_) => {
                        let n = bytes.len() / 6;
                        let mut out = Vec::with_capacity(bytes.len());
                        for i in 0..n {
                            for plane in [2, 0, 1] {
                                let at = (plane * n + i) * 2;
                                out.extend_from_slice(&bytes[at..at + 2]);
                            }
                        }
                        out
                    }
                    _ => bytes,
                })
            })
        }
        "avifdec" => {
            if !raw_converter_installed() {
                return Seen::Skipped;
            }
            let mut args = Vec::new();
            if deep {
                args.extend(["-d".into(), "16".into()]);
            }
            args.extend([s(file), s(&png)]);
            run("avifdec", &args).map(|ok| ok.then(|| png_to_raw(&png, &raw, layout)).flatten())
        }
        "sips" => {
            if layout != Layout::Rgba8 || !raw_converter_installed() {
                return Seen::Skipped;
            }
            run(
                "sips",
                &[
                    "-s".into(),
                    "format".into(),
                    "png".into(),
                    s(file),
                    "--out".into(),
                    s(&png),
                ],
            )
            .map(|ok| ok.then(|| png_to_raw(&png, &raw, layout)).flatten())
        }
        other => panic!("unknown reader {other}"),
    };
    match out {
        None => Seen::Skipped,
        Some(None) => Seen::Refused,
        Some(Some(bytes)) => Seen::Samples(bytes),
    }
}

struct Case {
    name: String,
    file: Vec<u8>,
    layout: Layout,
    /// The source in the case's layout (8-bit RGBA / gray, or
    /// `depth`-bit RGB words).
    expect: Vec<u8>,
    /// The picture before `irot` / `imir`, for a reader whose
    /// converter leaves the orientation to the presentation layer.
    unoriented: Option<Vec<u8>>,
}

/// `depth`-bit identity-matrix RGB planes (G, B, R order in the file)
/// from interleaved words.
fn deep_rgb_image(w: u32, h: u32, depth: u8, rgb: &[u16]) -> AvifImage {
    let n = w as usize * h as usize;
    let plane = |c: usize| {
        let mut data = Vec::with_capacity(n * 2);
        for px in rgb.as_chunks::<3>().0 {
            data.extend_from_slice(&px[c].to_le_bytes());
        }
        AvifPlane::new(w as usize * 2, data)
    };
    let format = if depth == 10 {
        AvifPixelFormat::Yuv444P10Le
    } else {
        AvifPixelFormat::Yuv444P12Le
    };
    AvifImage::new(w, h, format, vec![plane(1), plane(2), plane(0)])
        .unwrap()
        .with_color(ColorInfo::identity_full_range())
}

fn cases() -> Vec<Case> {
    let lossless = EncodeOptions::default().with_lossless(true).with_threads(4);
    let mut out = Vec::new();
    for &(w, h) in &[(16u32, 16u32), (17, 13), (64, 48), (301, 203), (1200, 900)] {
        let n = w as usize * h as usize;
        let rgb = if n < 100_000 {
            noise(n * 3, w + h)
        } else {
            textured(w, h, 3, w + h)
        };
        let mut expect = Vec::with_capacity(n * 4);
        for px in rgb.as_chunks::<3>().0 {
            expect.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
        out.push(Case {
            name: format!("rgb8_{w}x{h}"),
            file: encode_rgb8(w, h, &rgb, &lossless).unwrap(),
            layout: Layout::Rgba8,
            expect,
            unoriented: None,
        });
        let rgba = if n < 100_000 {
            noise(n * 4, w * 3 + h)
        } else {
            textured(w, h, 4, w * 3 + h)
        };
        out.push(Case {
            name: format!("rgba8_{w}x{h}"),
            file: encode_rgba8(w, h, &rgba, &lossless).unwrap(),
            layout: Layout::Rgba8,
            expect: rgba,
            unoriented: None,
        });
    }
    // Orientation on a padded, alpha-carrying picture: the auxiliary
    // carries the colour item's `clap` / `irot` / `imir`. The expected
    // pixels are this crate's own decode (whose colour / alpha
    // co-location `tests/lossless_roundtrip.rs` checks), so the readers
    // prove the orientation and the auxiliary's geometry.
    for (irot, imir) in [(Some(1u8), None), (Some(3), Some(0u8)), (None, Some(1))] {
        let (w, h) = (21u32, 10u32);
        let rgba = noise(w as usize * h as usize * 4, 21);
        let still = oxideav_avif::StillImage::rgba8(w, h, &rgba)
            .unwrap()
            .with_props(
                oxideav_avif::StillProperties::default()
                    .with_irot(irot)
                    .with_imir(imir),
            );
        let file = oxideav_avif::encode_still(&still, &lossless).unwrap();
        let expect = oxideav_avif::decode_rgba8(&file).unwrap().into_raw();
        out.push(Case {
            name: format!(
                "rgba8_{w}x{h}_irot{}_imir{}",
                irot.map_or("-".into(), |v| v.to_string()),
                imir.map_or("-".into(), |v| v.to_string())
            ),
            file,
            layout: Layout::Rgba8,
            expect,
            unoriented: Some(rgba),
        });
    }
    for depth in [10u8, 12] {
        let (w, h) = (33u32, 17u32);
        let words: Vec<u16> = noise(w as usize * h as usize * 6, u32::from(depth))
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b) & ((1 << depth) - 1))
            .collect();
        let img = deep_rgb_image(w, h, depth, &words);
        out.push(Case {
            name: format!("rgb{depth}_{w}x{h}"),
            file: encode(&img, &lossless).unwrap(),
            layout: Layout::RgbDeep(depth),
            expect: words.iter().flat_map(|v| v.to_le_bytes()).collect(),
            unoriented: None,
        });
    }
    {
        let (w, h) = (33u32, 17u32);
        let gray = noise(w as usize * h as usize, 4);
        let img = AvifImage::new(
            w,
            h,
            AvifPixelFormat::Gray8,
            vec![AvifPlane::new(w as usize, gray.clone())],
        )
        .unwrap();
        out.push(Case {
            name: format!("gray8_{w}x{h}"),
            file: encode(&img, &lossless).unwrap(),
            layout: Layout::Gray8,
            expect: gray,
            unoriented: None,
        });
    }
    out
}

/// Bring what a reader produced to the source's scale and compare.
/// `None` = identical; `Some(description)` otherwise.
fn mismatch(reader: &str, case: &Case, seen: &[u8]) -> Option<String> {
    // `sips` converts the stored picture and leaves `irot` / `imir` to
    // whoever presents it: it must reproduce the un-oriented source
    // (colour and alpha alike).
    let source = match (&case.unoriented, reader) {
        (Some(plain), "sips") => plain,
        _ => &case.expect,
    };
    let (expect, seen): (Vec<u16>, Vec<u16>) = match case.layout {
        Layout::Rgba8 | Layout::Gray8 => {
            let mut e: Vec<u16> = source.iter().map(|&b| u16::from(b)).collect();
            let mut g: Vec<u16> = seen.iter().map(|&b| u16::from(b)).collect();
            if reader == "ffmpeg" && case.layout == Layout::Rgba8 {
                // Colour item only: compare R, G, B.
                for v in [&mut e, &mut g] {
                    for px in v.as_chunks_mut::<4>().0 {
                        px[3] = 255;
                    }
                }
            }
            (e, g)
        }
        Layout::RgbDeep(depth) => {
            let e: Vec<u16> = case
                .expect
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect();
            let g: Vec<u16> = seen
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| {
                    let v = u16::from_le_bytes(*b);
                    if reader == "ffmpeg" {
                        v // native-depth planes
                    } else {
                        // 16-bit output of a `depth`-bit sample: its
                        // top `depth` bits are the coded value under a
                        // shift, a bit replication and a rounded
                        // rescale alike.
                        v >> (16 - depth)
                    }
                })
                .collect();
            (e, g)
        }
    };
    if expect.len() != seen.len() {
        return Some(format!(
            "{} samples where the source has {}",
            seen.len(),
            expect.len()
        ));
    }
    let differing = expect.iter().zip(&seen).filter(|(a, b)| a != b).count();
    (differing > 0).then(|| format!("{differing} of {} samples differ", expect.len()))
}

#[test]
fn installed_readers_reproduce_lossless_files_sample_for_sample() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("oxideav-avif-lossless-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let readers = ["avifdec", "ffmpeg", "magick", "sips"];
    let mut failures = Vec::new();
    let mut exact = std::collections::BTreeMap::<&str, usize>::new();
    println!("| file | {} |", readers.join(" | "));
    for case in cases() {
        let path = dir.join(format!("{}.avif", case.name));
        std::fs::write(&path, &case.file).expect("write");
        let mut row = format!("| {} ({} B) |", case.name, case.file.len());
        for reader in readers {
            match read_with(reader, &path, &dir, case.layout) {
                Seen::Skipped => row.push_str(" — |"),
                Seen::Refused => {
                    failures.push(format!("{}: {reader} refused the file", case.name));
                    row.push_str(" REFUSED |");
                }
                Seen::Samples(bytes) => match mismatch(reader, &case, &bytes) {
                    None => {
                        *exact.entry(reader).or_default() += 1;
                        row.push_str(" exact |");
                    }
                    Some(why) => {
                        failures.push(format!("{}: {reader}: {why}", case.name));
                        row.push_str(" DIFFERS |");
                    }
                },
            }
        }
        println!("{row}");
    }
    println!("exact files per reader: {exact:?} (absent = not installed, skipped)");
    if failures.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("files kept under {}", dir.display());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The other direction: an identity-matrix (G, B, R planes) lossless
/// file written by a black-box producer from a PNG decodes through
/// this crate to exactly the PNG's RGB(A) samples. Skipped when the
/// producer or the raw → PNG converter is not installed.
#[test]
fn identity_matrix_files_from_a_black_box_producer_decode_exactly() {
    if !installed("magick") || run("avifenc", &["--version".into()]).is_none() {
        println!("skipped: needs `magick` (raw → PNG) and `avifenc` (PNG → lossless AVIF)");
        return;
    }
    let dir: PathBuf =
        std::env::temp_dir().join(format!("oxideav-avif-identity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let mut checked = 0;
    for &(w, h) in &[(16u32, 16u32), (17, 13), (301, 203)] {
        for channels in [3usize, 4] {
            let source = noise(w as usize * h as usize * channels, w * 5 + channels as u32);
            let kind = if channels == 3 { "rgb" } else { "rgba" };
            let raw = dir.join(format!("{kind}_{w}x{h}.raw"));
            let png = dir.join(format!("{kind}_{w}x{h}.png"));
            let avif = dir.join(format!("{kind}_{w}x{h}.avif"));
            std::fs::write(&raw, &source).unwrap();
            assert_eq!(
                run(
                    "magick",
                    &[
                        "-size".into(),
                        format!("{w}x{h}"),
                        "-depth".into(),
                        "8".into(),
                        format!("{kind}:{}", s(&raw)),
                        s(&png),
                    ],
                ),
                Some(true),
                "raw → PNG"
            );
            assert_eq!(
                run("avifenc", &["--lossless".into(), s(&png), s(&avif)]),
                Some(true),
                "PNG → lossless AVIF"
            );
            let file = std::fs::read(&avif).unwrap();
            let image = oxideav_avif::decode(&file).unwrap();
            assert!(
                image.color.is_identity_matrix(),
                "{kind} {w}x{h}: the producer's lossless file is identity-matrix"
            );
            let back = if channels == 3 {
                oxideav_avif::decode_rgb8(&file).unwrap().into_raw()
            } else {
                oxideav_avif::decode_rgba8(&file).unwrap().into_raw()
            };
            assert!(back == source, "{kind} {w}x{h}: not exact");
            checked += 1;
        }
    }
    println!("{checked} black-box lossless files decoded exactly");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A lossless RGBA image sequence (picture track + alpha auxiliary
/// track, padded odd extents): every frame the black-box readers
/// output equals the source — colour and alpha through the reader
/// that composes the alpha track, colour through the one that reads
/// the picture track.
#[test]
fn installed_readers_reproduce_a_lossless_rgba_sequence() {
    let (w, h, count) = (17u32, 13u32, 4u32);
    let frames: Vec<Vec<u8>> = (0..count)
        .map(|t| noise(w as usize * h as usize * 4, 500 + t))
        .collect();
    let timed: Vec<oxideav_avif::Frame> = frames
        .iter()
        .map(|rgba| {
            oxideav_avif::Frame::new(
                AvifImage::from_rgba8(w, h, rgba.clone()).unwrap(),
                Some(std::time::Duration::from_millis(100)),
            )
        })
        .collect();
    let file =
        oxideav_avif::encode_all(&timed, &EncodeOptions::default().with_lossless(true)).unwrap();
    let dir: PathBuf =
        std::env::temp_dir().join(format!("oxideav-avif-lossless-seq-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let path = dir.join("rgba_sequence.avif");
    std::fs::write(&path, &file).unwrap();
    let mut failures = Vec::new();
    let mut seen = Vec::new();

    // Frame by frame, alpha composed from the auxiliary track.
    for (i, want) in frames.iter().enumerate() {
        if !raw_converter_installed() {
            break;
        }
        let png = dir.join(format!("frame{i}.png"));
        let raw = dir.join(format!("frame{i}.raw"));
        match run(
            "avifdec",
            &["--index".into(), i.to_string(), s(&path), s(&png)],
        ) {
            None => break,
            Some(false) => failures.push(format!("avifdec refused frame {i}")),
            Some(true) => match png_to_raw(&png, &raw, Layout::Rgba8) {
                None => break,
                Some(got) => {
                    if &got != want {
                        failures.push(format!("avifdec frame {i} differs"));
                    }
                    if i == 0 {
                        seen.push("avifdec");
                    }
                }
            },
        }
    }
    // Every picture-track sample in one pass (colour only).
    let raw = dir.join("all.raw");
    match run(
        "ffmpeg",
        &[
            "-v".into(),
            "error".into(),
            "-y".into(),
            "-i".into(),
            s(&path),
            // Stream 0 is the cover image item; 1 the picture track.
            "-map".into(),
            "0:1".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "rgb24".into(),
            s(&raw),
        ],
    ) {
        None => {}
        Some(false) => failures.push("ffmpeg refused the sequence".into()),
        Some(true) => {
            let got = std::fs::read(&raw).unwrap();
            let want: Vec<u8> = frames
                .iter()
                .flat_map(|f| f.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]))
                .collect();
            if got != want {
                failures.push(format!(
                    "ffmpeg: {} bytes of colour, {} expected, or different samples",
                    got.len(),
                    want.len()
                ));
            }
            seen.push("ffmpeg");
        }
    }
    println!("lossless RGBA sequence reproduced exactly by: {seen:?}");
    if failures.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("files kept under {}", dir.display());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
