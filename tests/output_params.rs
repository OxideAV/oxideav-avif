//! The decoder's stream-level output description under the
//! `oxideav-core` 0.1.42 contract: `Decoder::output_params()` describes
//! the frame most recently returned by `receive_frame` (before the
//! first one: the next frame), `Decoder::output_params_generation()`
//! moves exactly at the frame boundary where that description changes
//! and never otherwise (across `reset` too), and `OutputParamsWatch`
//! sees one change per shape switch. Each switch is announced in-band
//! too (`oxideav-core` 0.1.43): one `Frame::StreamParams` right before
//! the first frame of the new shape, payload == `output_params()`, in
//! 1:1 agreement with the counter (`observe` + `observe_frame` on one
//! watch report it once). Fixtures are built with this
//! crate's own encoders (stills through the registry encoder, `avis`
//! sequences through `encode_sequence`, spliced into one track with
//! the container's sequence writer for the mid-stream size change).

#![cfg(feature = "registry")]

use oxideav_avif::{
    AvifDecoder, SequenceEncodeOptions, StillChroma, StillImage, encode_sequence, stream_params,
};
use oxideav_core::frame::{VideoFrame, VideoPlane};
use oxideav_core::{
    CodecId, CodecParameters, Decoder, Frame, OutputParamsWatch, Packet, PixelFormat, Rational,
    RuntimeContext, TimeBase,
};

const AVIF: &str = oxideav_avif::CODEC_ID_STR;

fn registry() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_avif::register(&mut ctx);
    ctx
}

fn texel(x: usize, y: usize, c: usize) -> u8 {
    ((x * 7 + y * 13 + c * 41) % 200 + 20) as u8
}

/// A textured 8-bit frame in `fmt` (`Yuv420P`, `Yuva420P`, `Gray8`,
/// `Yuv444P`).
fn frame(fmt: PixelFormat, w: u32, h: u32) -> Frame {
    let shifts: &[(u32, u32)] = match fmt {
        PixelFormat::Yuv420P => &[(0, 0), (1, 1), (1, 1)],
        PixelFormat::Yuva420P => &[(0, 0), (1, 1), (1, 1), (0, 0)],
        PixelFormat::Yuv444P => &[(0, 0), (0, 0), (0, 0)],
        PixelFormat::Gray8 => &[(0, 0)],
        other => panic!("no test geometry for {other:?}"),
    };
    let planes = shifts
        .iter()
        .enumerate()
        .map(|(c, (sx, sy))| {
            let (pw, ph) = ((w >> sx) as usize, (h >> sy) as usize);
            let mut data = Vec::with_capacity(pw * ph);
            for y in 0..ph {
                for x in 0..pw {
                    data.push(if c == 3 { 255 } else { texel(x, y, c) });
                }
            }
            VideoPlane { stride: pw, data }
        })
        .collect();
    Frame::Video(VideoFrame {
        pts: Some(0),
        planes,
    })
}

/// One AVIF file holding a `w × h` picture in `fmt`, through the
/// registry encoder (lossless).
fn still(ctx: &RuntimeContext, fmt: PixelFormat, w: u32, h: u32) -> Vec<u8> {
    let mut p = CodecParameters::video(CodecId::new(AVIF));
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = Some(fmt);
    let mut enc = ctx.codecs.first_encoder(&p).expect("encoder");
    enc.send_frame(&frame(fmt, w, h)).expect("send_frame");
    enc.receive_packet().expect("receive_packet").data
}

fn packet(bytes: &[u8]) -> Packet {
    Packet::new(0, TimeBase::new(1, 1), bytes.to_vec())
}

fn decoder(ctx: &RuntimeContext) -> Box<dyn Decoder> {
    ctx.codecs
        .first_decoder(&CodecParameters::video(CodecId::new(AVIF)))
        .expect("decoder")
}

/// `(width, height, label)` of an output description.
fn shape(p: &CodecParameters) -> (u32, u32, PixelFormat) {
    (
        p.width.expect("width"),
        p.height.expect("height"),
        p.pixel_format.expect("pixel_format"),
    )
}

