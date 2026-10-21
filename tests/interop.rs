//! Black-box interop: every shape the registry encoder writes — the
//! declared layouts at the default and lossless qualities, alpha,
//! 10 / 12-bit, planar RGB, metadata (ICC / Exif / XMP), the automatic
//! grid with and without alpha — is handed to the AVIF readers
//! installed on the host (`sips`, `magick identify`, `ffmpeg`,
//! `heif-info`, `avifdec`), each as an opaque process. A reader that
//! is not installed is skipped (and said so); one that is installed
//! must accept every file and, where it prints them, report the
//! picture's extents. The table of results is printed per file.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{
    BlobKind, CodecId, CodecOptions, CodecParameters, ExecutionContext, Frame, MetadataBlob,
    PixelFormat, RuntimeContext,
};

const AVIF: &str = oxideav_avif::CODEC_ID_STR;

fn registry() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_avif::register(&mut ctx);
    ctx
}

/// Plane geometry of a declared layout: `(subsampling per plane,
/// bytes per sample, channels per sample)`.
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
        other => panic!("undeclared layout {other:?}"),
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

/// A textured picture (gradient + weave; alpha mostly opaque with a
/// soft edge) in `fmt`.
fn frame(fmt: PixelFormat, w: u32, h: u32) -> VideoFrame {
    let (planes, bps, channels) = geometry(fmt);
    let depth = depth_of(fmt);
    let mut out = Vec::new();
    for (pi, &(sx, sy)) in planes.iter().enumerate() {
        let (pw, ph) = ((w >> sx) as usize, (h >> sy) as usize);
        let stride = pw * channels * bps;
        let mut data = Vec::with_capacity(stride * ph);
        for y in 0..ph {
            for x in 0..pw {
                for c in 0..channels {
                    let g = ((x * 255) / pw.max(1)) as i32;
                    let weave = (((x * 7 + y * 5 + (pi + c) * 40) % 61) as i32) * 2 - 60;
                    let mut v8 = (g + weave + (y as i32 * 3) % 50).clamp(0, 255) as u8;
                    let is_alpha = (channels == 4 && c == 3)
                        || (channels == 2 && c == 1)
                        || (channels == 1 && pi == 3);
                    if is_alpha {
                        v8 = 255 - v8 / 3;
                    }
                    if bps == 1 {
                        data.push(v8);
                    } else {
                        let v = (u32::from(v8) << (depth - 8)) | (u32::from(v8) >> (16 - depth));
                        data.extend_from_slice(&(v as u16).to_le_bytes());
                    }
                }
            }
        }
        out.push(VideoPlane { stride, data });
    }
    VideoFrame {
        pts: Some(0),
        planes: out,
    }
}

fn exif_block() -> Vec<u8> {
    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v
}

struct Case {
    name: &'static str,
    fmt: PixelFormat,
    w: u32,
    h: u32,
    options: Vec<(&'static str, &'static str)>,
    blobs: bool,
}

fn write_case(ctx: &RuntimeContext, dir: &Path, case: &Case) -> (PathBuf, usize) {
    let mut p = CodecParameters::video(CodecId::new(AVIF));
    p.width = Some(case.w);
    p.height = Some(case.h);
    p.pixel_format = Some(case.fmt);
    let mut o = CodecOptions::new();
    for (k, v) in &case.options {
        o = o.set(*k, *v);
    }
    p.options = o;
    let mut vf = frame(case.fmt, case.w, case.h);
    if case.blobs {
        p.blobs
            .push(MetadataBlob::new(BlobKind::ICC, b"stream profile".to_vec()));
        vf.set_blobs(vec![
            MetadataBlob::new(BlobKind::ICC, b"frame profile".to_vec()),
            MetadataBlob::new(BlobKind::EXIF, exif_block()),
            MetadataBlob::new(
                BlobKind::XMP,
                b"<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?><x:xmpmeta xmlns:x='adobe:ns:meta/'/><?xpacket end='w'?>".to_vec(),
            ),
        ]);
    }
    let mut enc = ctx.codecs.first_encoder(&p).expect("encoder");
    enc.set_execution_context(&ExecutionContext::with_threads(4));
    enc.send_frame(&Frame::Video(vf)).expect("send_frame");
    let bytes = enc.receive_packet().expect("packet").data;
    let path = dir.join(format!("{}.avif", case.name));
    std::fs::write(&path, &bytes).expect("write");
    (path, bytes.len())
}

