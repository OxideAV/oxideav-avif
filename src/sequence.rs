//! AVIF **image sequence** (`avis`) encode — av1-avif §3 / §6.3 / §8.2,
//! the AV1 ISOBMFF binding §2 (AV1 sample entry, sample format, sync
//! samples) and ISO/IEC 14496-12 §8 (movie / track / sample tables).
//!
//! [`encode_sequence`] turns a list of same-layout [`StillImage`]s into
//! one file:
//!
//! * `ftyp` — major `avis`, compatible `avis` / `avif` / `mif1` /
//!   `msf1` / `miaf` / `av01` (the binding's §2.1 `SHALL`), the AVIF
//!   profile brand elected from the coded `seq_profile` (§8), and
//!   `avio` when every sample is a KEY frame (§6.3).
//! * `meta` — a still primary `av01` item aliasing the first sample's
//!   bytes in `mdat` (§6.3 NOTE: a file that is primarily an image
//!   sequence still has at least an image item; the first sample is a
//!   sync sample, i.e. valid AV1 Image Item Data per §2.1).
//! * `moov` / `trak` — `'pict'` handler (§3), one `av01` sample entry
//!   (§3: exactly one) carrying `av1C` (+ `colr`, + `clap` when the
//!   coded extents pad the requested ones), `stts` / `stsc` / `stsz` /
//!   `stco` and — when not every sample is a sync sample — `stss`
//!   (14496-12 §8.6.2: absence means every sample is sync).
//! * `mdat` — the AV1 temporal units, one per sample (binding §2.4).
//!
//! Frames are coded either all-intra (every sample a KEY frame, one
//! Sequence Header per sample — §3 requires repeated headers to be
//! identical, which holds by construction) or as KEY + P groups of up
//! to [`SequenceEncodeOptions::gop_length`] frames through the AV1
//! crate's inter encoder; the first sample of each group is the sync
//! sample.

use crate::error::{AvifError as Error, Result};
use crate::mux::{
    boxed, build_meta, build_property_table, full_boxed, prop_av1c, prop_clap, prop_colr,
    prop_ispe, prop_pixi, MuxItem, ProfileBrand, W,
};
use crate::still::{
    av1_err, av1c_from_seq, coded_extent, colr_is_full_range, encode_coded_item,
    full_range_temporal_unit, pad_plane, pixi_bits, top_left_clap, StillImage, STILL_MAX_CODED_DIM,
};
use oxideav_av1::encoder::inter_frame::{encode_gop_yuv_with_q, GOP_MAX_FRAMES};
use oxideav_av1::encoder::yuv_frame::YuvFrame;

/// Tuning for [`encode_sequence`].
#[derive(Clone, Copy, Debug)]
pub struct SequenceEncodeOptions {
    /// Movie / media timescale in ticks per second (`mvhd` / `mdhd`).
    pub timescale: u32,
    /// Duration of every sample in timescale ticks (`stts`).
    pub frame_duration: u32,
    /// AV1 `base_q_idx`; `0` = lossless.
    pub base_q_idx: u8,
    /// Code every frame as a KEY frame (every sample a sync sample,
    /// `avio` brand). Otherwise KEY + P groups.
    pub all_intra: bool,
    /// Frames per KEY + P group, 1..=`GOP_MAX_FRAMES` (64). Ignored
    /// when `all_intra`.
    pub gop_length: usize,
}

impl Default for SequenceEncodeOptions {
    fn default() -> Self {
        Self {
            timescale: 30,
            frame_duration: 1,
            base_q_idx: 0,
            all_intra: false,
            gop_length: GOP_MAX_FRAMES,
        }
    }
}

