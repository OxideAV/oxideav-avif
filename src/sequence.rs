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
    prop_av1c, prop_clap, prop_colr, prop_ispe, prop_pixi, sequence_brands, ProfileBrand,
};
use crate::still::{
    av1_err, av1c_from_seq, coded_extent, colr_is_full_range, encode_coded_item,
    full_range_temporal_unit, pad_plane, pixi_bits, top_left_clap, StillImage, STILL_MAX_CODED_DIM,
};
use oxideav_av1::encoder::inter_frame::{encode_gop_yuv_with_q, GOP_MAX_FRAMES};
use oxideav_av1::encoder::yuv_frame::YuvFrame;
use oxideav_heif::{HeifWriter, SequenceWriter};

/// Tuning for [`encode_sequence`].
#[non_exhaustive]
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

impl SequenceEncodeOptions {
    /// Every field as a positional argument, in declaration order
    /// (the record is `#[non_exhaustive]`: build it here, or from
    /// `Default` where one exists, then read / assign the public
    /// fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        timescale: u32,
        frame_duration: u32,
        base_q_idx: u8,
        all_intra: bool,
        gop_length: usize,
    ) -> Self {
        Self {
            timescale,
            frame_duration,
            base_q_idx,
            all_intra,
            gop_length,
        }
    }
    /// Setter: replace `timescale`.
    pub fn with_timescale(mut self, timescale: u32) -> Self {
        self.timescale = timescale;
        self
    }
    /// Setter: replace `frame_duration`.
    pub fn with_frame_duration(mut self, frame_duration: u32) -> Self {
        self.frame_duration = frame_duration;
        self
    }
    /// Setter: replace `base_q_idx`.
    pub fn with_base_q_idx(mut self, base_q_idx: u8) -> Self {
        self.base_q_idx = base_q_idx;
        self
    }
    /// Setter: replace `all_intra`.
    pub fn with_all_intra(mut self, all_intra: bool) -> Self {
        self.all_intra = all_intra;
        self
    }
    /// Setter: replace `gop_length`.
    pub fn with_gop_length(mut self, gop_length: usize) -> Self {
        self.gop_length = gop_length;
        self
    }
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
    encode_sequence_timed(frames, opts, None)
}

/// [`encode_sequence`] with one display duration per frame (in
/// `opts.timescale` units) instead of the uniform `frame_duration`;
/// `None` is the uniform case.
pub(crate) fn encode_sequence_timed(
    frames: &[StillImage],
    opts: &SequenceEncodeOptions,
    durations: Option<&[u32]>,
) -> Result<Vec<u8>> {
    let first = frames
        .first()
        .ok_or_else(|| Error::invalid("avif sequence: no frames"))?;
    if let Some(d) = durations {
        if d.len() != frames.len() {
            return Err(Error::invalid(format!(
                "avif sequence: {} durations for {} frames",
                d.len(),
                frames.len()
            )));
        }
    }
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
    let clap = ((pw, ph) != (first.width, first.height))
        .then(|| top_left_clap(first.width, first.height, pw, ph));
    let all_sync = samples.iter().all(|(_, s)| *s);
    if pw > u32::from(u16::MAX) || ph > u32::from(u16::MAX) {
        return Err(Error::unsupported(
            "avif sequence: coded extents exceed the 16-bit sample-entry fields",
        ));
    }
    // The file is the container's `SequenceWriter`: `ftyp` with this
    // crate's brands, the cover still (one `av01` item with the
    // track's properties, its `iloc` aliasing sample 0 in the track
    // `mdat` — HEIF §7.1 recommends the cover, av1-avif §6.3 NOTE
    // requires the item), `moov` / `trak` with the `pict` handler,
    // the `av01` sample entry carrying `av1C` (+ `colr`, + `clap`)
    // and the mandatory `ccst` (HEIF §7.2.3.1), `stts` / `stsc` /
    // `stsz` / `stco`, `stss` only when a sample is not sync.
    let pixi = pixi_bits(first);
    let mut props = vec![
        prop_av1c(&av1c, "sequence ")?,
        prop_ispe(pw, ph),
        prop_pixi(&pixi),
    ];
    let mut entry_properties = Vec::new();
    if let Some(c) = first.colr.as_ref() {
        let (colr, _) = prop_colr(c)?;
        entry_properties.push(colr.clone());
        props.push((colr, false));
    }
    if let Some(c) = clap.as_ref() {
        let (clap, essential) = prop_clap(c);
        entry_properties.push(clap.clone());
        props.push((clap, essential));
    }
    let (av1c_prop, _) = prop_av1c(&av1c, "sequence ")?;
    let (major, compat) = sequence_brands(profile_brand, all_sync);
    let mut writer = SequenceWriter::new(*b"av01", av1c_prop, pw as u16, ph as u16, opts.timescale)
        .with_brands(major, compat);
    writer.entry_properties = entry_properties;
    // `ccst` (HEIF §7.2.3.4): all-intra tracks have no inter-predicted
    // image (every reference is vacuously a sync sample, no reference
    // pictures); the KEY + P groups predict from the previous frame,
    // intra prediction may be used, any number of references the
    // sample entry permits (15).
    writer.coding_constraints = if all_sync {
        (true, true, 0)
    } else {
        (false, true, 15)
    };
    let mut still = HeifWriter::new();
    // The body is ignored: `cover_sample` points the item at sample 0.
    let cover = still.add_coded_item(*b"av01", Vec::new(), props);
    still.set_primary(cover);
    writer.still = Some(still);
    writer.cover_sample = Some(0);
    for (i, (data, sync)) in samples.into_iter().enumerate() {
        let duration = durations
            .and_then(|d| d.get(i).copied())
            .unwrap_or(opts.frame_duration);
        writer.push_sample(data, duration, sync);
    }
    Ok(writer.write_to_vec()?)
}