/// The planes of a frame really have the described geometry: the
/// luma plane is `width` samples wide (two bytes each past 8 bits) and `height` rows tall, and the
/// image-plane count matches the label's.
fn assert_frame_matches(f: &Frame, p: &CodecParameters) {
    let Frame::Video(vf) = f else {
        panic!("expected a video frame")
    };
    let (w, h, fmt) = shape(p);
    let name = format!("{fmt:?}");
    let bps = if ["10", "12", "16"].iter().any(|d| name.contains(d)) {
        2
    } else {
        1
    };
    let y = &vf.planes[0];
    assert_eq!(
        y.stride,
        w as usize * bps,
        "luma stride vs described {fmt:?} width"
    );
    assert_eq!(
        y.data.len() / y.stride,
        h as usize,
        "rows vs described height"
    );
    assert_eq!(
        vf.image_plane_count(),
        fmt.plane_count(),
        "{fmt:?} plane count"
    );
}

/// The next media frame, asserting the `Frame::StreamParams` rules on
/// the way: at most one event before it, only when the generation
/// moved (by exactly one), never before the first frame, its payload
/// equal to `output_params()` at the moment it is returned. Returns
/// the frame and whether an event preceded it.
fn next_frame(dec: &mut dyn Decoder, last_gen: Option<u64>) -> Option<(Frame, Option<Frame>)> {
    let mut event = None;
    loop {
        let f = dec.receive_frame().ok()?;
        if let Some(p) = f.stream_params() {
            assert!(event.is_none(), "two events for one change");
            let prev = last_gen.expect("no event for the first shape");
            assert_eq!(
                dec.output_params_generation(),
                prev.wrapping_add(1),
                "the generation moved together with the event"
            );
            assert_eq!(
                format!("{p:?}"),
                format!("{:?}", dec.output_params().expect("described")),
                "event payload == output_params()"
            );
            event = Some(f);
            continue;
        }
        if let Some(prev) = last_gen {
            assert_eq!(
                event.is_some(),
                dec.output_params_generation() != prev,
                "one event per generation bump, none otherwise"
            );
        }
        return Some((f, event));
    }
}

/// Feed `files` either interleaved (send one, receive one) or all at
/// once (every packet queued before the first `receive_frame`), and
/// record after each returned frame: the generation, whether the
/// watch saw a change, and the description. One watch takes both
/// signals — the in-band event (`observe_frame`) and the counter
/// (`observe`) — and must report each change exactly once; the event
/// count equals the final generation.
fn run(
    dec: &mut dyn Decoder,
    files: &[Vec<u8>],
    queued: bool,
) -> Vec<(u64, bool, (u32, u32, PixelFormat))> {
    let mut stream = CodecParameters::video(CodecId::new(AVIF));
    let mut watch = OutputParamsWatch::new();
    // An event-only consumer: folds nothing but the events.
    let mut event_stream: Option<CodecParameters> = None;
    let mut events = 0u64;
    let mut last_gen = None;
    let mut out = Vec::new();
    let mut pull = |dec: &mut dyn Decoder, out: &mut Vec<_>| {
        while let Some((f, event)) = next_frame(dec, last_gen) {
            let mut by_event = false;
            if let Some(e) = &event {
                events += 1;
                let es = event_stream.as_mut().expect("first shape read");
                assert!(watch.observe_frame(e, es), "the event is a change");
                by_event = watch.observe_frame(e, &mut stream);
                assert!(by_event, "the event changes the mixed stream");
            }
            let by_counter = watch.observe(dec, &mut stream);
            assert!(!(by_event && by_counter), "change reported twice");
            let changed = by_event || by_counter;
            let p = dec.output_params().expect("described after a frame");
            assert_frame_matches(&f, &p);
            // The watch's stream folded the same description.
            assert_eq!(shape(&stream), shape(&p));
            assert_eq!(stream.color_signal, p.color_signal);
            // So did the event-only consumer (seeded from the first
            // frame's description, as `output_params()` announces it).
            let es = event_stream.get_or_insert_with(|| p.clone());
            assert_eq!(shape(es), shape(&p));
            assert_eq!(es.color_signal, p.color_signal);
            let generation = dec.output_params_generation();
            last_gen = Some(generation);
            out.push((generation, changed, shape(&p)));
        }
    };
    if queued {
        for f in files {
            dec.send_packet(&packet(f)).expect("send_packet");
        }
        pull(dec, &mut out);
    } else {
        for f in files {
            dec.send_packet(&packet(f)).expect("send_packet");
            pull(dec, &mut out);
        }
    }
    let final_gen = out.last().map_or(0, |r| r.0);
    assert_eq!(events, final_gen, "events and generation bumps agree 1:1");
    out
}