/// Encode `frames` as an `avis` image sequence. Every frame must share
/// one width, height, bit depth and chroma layout; alpha / depth
/// auxiliaries are not carried (an auxiliary image sequence track is
/// out of scope here).
pub fn encode_sequence(frames: &[StillImage], opts: &SequenceEncodeOptions) -> Result<Vec<u8>> {
    let first = frames
        .first()
        .ok_or_else(|| Error::invalid("avif sequence: no frames"))?;
    if opts.timescale == 0 || opts.frame_duration == 0 {
        return Err(Error::invalid(
            "avif sequence: timescale and frame_duration must be non-zero",
        ));
    }
    if !(1..=GOP_MAX_FRAMES).contains(&opts.gop_length) && !opts.all_intra {
        return Err(Error::invalid(format!(
            "avif sequence: gop_length {} outside 1..={GOP_MAX_FRAMES}",
            opts.gop_length
        )));
    }
    let (pw, ph) = (coded_extent(first.width), coded_extent(first.height));
    if pw > STILL_MAX_CODED_DIM || ph > STILL_MAX_CODED_DIM {
        return Err(Error::unsupported(format!(
            "avif sequence: coded extents {pw}x{ph} exceed {STILL_MAX_CODED_DIM}"
        )));
    }
    let mut yuv = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        f.validate()?;
        if (f.width, f.height, f.bit_depth, f.chroma)
            != (first.width, first.height, first.bit_depth, first.chroma)
        {
            return Err(Error::invalid(format!(
                "avif sequence: frame {i} ({}x{} {}-bit {:?}) differs from frame 0 ({}x{} {}-bit {:?})",
                f.width, f.height, f.bit_depth, f.chroma, first.width, first.height,
                first.bit_depth, first.chroma
            )));
        }
        if f.alpha.is_some() || f.depth_map.is_some() {
            return Err(Error::unsupported(
                "avif sequence: auxiliary (alpha / depth) image sequences are not supported",
            ));
        }
        let (w, h) = (f.width as usize, f.height as usize);
        let y = pad_plane(&f.y, w, h, pw as usize, ph as usize);
        let (u, v) = if f.chroma.has_chroma() {
            let (sx, sy) = f.chroma.subsampling();
            let (cw, ch) = f.chroma_dims();
            let (pcw, pch) = ((pw >> sx) as usize, (ph >> sy) as usize);
            (
                pad_plane(&f.u, cw as usize, ch as usize, pcw, pch),
                pad_plane(&f.v, cw as usize, ch as usize, pcw, pch),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        yuv.push(YuvFrame {
            width: pw,
            height: ph,
            bit_depth: f.bit_depth,
            format: f.chroma.to_av1(),
            y,
            u,
            v,
        });
    }
    let full_range = colr_is_full_range(first);

    // Code the samples.
    let mut samples: Vec<(Vec<u8>, bool)> = Vec::with_capacity(frames.len());
    let mut av1c: Option<Vec<u8>> = None;
    let mut seq_profile = 0u8;
    if opts.all_intra {
        for f in &yuv {
            let coded = encode_coded_item(
                pw,
                ph,
                f.bit_depth,
                f.format,
                f.y.clone(),
                f.u.clone(),
                f.v.clone(),
                opts.base_q_idx,
                full_range,
                "sequence frame",
            )?;
            if av1c.is_none() {
                av1c = Some(coded.av1c);
                seq_profile = coded.seq_profile;
            }
            samples.push((coded.payload, true));
        }
    } else {
        for chunk in yuv.chunks(opts.gop_length) {
            let gop = encode_gop_yuv_with_q(chunk, opts.base_q_idx)
                .map_err(|e| av1_err("sequence GOP", e))?;
            if av1c.is_none() {
                av1c = Some(av1c_from_seq(&gop.seq));
                seq_profile = gop.seq.seq_profile;
            }
            for (i, unit) in gop.temporal_units.into_iter().enumerate() {
                let unit = if i == 0 && full_range {
                    full_range_temporal_unit(&unit, &gop.seq)?
                } else {
                    unit
                };
                samples.push((unit, i == 0));
            }
        }
    }
    let av1c = av1c.expect("at least one sample");
    if samples.len() != frames.len() {
        return Err(Error::invalid(format!(
            "avif sequence: coded {} samples for {} frames",
            samples.len(),
            frames.len()
        )));
    }
    let profile_brand = match seq_profile {
        0 => ProfileBrand::Baseline,
        1 => ProfileBrand::Advanced,
        _ => ProfileBrand::Bare,
    };

    // Still primary item = sample 0 (its bytes live in mdat).
    let clap = ((pw, ph) != (first.width, first.height))
        .then(|| top_left_clap(first.width, first.height, pw, ph));
    let mut props = vec![
        prop_av1c(&av1c),
        prop_ispe(pw, ph),
        prop_pixi(&pixi_bits(first)),
    ];
    if let Some(colr) = &first.colr {
        props.push(prop_colr(colr)?);
    }
    if let Some(c) = &clap {
        props.push(prop_clap(c));
    }
    let items = vec![MuxItem {
        id: 1,
        item_type: *b"av01",
        name: String::new(),
        hidden: false,
        content_type: None,
        payload: samples[0].0.clone(),
        props,
    }];
    let (ipco_props, item_assocs) = build_property_table(&items)?;

    let all_sync = samples.iter().all(|(_, s)| *s);
    let ftyp = build_sequence_ftyp(profile_brand, all_sync);
    let sample_sizes: Vec<u32> = samples.iter().map(|(b, _)| b.len() as u32).collect();
    let sync_samples: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(_, (_, s))| *s)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    let track = TrackLayout {
        timescale: opts.timescale,
        frame_duration: opts.frame_duration,
        coded_width: pw,
        coded_height: ph,
        av1c: &av1c,
        colr: first.colr.as_ref(),
        clap: clap.as_ref(),
        sample_sizes: &sample_sizes,
        sync_samples: if all_sync { None } else { Some(&sync_samples) },
    };
    // Two-pass layout: meta and moov sizes do not depend on the offsets
    // they carry (fixed-width fields), so build with 0 to measure, then
    // rebuild with the absolute mdat data start.
    let probe_meta = build_meta(&items, 1, &[], &ipco_props, &item_assocs, &[0], 0);
    let probe_moov = build_moov(&track, 0)?;
    let mdat_data_start = (ftyp.len() + probe_meta.len() + probe_moov.len() + 8) as u64;
    let meta = build_meta(
        &items,
        1,
        &[],
        &ipco_props,
        &item_assocs,
        &[0],
        mdat_data_start,
    );
    let moov = build_moov(&track, mdat_data_start)?;
    debug_assert_eq!(meta.len(), probe_meta.len());
    debug_assert_eq!(moov.len(), probe_moov.len());
    let mut mdat_payload = Vec::new();
    for (bytes, _) in &samples {
        mdat_payload.extend_from_slice(bytes);
    }
    let mut out = Vec::with_capacity(mdat_data_start as usize + mdat_payload.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&meta);
    out.extend_from_slice(&moov);
    out.extend_from_slice(&boxed(b"mdat", &mdat_payload));
    Ok(out)
}