/// One black-box reader: how it is run and how its answer is read.
struct Reader {
    name: &'static str,
    program: &'static str,
    args: fn(&Path) -> Vec<String>,
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

const READERS: &[Reader] = &[
    Reader {
        name: "sips",
        program: "sips",
        args: |p| {
            vec![
                "-g".into(),
                "pixelWidth".into(),
                "-g".into(),
                "pixelHeight".into(),
                s(p),
            ]
        },
    },
    Reader {
        name: "magick identify",
        program: "magick",
        args: |p| vec!["identify".into(), s(p)],
    },
    Reader {
        name: "ffmpeg",
        program: "ffmpeg",
        args: |p| {
            vec![
                "-v".into(),
                "error".into(),
                "-i".into(),
                s(p),
                "-frames:v".into(),
                "1".into(),
                "-f".into(),
                "null".into(),
                "-".into(),
            ]
        },
    },
    Reader {
        name: "heif-info",
        program: "heif-info",
        args: |p| vec![s(p)],
    },
    Reader {
        name: "avifdec",
        program: "avifdec",
        args: |p| vec!["--info".into(), s(p)],
    },
];

/// Reader gaps that are not this crate's: `(reader, case)` pairs the
/// reader is known to refuse although the file is conformant. `sips`
/// (ImageIO) yields no picture for a file carrying the `prem` item
/// reference — HEIF §6.9.1's "from the master image item to the
/// auxiliary image item" reference that marks pre-multiplied alpha
/// (a `mif2` reader requirement, §10.2.3.2); the file decodes on
/// every other reader, with the alpha.
const READER_GAPS: &[(&str, &str)] = &[("sips", "bgra_premultiplied")];

/// `Ok(stdout + stderr)` when the reader accepted the file, `Err(..)`
/// with the diagnostic when it did not, `None` when it is not
/// installed.
fn run(reader: &Reader, path: &Path) -> Option<Result<String, String>> {
    match Command::new(reader.program)
        .args((reader.args)(path))
        .output()
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(Err(format!("spawn failed: {e}"))),
        Ok(out) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let failed = !out.status.success()
                || text.contains("error")
                || text.contains("Error")
                || text.contains("Could not")
                || text.contains("Failed");
            Some(if failed { Err(text) } else { Ok(text) })
        }
    }
}