#[test]
fn constant_still_stream_never_bumps() {
    let ctx = registry();
    let a = still(&ctx, PixelFormat::Yuv420P, 16, 16);
    for queued in [false, true] {
        let mut dec = decoder(&ctx);
        assert!(dec.output_params().is_none(), "nothing decoded yet");
        let seen = run(
            &mut *dec,
            &[a.clone(), a.clone(), a.clone(), a.clone()],
            queued,
        );
        assert_eq!(seen.len(), 4);
        assert!(seen.iter().all(|(g, _, _)| *g == 0), "{seen:?}");
        let changes: Vec<bool> = seen.iter().map(|(_, c, _)| *c).collect();
        assert_eq!(changes, [true, false, false, false], "first read only");
    }
}

#[test]
fn still_burst_bumps_once_per_shape_change() {
    let ctx = registry();
    let a = still(&ctx, PixelFormat::Yuv420P, 16, 16);
    let wide = still(&ctx, PixelFormat::Yuv420P, 32, 16);
    let alpha = still(&ctx, PixelFormat::Yuva420P, 32, 16);
    let gray = still(&ctx, PixelFormat::Gray8, 32, 16);
    let files = [
        a.clone(),
        a.clone(),
        wide.clone(),
        wide,
        alpha,
        gray.clone(),
        gray,
        a,
    ];
    for queued in [false, true] {
        let mut dec = decoder(&ctx);
        let seen = run(&mut *dec, &files, queued);
        let gens: Vec<u64> = seen.iter().map(|(g, _, _)| *g).collect();
        assert_eq!(gens, [0, 0, 1, 1, 2, 3, 3, 4], "queued={queued}");
        let changes: Vec<bool> = seen.iter().map(|(_, c, _)| *c).collect();
        assert_eq!(
            changes,
            [true, false, true, false, true, true, false, true],
            "queued={queued}"
        );
        let dims: Vec<(u32, u32)> = seen.iter().map(|(_, _, s)| (s.0, s.1)).collect();
        assert_eq!(
            dims,
            [
                (16, 16),
                (16, 16),
                (32, 16),
                (32, 16),
                (32, 16),
                (32, 16),
                (32, 16),
                (16, 16)
            ]
        );
        // Opaque, alpha-carrying and monochrome pictures carry
        // different labels.
        assert_eq!(seen[3].2.2.plane_count(), 3);
        assert_eq!(seen[4].2.2.plane_count(), 4, "{:?}", seen[4].2.2);
        assert_eq!(seen[5].2.2.plane_count(), 1, "{:?}", seen[5].2.2);
    }
}

