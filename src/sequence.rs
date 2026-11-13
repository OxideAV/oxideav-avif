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
//!   (§3: exactly one) carrying `av1C` (+ `colr`) at the pictures' own
//!   size (no `clap`), `stts` / `stsc` / `stsz` /
//!   `stco` and — when not every sample is a sync sample — `stss`
//!   (14496-12 §8.6.2: absence means every sample is sync).
//! * `mdat` — the AV1 temporal units, one per sample (binding §2.4).
//! * Frames with an alpha plane add an **alpha auxiliary track** (HEIF
//!   §7.5.3, av1-avif §4.1): a second `av01` track with the `auxv`
//!   handler, an `auxl` track reference to the picture track and the
//!   alpha URN in its sample entry's `auxi`; its samples are
//!   monochrome, full-range KEY frames at the picture's depth, one per
//!   picture sample with the same duration (the time-parallel sample
//!   is the picture's alpha).
//! * [`SequenceEncodeOptions::loop_count`] writes the edit list MIAF
//!   reads the repetition count from; Exif / XMP of the first frame
//!   ride as items of the picture track's `meta`.
//!
//! Every sample — KEY frames and the P frames of a group alike — is
//! coded at the picture's own extents, whatever they are (odd, not a
//! multiple of 8, sub-sampled chroma at an odd size): the sample
//! entry, the `tkhd` and the decoded frame state one size and there
//! is no clean aperture for a player to apply (ISO/IEC 14496-12
//! §8.5.2: the entry documents the cropped frame dimensions; block
//! padding is the codec's own business).
//!
//! Frames are coded either all-intra (every sample a KEY frame, one
//! Sequence Header per sample — §3 requires repeated headers to be
//! identical, which holds by construction) or as KEY + P groups of up
//! to [`SequenceEncodeOptions::gop_length`] frames through the AV1
//! crate's inter encoder; the first sample of each group is the sync
//! sample.

use crate::api::EncodeSpeed;
use crate::error::{AvifError as Error, Result};
use crate::meta::Colr;
use crate::mux::{ProfileBrand, prop_av1c, prop_colr, prop_ispe, prop_pixi, sequence_brands};
use crate::source::{PictureView, TileSource};
use crate::still::{
    CodedTuning, STILL_MAX_CODED_DIM, StillImage, StillProperties, av1_err, av1c_from_seq,
    code_in_parallel, colour_description_of, colr_full_range, encode_coded_item, item_extent,
    resignalled_temporal_unit,
};
use oxideav_av1::encoder::inter_frame::{GOP_MAX_FRAMES, encode_gop_yuv_with_q};
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
    /// AV1 `base_q_idx` of the alpha auxiliary track's samples; `0` =
    /// lossless (the default).
    pub alpha_q_idx: u8,
    /// How often the sequence plays: `None` (default) writes no edit
    /// list, which players read as "indefinitely"; `Some(0)` states
    /// "indefinitely" explicitly; `Some(n)` plays it `n` times (MIAF
    /// Annex A.2: an `elst` over the media with the repeat flag and the
    /// track duration multiplied).
    pub loop_count: Option<u32>,
    /// Worker threads: all-intra samples, the alpha track's samples
    /// and whole KEY + P groups are independent of each other and are
    /// coded on up to this many workers; the bytes never depend on
    /// it. Default `1`.
    pub threads: usize,
    /// AV1 search effort of the all-intra and alpha samples (default
    /// [`EncodeSpeed::Fast`]).
    pub speed: EncodeSpeed,
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
            ..Self::default()
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
    /// Setter: replace `alpha_q_idx`.
    pub fn with_alpha_q_idx(mut self, alpha_q_idx: u8) -> Self {
        self.alpha_q_idx = alpha_q_idx;
        self
    }
    /// Setter: replace `loop_count`.
    pub fn with_loop_count(mut self, loop_count: Option<u32>) -> Self {
        self.loop_count = loop_count;
        self
    }
    /// Setter: replace `threads` (clamped to at least 1).
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }
    /// Setter: replace `speed`.
    pub fn with_speed(mut self, speed: EncodeSpeed) -> Self {
        self.speed = speed;
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
            alpha_q_idx: 0,
            loop_count: None,
            threads: 1,
            speed: EncodeSpeed::Fast,
        }
    }
}

/// Encode `frames` as an `avis` image sequence. Every frame must share
/// one width, height, bit depth and chroma layout. When the frames
/// carry an alpha plane (all of them, or none) it is written as the
/// alpha auxiliary track; depth maps are not carried. The first
/// frame's Exif / XMP ([`crate::StillProperties`]) describe the whole
/// sequence.
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
    let mut views = Vec::with_capacity(frames.len());
    for f in frames {
        f.validate()?;
        if f.depth_map.is_some() {
            return Err(Error::unsupported(
                "avif sequence: depth-map auxiliary sequences are not supported",
            ));
        }
        views.push(PictureView::of_still(f)?);
    }
    let sources: Vec<&dyn TileSource> = views.iter().map(|v| v as &dyn TileSource).collect();
    encode_sequence_sources(&sources, first.colr.as_ref(), &first.props, opts, durations)
}