#[test]
fn every_written_shape_is_accepted_by_the_installed_readers() {
    let ctx = registry();
    let dir = std::env::temp_dir().join(format!("oxideav-avif-interop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let (w, h) = (64u32, 48u32);
    let cases = vec![
        Case {
            name: "yuv420_default",
            fmt: PixelFormat::Yuv420P,
            w,
            h,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "yuv420_lossless",
            fmt: PixelFormat::Yuv420P,
            w,
            h,
            options: vec![("quality", "100")],
            blobs: false,
        },
        Case {
            name: "yuvj444",
            fmt: PixelFormat::YuvJ444P,
            w,
            h,
            options: vec![("quality", "80")],
            blobs: false,
        },
        Case {
            name: "yuv422",
            fmt: PixelFormat::Yuv422P,
            w,
            h,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "gray8",
            fmt: PixelFormat::Gray8,
            w,
            h,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "yuv420p10",
            fmt: PixelFormat::Yuv420P10Le,
            w,
            h,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "yuv444p12",
            fmt: PixelFormat::Yuv444P12Le,
            w,
            h,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "gray12",
            fmt: PixelFormat::Gray12Le,
            w,
            h,
            options: vec![("quality", "100")],
            blobs: false,
        },
        Case {
            name: "rgb24_lossy",
            fmt: PixelFormat::Rgb24,
            w,
            h,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "rgb24_444",
            fmt: PixelFormat::Rgb24,
            w,
            h,
            options: vec![("chroma", "444")],
            blobs: false,
        },
        Case {
            name: "bgr24_identity",
            fmt: PixelFormat::Bgr24,
            w,
            h,
            options: vec![("quality", "100")],
            blobs: false,
        },
        Case {
            name: "rgba_alpha",
            fmt: PixelFormat::Rgba,
            w,
            h,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "bgra_premultiplied",
            fmt: PixelFormat::Bgra,
            w,
            h,
            options: vec![("premultiplied", "true")],
            blobs: false,
        },
        Case {
            name: "gbrp8",
            fmt: PixelFormat::Gbrp8,
            w,
            h,
            options: vec![("quality", "90")],
            blobs: false,
        },
        Case {
            name: "gbrap10",
            fmt: PixelFormat::Gbrap10Le,
            w,
            h,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "yuva420",
            fmt: PixelFormat::Yuva420P,
            w,
            h,
            options: vec![("quality", "100")],
            blobs: false,
        },
        Case {
            name: "ya8",
            fmt: PixelFormat::Ya8,
            w,
            h,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "metadata",
            fmt: PixelFormat::Yuv420P,
            w,
            h,
            options: vec![],
            blobs: true,
        },
        Case {
            name: "grid_auto",
            fmt: PixelFormat::Yuv420P,
            w: 1200,
            h: 900,
            options: vec![],
            blobs: true,
        },
        Case {
            name: "grid_alpha",
            fmt: PixelFormat::Rgba,
            w: 1200,
            h: 900,
            options: vec![],
            blobs: false,
        },
        Case {
            name: "grid_256",
            fmt: PixelFormat::Yuv420P,
            w: 1200,
            h: 900,
            options: vec![("grid", "256")],
            blobs: false,
        },
    ];
    let mut failures = Vec::new();
    let mut installed: Vec<&str> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for case in &cases {
        let (path, len) = write_case(&ctx, &dir, case);
        let mut row = format!("| {} ({}x{}, {} B) |", case.name, case.w, case.h, len);
        for reader in READERS {
            match run(reader, &path) {
                None => {
                    if !missing.contains(&reader.name) {
                        missing.push(reader.name);
                    }
                    row.push_str(" — |");
                }
                Some(Ok(text)) => {
                    if !installed.contains(&reader.name) {
                        installed.push(reader.name);
                    }
                    let dims = format!("{}x{}", case.w, case.h);
                    let saw = text.contains(&dims)
                        || (text.contains(&format!("pixelWidth: {}", case.w))
                            && text.contains(&format!("pixelHeight: {}", case.h)))
                        || reader.name == "ffmpeg";
                    let gap = READER_GAPS.contains(&(reader.name, case.name));
                    if !saw && !gap {
                        failures.push(format!(
                            "{}: {} accepted the file but did not report {dims}:\n{text}",
                            case.name, reader.name
                        ));
                    }
                    row.push_str(if saw {
                        " opens |"
                    } else {
                        " no picture (reader gap) |"
                    });
                }
                Some(Err(text)) => {
                    if READER_GAPS.contains(&(reader.name, case.name)) {
                        row.push_str(" refused (reader gap) |");
                    } else {
                        failures.push(format!("{}: {} refused:\n{text}", case.name, reader.name));
                        row.push_str(" REFUSED |");
                    }
                }
            }
        }
        println!("{row}");
    }
    println!("readers installed: {installed:?}; not installed (skipped): {missing:?}");
    if failures.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("files kept under {}", dir.display());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