#[test]
fn description_switches_at_the_frame_boundary_not_at_send_packet() {
    let ctx = registry();
    let a = still(&ctx, PixelFormat::Yuv420P, 16, 16);
    let b = still(&ctx, PixelFormat::Yuv420P, 32, 16);
    let mut dec = decoder(&ctx);
    dec.send_packet(&packet(&a)).unwrap();
    // Before the first frame: the frame that comes next.
    assert_eq!(shape(&dec.output_params().unwrap()).0, 16);
    dec.send_packet(&packet(&b)).unwrap();
    assert_eq!(shape(&dec.output_params().unwrap()).0, 16);
    dec.receive_frame().unwrap();
    assert_eq!(dec.output_params_generation(), 0);
    assert_eq!(shape(&dec.output_params().unwrap()).0, 16);
    // `b` is queued but not returned: still `a`'s description.
    dec.send_packet(&packet(&b)).unwrap();
    assert_eq!(shape(&dec.output_params().unwrap()).0, 16);
    assert_eq!(dec.output_params_generation(), 0);
    // The change: the event first, the description and the counter
    // already moved when it is returned …
    let ev = dec.receive_frame().unwrap();
    assert_eq!(shape(ev.stream_params().expect("an event")).0, 32);
    assert_eq!(dec.output_params_generation(), 1);
    assert_eq!(shape(&dec.output_params().unwrap()).0, 32);
    // … then the frame of the new shape, without a second bump.
    assert!(matches!(dec.receive_frame().unwrap(), Frame::Video(_)));
    assert_eq!(dec.output_params_generation(), 1);
    assert_eq!(shape(&dec.output_params().unwrap()).0, 32);
    assert!(matches!(dec.receive_frame().unwrap(), Frame::Video(_)));
    assert_eq!(dec.output_params_generation(), 1, "same shape, no bump");
    assert!(dec.receive_frame().is_err());
    assert_eq!(dec.output_params_generation(), 1);
}

/// The first shape is never announced in-band, a constant stream
/// never sees an event, and `reset()` keeps the rule.
#[test]
fn events_only_for_mid_stream_changes() {
    let ctx = registry();
    let a = still(&ctx, PixelFormat::Yuv420P, 16, 16);
    let mut dec = decoder(&ctx);
    for _ in 0..3 {
        dec.send_packet(&packet(&a)).unwrap();
    }
    for _ in 0..3 {
        assert!(matches!(dec.receive_frame().unwrap(), Frame::Video(_)));
    }
    dec.reset().unwrap();
    dec.send_packet(&packet(&a)).unwrap();
    assert!(matches!(dec.receive_frame().unwrap(), Frame::Video(_)));
    assert_eq!(dec.output_params_generation(), 0);
}

#[test]
fn reset_keeps_the_rule() {
    let ctx = registry();
    let a = still(&ctx, PixelFormat::Yuv420P, 16, 16);
    let b = still(&ctx, PixelFormat::Gray8, 32, 16);
    let mut dec = decoder(&ctx);
    let mut stream = CodecParameters::video(CodecId::new(AVIF));
    let mut watch = OutputParamsWatch::new();
    dec.send_packet(&packet(&a)).unwrap();
    dec.receive_frame().unwrap();
    assert!(watch.observe(&*dec, &mut stream));
    // A queued frame of another shape is dropped by the reset: no
    // bump, the description stays the last returned frame's.
    dec.send_packet(&packet(&b)).unwrap();
    dec.reset().unwrap();
    assert_eq!(dec.output_params_generation(), 0);
    assert_eq!(shape(&dec.output_params().unwrap()), shape(&stream));
    // Same shape after the reset: no bump.
    dec.send_packet(&packet(&a)).unwrap();
    dec.receive_frame().unwrap();
    assert_eq!(dec.output_params_generation(), 0);
    assert!(!watch.observe(&*dec, &mut stream));
    // A different shape after the reset: one bump, announced by one
    // event right before its first frame.
    dec.reset().unwrap();
    dec.send_packet(&packet(&b)).unwrap();
    assert_eq!(dec.output_params_generation(), 0);
    let ev = dec.receive_frame().unwrap();
    assert!(ev.is_stream_params());
    assert_eq!(dec.output_params_generation(), 1);
    assert!(watch.observe_frame(&ev, &mut stream));
    assert!(matches!(dec.receive_frame().unwrap(), Frame::Video(_)));
    assert_eq!(dec.output_params_generation(), 1);
    assert!(
        !watch.observe(&*dec, &mut stream),
        "already seen via the event"
    );
    assert_eq!(shape(&stream).0, 32);
    assert_eq!(stream.pixel_format.unwrap().plane_count(), 1);
    // An event the consumer saw for a frame then dropped by a reset:
    // the description already switched, so returning to the old
    // shape is a change again — one more event, one more bump.
    dec.send_packet(&packet(&a)).unwrap();
    assert!(dec.receive_frame().unwrap().is_stream_params());
    assert_eq!(dec.output_params_generation(), 2);
    dec.reset().unwrap();
    dec.send_packet(&packet(&b)).unwrap();
    let ev = dec.receive_frame().unwrap();
    assert_eq!(shape(ev.stream_params().unwrap()).0, 32);
    assert_eq!(dec.output_params_generation(), 3);
    assert!(matches!(dec.receive_frame().unwrap(), Frame::Video(_)));
}