/// The sequence encoder over [`TileSource`]s: every frame is read from
/// wherever the caller keeps it, when its sample (or its KEY + P
/// group) is coded — the encode holds the frames in flight, not a
/// copy of the sequence. `colr` and `metadata` (ICC / Exif / XMP) are the
/// first frame's and describe the whole sequence.
pub(crate) fn encode_sequence_sources(
    frames: &[&dyn TileSource],
    colr: Option<&Colr>,
    metadata: &StillProperties,
    opts: &SequenceEncodeOptions,
    durations: Option<&[u32]>,
) -> Result<Vec<u8>> {
    let first = *frames
        .first()
        .ok_or_else(|| Error::invalid("avif sequence: no frames"))?;
    let (width, height) = (first.width(), first.height());
    let (bit_depth, chroma, has_alpha) = (first.bit_depth(), first.chroma(), first.has_alpha());
    if width == 0 || height == 0 {
        return Err(Error::invalid(
            "avif still: dimensions must be at least 1x1",
        ));
    }
    if let Some(d) = durations
        && d.len() != frames.len()
    {
        return Err(Error::invalid(format!(
            "avif sequence: {} durations for {} frames",
            d.len(),
            frames.len()
        )));
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
    // Every sample is coded at the picture's own extents (KEY frames
    // and KEY + P groups alike: the AV1 coder pads to its mode-info
    // grid internally and codes the true frame size).
    let (pw, ph) = (item_extent(width), item_extent(height));
    if pw > STILL_MAX_CODED_DIM || ph > STILL_MAX_CODED_DIM {
        return Err(Error::unsupported(format!(
            "avif sequence: coded extents {pw}x{ph} exceed {STILL_MAX_CODED_DIM}"
        )));
    }
    for (i, f) in frames.iter().enumerate() {
        if (f.width(), f.height(), f.bit_depth(), f.chroma()) != (width, height, bit_depth, chroma)
        {
            return Err(Error::invalid(format!(
                "avif sequence: frame {i} ({}x{} {}-bit {:?}) differs from frame 0 ({}x{} {}-bit {:?})",
                f.width(),
                f.height(),
                f.bit_depth(),
                f.chroma(),
                width,
                height,
                bit_depth,
                chroma
            )));
        }
        if f.has_alpha() != has_alpha {
            return Err(Error::invalid(format!(
                "avif sequence: frame {i} {} an alpha plane but frame 0 {} — the alpha \
                 auxiliary track needs one sample per picture",
                if f.has_alpha() { "has" } else { "lacks" },
                if has_alpha { "has one" } else { "has none" },
            )));
        }
    }
    let (w, h) = (width as usize, height as usize);
    let format = chroma.to_av1();
    let full_range = colr_full_range(colr);
    let colour = colour_description_of(colr, chroma);

    // Code the samples.
    let mut samples: Vec<(Vec<u8>, bool)> = Vec::with_capacity(frames.len());
    let mut av1c: Option<Vec<u8>> = None;
    let mut seq_profile = 0u8;
    let threads = opts.threads.max(1);
    let workers =
        oxideav_core::ExecutionContext::with_threads(threads).effective_workers(frames.len());
    let tuning = CodedTuning::sequence(opts.speed, (threads / workers).max(1));
    if opts.all_intra {
        // Independent KEY frames: coded on the workers, in sample
        // order, each copied into the coder's planes when its worker
        // picks it up.
        let coded = code_in_parallel(frames.len(), workers, &|i| {
            let [y, u, v] = frames[i].colour(0, 0, w, h)?;
            encode_coded_item(
                pw,
                ph,
                bit_depth,
                format,
                y,
                u,
                v,
                opts.base_q_idx,
                full_range,
                colour,
                tuning,
                "sequence frame",
            )
        })?;
        for coded in coded {
            if av1c.is_none() {
                av1c = Some(coded.av1c);
                seq_profile = coded.seq_profile;
            }
            samples.push((coded.payload, true));
        }
    } else {
        // KEY + P groups are independent of each other: coded on the
        // workers, in group order, each group's frames copied into the
        // coder's planes when its worker picks the group up (the
        // working set is the caller's frames plus one group per
        // worker, not a second copy of the whole sequence).
        let groups: Vec<&[&dyn TileSource]> = frames.chunks(opts.gop_length).collect();
        let group_workers =
            oxideav_core::ExecutionContext::with_threads(threads).effective_workers(groups.len());
        let coded = code_in_parallel(groups.len(), group_workers, &|g| {
            let chunk = groups[g]
                .iter()
                .map(|f| {
                    let [y, u, v] = f.colour(0, 0, w, h)?;
                    Ok(YuvFrame {
                        width: pw,
                        height: ph,
                        bit_depth,
                        format,
                        y,
                        u,
                        v,
                    })
                })
                .collect::<Result<Vec<YuvFrame>>>()?;
            encode_gop_yuv_with_q(&chunk, opts.base_q_idx).map_err(|e| av1_err("sequence GOP", e))
        })?;
        for gop in coded {
            if av1c.is_none() {
                av1c = Some(av1c_from_seq(&gop.seq));
                seq_profile = gop.seq.seq_profile;
            }
            for (i, unit) in gop.temporal_units.into_iter().enumerate() {
                let unit = if i == 0 && (full_range || colour.is_some()) {
                    resignalled_temporal_unit(&unit, &gop.seq, full_range, colour)?
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
    // the `av01` sample entry carrying `av1C` (+ `colr`)
    // and the mandatory `ccst` (HEIF §7.2.3.1), `stts` / `stsc` /
    // `stsz` / `stco`, `stss` only when a sample is not sync.
    let pixi = vec![bit_depth; if chroma.has_chroma() { 3 } else { 1 }];
    let mut props = vec![
        prop_av1c(&av1c, "sequence ")?,
        prop_ispe(pw, ph),
        prop_pixi(&pixi),
    ];
    let mut entry_properties = Vec::new();
    if let Some(c) = colr {
        let (colr, _) = prop_colr(c)?;
        entry_properties.push(colr.clone());
        props.push((colr, false));
    }
    if let Some(icc) = metadata.icc.as_ref().filter(|p| !p.is_empty()) {
        let (colr, _) = prop_colr(&crate::meta::Colr::Icc(icc.clone()))?;
        entry_properties.push(colr.clone());
        props.push((colr, false));
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
    let duration_of = |i: usize| {
        durations
            .and_then(|d| d.get(i).copied())
            .unwrap_or(opts.frame_duration)
    };
    for (i, (data, sync)) in samples.into_iter().enumerate() {
        writer.push_sample(data, duration_of(i), sync);
    }
    if has_alpha {
        // The alpha auxiliary track: one monochrome, full-range KEY
        // frame per picture sample at the picture's depth and coded
        // extents (av1-avif §4.1), time-parallel by construction (the
        // same durations).
        let coded = code_in_parallel(frames.len(), workers, &|i| {
            let alpha = frames[i]
                .alpha(0, 0, w, h)?
                .ok_or_else(|| Error::invalid("avif sequence: alpha plane vanished"))?;
            encode_coded_item(
                pw,
                ph,
                bit_depth,
                oxideav_av1::encoder::yuv_frame::ChromaFormat::Monochrome,
                alpha,
                Vec::new(),
                Vec::new(),
                opts.alpha_q_idx,
                true,
                None,
                tuning,
                "sequence alpha frame",
            )
        })?;
        let mut alpha_config = None;
        let mut alpha_samples = Vec::with_capacity(coded.len());
        for (i, item) in coded.into_iter().enumerate() {
            if alpha_config.is_none() {
                alpha_config = Some(prop_av1c(&item.av1c, "sequence alpha ")?.0);
            }
            alpha_samples.push(oxideav_heif::SequenceSample::new(
                item.payload,
                duration_of(i),
                true,
            ));
        }
        writer.alpha = Some(oxideav_heif::SequenceAlphaTrack::new(
            *b"av01",
            alpha_config.expect("at least one alpha sample"),
            pw as u16,
            ph as u16,
            crate::alpha::ALPHA_URN_PREFIX.to_string(),
            alpha_samples,
            Vec::new(),
        ));
    }
    if let Some(count) = opts.loop_count {
        writer = writer.with_looping(match count {
            0 => oxideav_heif::sequence::LoopBehaviour::Forever,
            n => oxideav_heif::sequence::LoopBehaviour::Times(u64::from(n)),
        });
    }
    // Exif (the item body carries the 4-byte offset word; the track
    // `meta` takes the payload from the TIFF header on) and XMP of the
    // first frame describe the whole sequence (HEIF §8.3).
    if let Some(body) = metadata.exif.as_ref().filter(|b| b.len() > 4) {
        let offset = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
        if let Some(tiff) = body.get(4 + offset..).filter(|t| !t.is_empty()) {
            writer = writer.with_exif(Some(tiff.to_vec()));
        }
    }
    if let Some(xmp) = metadata.xmp.as_ref().filter(|x| !x.is_empty()) {
        writer = writer.with_xmp(Some(xmp.clone()));
    }
    Ok(writer.write_to_vec()?)
}