/// `ftyp` for an image sequence: av1-avif §6.3 / §8.2 / §8.3 brand sets
/// plus the AV1 ISOBMFF binding's `av01` (§2.1) and `avio` when every
/// sample is a sync sample (§6.3).
fn build_sequence_ftyp(profile_brand: ProfileBrand, all_sync: bool) -> Vec<u8> {
    let mut w = W::default();
    w.fourcc(b"avis"); // major_brand
    w.u32(0); // minor_version
    w.fourcc(b"avis");
    w.fourcc(b"avif");
    w.fourcc(b"mif1");
    w.fourcc(b"msf1");
    w.fourcc(b"miaf");
    w.fourcc(b"av01");
    if all_sync {
        w.fourcc(b"avio");
    }
    match profile_brand {
        ProfileBrand::Baseline => w.fourcc(b"MA1B"),
        ProfileBrand::Advanced => w.fourcc(b"MA1A"),
        ProfileBrand::Bare => {}
    }
    boxed(b"ftyp", &w.into_vec())
}

/// Everything the `moov` writer needs about the single video track.
struct TrackLayout<'a> {
    timescale: u32,
    frame_duration: u32,
    coded_width: u32,
    coded_height: u32,
    av1c: &'a [u8],
    colr: Option<&'a crate::meta::Colr>,
    clap: Option<&'a crate::meta::Clap>,
    sample_sizes: &'a [u32],
    /// 1-based sync sample numbers; `None` = every sample is sync (no
    /// `stss`, 14496-12 §8.6.2).
    sync_samples: Option<&'a [u32]>,
}