#[test]
fn tone_mapping_switch_is_one_change() {
    // The gain-map file decodes to the base by default and to the
    // `tmap` reconstruction when tone mapping is on: two shapes.
    let file = include_bytes!("fixtures/gainmap_tool.avif");
    let mut dec = AvifDecoder::new(CodecId::new(AVIF));
    let mut stream = CodecParameters::video(CodecId::new(AVIF));
    let mut watch = OutputParamsWatch::new();
    dec.send_packet(&packet(file)).unwrap();
    dec.receive_frame().unwrap();
    assert!(watch.observe(&dec, &mut stream));
    let base = dec.output_params().unwrap();
    dec.set_tone_mapped(true);
    dec.send_packet(&packet(file)).unwrap();
    let ev = dec.receive_frame().unwrap();
    assert!(ev.is_stream_params(), "the switch is announced in-band");
    assert_eq!(dec.output_params_generation(), 1);
    assert!(matches!(dec.receive_frame().unwrap(), Frame::Video(_)));
    assert_eq!(dec.output_params_generation(), 1);
    assert!(watch.observe(&dec, &mut stream));
    let applied = dec.output_params().unwrap();
    assert_eq!(
        format!("{:?}", ev.stream_params().unwrap()),
        format!("{applied:?}")
    );
    assert_ne!(
        (base.pixel_format, base.color_signal),
        (applied.pixel_format, applied.color_signal)
    );
    dec.send_packet(&packet(file)).unwrap();
    assert!(matches!(dec.receive_frame().unwrap(), Frame::Video(_)));
    assert_eq!(dec.output_params_generation(), 1);
    assert!(!watch.observe(&dec, &mut stream));
}

/// `n` all-intra 8-bit samples of `w × h` in `chroma`.
fn sequence(w: u32, h: u32, chroma: StillChroma, n: u16, duration: u32) -> Vec<u8> {
    let frames: Vec<StillImage> = (0..n)
        .map(|i| {
            let y: Vec<u16> = (0..w * h)
                .map(|k| (k as u16 * 3 + i * 17) % 220 + 16)
                .collect();
            let (cw, ch) = match chroma {
                StillChroma::Yuv420 => (w / 2, h / 2),
                _ => (0, 0),
            };
            let c: Vec<u16> = (0..cw * ch)
                .map(|k| (k as u16 * 5 + i) % 200 + 28)
                .collect();
            StillImage::yuv(w, h, 8, chroma, y, c.clone(), c).unwrap()
        })
        .collect();
    let opts = SequenceEncodeOptions::default()
        .with_timescale(30)
        .with_frame_duration(duration)
        .with_all_intra(true);
    encode_sequence(&frames, &opts).expect("sequence encode")
}

/// The coded samples of a sequence file.
fn samples_of(file: &[u8]) -> Vec<Vec<u8>> {
    let meta = oxideav_avif::parse_avis(file).unwrap();
    meta.samples
        .iter()
        .map(|s| oxideav_avif::sample_bytes(file, s).unwrap().to_vec())
        .collect()
}

#[test]
fn constant_sequence_reports_its_rate_and_never_bumps() {
    let file = sequence(32, 24, StillChroma::Yuv420, 4, 2);
    let predicted = stream_params(&file).unwrap();
    assert_eq!(predicted.frame_rate, Some(Rational::new(15, 1)));
    let ctx = registry();
    let mut dec = decoder(&ctx);
    let seen = run(&mut *dec, &[file], false);
    assert_eq!(seen.len(), 4);
    assert!(
        seen.iter()
            .all(|(g, _, s)| *g == 0 && (s.0, s.1) == (32, 24))
    );
    let changes: Vec<bool> = seen.iter().map(|(_, c, _)| *c).collect();
    assert_eq!(changes, [true, false, false, false]);
    let p = dec.output_params().unwrap();
    assert_eq!(p.frame_rate, Some(Rational::new(15, 1)));
    assert_eq!(shape(&p), shape(&predicted));
    assert_eq!(p.color_signal, predicted.color_signal);
}

#[test]
fn sequence_changing_size_and_layout_mid_track_bumps_at_each_key_frame() {
    // Three 32×24 4:2:0 samples, two 48×32 4:2:0, two 48×32
    // monochrome — each all-intra sample carries its own sequence
    // header, so one track can switch at any sample. The track is
    // re-assembled with the first part's sample entry.
    let a = sequence(32, 24, StillChroma::Yuv420, 3, 1);
    let b = sequence(48, 32, StillChroma::Yuv420, 2, 1);
    let c = sequence(48, 32, StillChroma::Monochrome, 2, 1);
    let meta = oxideav_avif::parse_avis(&a).unwrap();
    let av1c = meta.av1_codec_config.clone().unwrap();
    let config =
        oxideav_heif::props::Property::Av1C(oxideav_heif::Av1Config::parse(&av1c).unwrap());
    let mut writer = oxideav_heif::SequenceWriter::new(*b"av01", config, 32, 24, 30)
        .with_brands(*b"avis", vec![*b"avis", *b"msf1", *b"miaf", *b"mif1"]);
    for s in samples_of(&a)
        .into_iter()
        .chain(samples_of(&b))
        .chain(samples_of(&c))
    {
        writer.push_sample(s, 1, true);
    }
    let file = writer.write_to_vec().unwrap();
    let ctx = registry();
    for queued in [false, true] {
        let mut dec = decoder(&ctx);
        let seen = run(&mut *dec, std::slice::from_ref(&file), queued);
        let gens: Vec<u64> = seen.iter().map(|(g, _, _)| *g).collect();
        assert_eq!(gens, [0, 0, 0, 1, 1, 2, 2]);
        let changes = seen.iter().filter(|(_, c, _)| *c).count();
        assert_eq!(changes, 3, "first read + two switches");
        assert_eq!((seen[2].2.0, seen[2].2.1), (32, 24));
        assert_eq!((seen[3].2.0, seen[3].2.1), (48, 32));
        assert_eq!(seen[4].2.2.plane_count(), 3);
        assert_eq!(seen[5].2.2.plane_count(), 1);
        assert_eq!(
            dec.output_params().unwrap().frame_rate,
            Some(Rational::new(30, 1))
        );
    }
}

/// Every fixture: what the container states (`stream_params`) is what
/// the decoder reports after its first frame.
#[test]
fn container_prediction_matches_the_decoded_description() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("avif") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let predicted = stream_params(&bytes).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        let mut dec = AvifDecoder::new(CodecId::new(AVIF));
        dec.send_packet(&packet(&bytes))
            .unwrap_or_else(|e| panic!("{path:?}: {e}"));
        // Before the first frame the decoder already answers.
        let early = dec.output_params().expect("answer before the first frame");
        let f = dec.receive_frame().unwrap();
        let p = dec.output_params().unwrap();
        assert_eq!(shape(&early), shape(&p), "{path:?}");
        assert_eq!(early.color_signal, p.color_signal, "{path:?}");
        assert_frame_matches(&f, &p);
        assert_eq!(shape(&predicted), shape(&p), "{path:?}");
        assert_eq!(predicted.color_signal, p.color_signal, "{path:?}");
        assert_eq!(predicted.frame_rate, p.frame_rate, "{path:?}");
        let mut stream = predicted.clone();
        assert!(!stream.merge_output_params(&p), "{path:?}: nothing to fix");
        checked += 1;
    }
    assert!(checked >= 12, "{checked} fixtures");
}