const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn build_moov(t: &TrackLayout<'_>, chunk_offset: u64) -> Result<Vec<u8>> {
    let sample_count = t.sample_sizes.len() as u64;
    let duration = sample_count * u64::from(t.frame_duration);
    let v1 = duration > u64::from(u32::MAX);

    // mvhd (14496-12 §8.2.2).
    let mut w = W::default();
    if v1 {
        w.u64(0);
        w.u64(0);
        w.u32(t.timescale);
        w.u64(duration);
    } else {
        w.u32(0);
        w.u32(0);
        w.u32(t.timescale);
        w.u32(duration as u32);
    }
    w.u32(0x0001_0000); // rate 1.0
    w.u16(0x0100); // volume
    w.u16(0); // reserved
    w.u32(0);
    w.u32(0); // reserved[2]
    for m in UNITY_MATRIX {
        w.u32(m);
    }
    for _ in 0..6 {
        w.u32(0); // pre_defined
    }
    w.u32(2); // next_track_ID
    let mvhd = full_boxed(b"mvhd", if v1 { 1 } else { 0 }, 0, &w.into_vec());

    // tkhd (§8.3.2): enabled + in movie; width/height 16.16 fixed.
    let mut w = W::default();
    if v1 {
        w.u64(0);
        w.u64(0);
        w.u32(1); // track_ID
        w.u32(0); // reserved
        w.u64(duration);
    } else {
        w.u32(0);
        w.u32(0);
        w.u32(1);
        w.u32(0);
        w.u32(duration as u32);
    }
    w.u32(0);
    w.u32(0); // reserved[2]
    w.u16(0); // layer
    w.u16(0); // alternate_group
    w.u16(0); // volume (video)
    w.u16(0); // reserved
    for m in UNITY_MATRIX {
        w.u32(m);
    }
    w.u32(t.coded_width << 16);
    w.u32(t.coded_height << 16);
    let tkhd = full_boxed(b"tkhd", if v1 { 1 } else { 0 }, 0x000003, &w.into_vec());

    // mdhd (§8.4.2), language 'und' packed 5-bit.
    let mut w = W::default();
    if v1 {
        w.u64(0);
        w.u64(0);
        w.u32(t.timescale);
        w.u64(duration);
    } else {
        w.u32(0);
        w.u32(0);
        w.u32(t.timescale);
        w.u32(duration as u32);
    }
    let und = ((b'u' - 0x60) as u16) << 10 | ((b'n' - 0x60) as u16) << 5 | (b'd' - 0x60) as u16;
    w.u16(und);
    w.u16(0); // pre_defined
    let mdhd = full_boxed(b"mdhd", if v1 { 1 } else { 0 }, 0, &w.into_vec());

    // hdlr (§8.4.3): 'pict' (av1-avif §3).
    let mut w = W::default();
    w.u32(0); // pre_defined
    w.fourcc(b"pict");
    w.u32(0);
    w.u32(0);
    w.u32(0); // reserved[3]
    w.cstr("PictureHandler");
    let hdlr = full_boxed(b"hdlr", 0, 0, &w.into_vec());

    // vmhd (§12.1.2): flags = 1.
    let mut w = W::default();
    w.u16(0); // graphicsmode
    w.u16(0);
    w.u16(0);
    w.u16(0); // opcolor
    let vmhd = full_boxed(b"vmhd", 0, 1, &w.into_vec());

    // dinf / dref / url (§8.7.2): one self-contained entry.
    let url = full_boxed(b"url ", 0, 1, &[]);
    let mut w = W::default();
    w.u32(1);
    w.bytes(&url);
    let dref = full_boxed(b"dref", 0, 0, &w.into_vec());
    let dinf = boxed(b"dinf", &dref);

    // stsd / av01 VisualSampleEntry (§12.1.3 + AV1-ISOBMFF §2.2).
    if t.coded_width > u32::from(u16::MAX) || t.coded_height > u32::from(u16::MAX) {
        return Err(Error::unsupported(
            "avif sequence: coded extents exceed the 16-bit sample-entry fields",
        ));
    }
    let mut w = W::default();
    w.bytes(&[0u8; 6]); // reserved
    w.u16(1); // data_reference_index
    w.u16(0); // pre_defined
    w.u16(0); // reserved
    w.u32(0);
    w.u32(0);
    w.u32(0); // pre_defined[3]
    w.u16(t.coded_width as u16);
    w.u16(t.coded_height as u16);
    w.u32(0x0048_0000); // horizresolution 72 dpi
    w.u32(0x0048_0000); // vertresolution
    w.u32(0); // reserved
    w.u16(1); // frame_count
    let mut name = [0u8; 32];
    let label = b"AOM Coding";
    name[0] = label.len() as u8;
    name[1..1 + label.len()].copy_from_slice(label);
    w.bytes(&name); // compressorname (§2.2.4 recommended value)
    w.u16(0x0018); // depth
    w.u16(0xFFFF); // pre_defined = -1
    w.bytes(&boxed(b"av1C", t.av1c));
    if let Some(colr) = t.colr {
        w.bytes(&prop_colr(colr)?.bytes);
    }
    if let Some(clap) = t.clap {
        w.bytes(&prop_clap(clap).bytes);
    }
    let av01 = boxed(b"av01", &w.into_vec());
    let mut w = W::default();
    w.u32(1); // entry_count
    w.bytes(&av01);
    let stsd = full_boxed(b"stsd", 0, 0, &w.into_vec());

    // stts (§8.6.1.2): one run.
    let mut w = W::default();
    w.u32(1);
    w.u32(sample_count as u32);
    w.u32(t.frame_duration);
    let stts = full_boxed(b"stts", 0, 0, &w.into_vec());

    // stss (§8.6.2) only when not every sample is sync.
    let stss = t.sync_samples.map(|sync| {
        let mut w = W::default();
        w.u32(sync.len() as u32);
        for &s in sync {
            w.u32(s);
        }
        full_boxed(b"stss", 0, 0, &w.into_vec())
    });

    // stsc (§8.7.4): one chunk holding every sample.
    let mut w = W::default();
    w.u32(1);
    w.u32(1); // first_chunk
    w.u32(sample_count as u32); // samples_per_chunk
    w.u32(1); // sample_description_index
    let stsc = full_boxed(b"stsc", 0, 0, &w.into_vec());

    // stsz (§8.7.3.2): per-sample table.
    let mut w = W::default();
    w.u32(0); // sample_size = 0 → table follows
    w.u32(sample_count as u32);
    for &s in t.sample_sizes {
        w.u32(s);
    }
    let stsz = full_boxed(b"stsz", 0, 0, &w.into_vec());

    // stco / co64 (§8.7.5): the single chunk.
    let stco = if chunk_offset > u64::from(u32::MAX) {
        let mut w = W::default();
        w.u32(1);
        w.u64(chunk_offset);
        full_boxed(b"co64", 0, 0, &w.into_vec())
    } else {
        let mut w = W::default();
        w.u32(1);
        w.u32(chunk_offset as u32);
        full_boxed(b"stco", 0, 0, &w.into_vec())
    };

    let mut stbl = Vec::new();
    stbl.extend_from_slice(&stsd);
    stbl.extend_from_slice(&stts);
    if let Some(stss) = &stss {
        stbl.extend_from_slice(stss);
    }
    stbl.extend_from_slice(&stsc);
    stbl.extend_from_slice(&stsz);
    stbl.extend_from_slice(&stco);
    let stbl = boxed(b"stbl", &stbl);

    let mut minf = Vec::new();
    minf.extend_from_slice(&vmhd);
    minf.extend_from_slice(&dinf);
    minf.extend_from_slice(&stbl);
    let minf = boxed(b"minf", &minf);

    let mut mdia = Vec::new();
    mdia.extend_from_slice(&mdhd);
    mdia.extend_from_slice(&hdlr);
    mdia.extend_from_slice(&minf);
    let mdia = boxed(b"mdia", &mdia);

    let mut trak = Vec::new();
    trak.extend_from_slice(&tkhd);
    trak.extend_from_slice(&mdia);
    let trak = boxed(b"trak", &trak);

    let mut moov = Vec::new();
    moov.extend_from_slice(&mvhd);
    moov.extend_from_slice(&trak);
    Ok(boxed(b"moov", &moov))
}
