//! Derived-image descriptors and entity grouping — HEIF §6.6.2 + §9.4,
//! plus AV1-AVIF v1.2.0 §4.2.3 sample-transform derivations.
//!
//! AVIF restricts derived-image carriage to the `grid` form (av1-avif
//! §4.2), but a reader that walks AVIF / HEIF files in the wild will
//! encounter HEIF features layered on top of an AVIF brand:
//!
//! * **`iovl` image-overlay derivations** (HEIF §6.6.2.2): one or more
//!   source images placed at signed `(x, y)` offsets on a fixed canvas
//!   with an RGBA fill colour.
//! * **`iden` identity derivations** (HEIF §6.6.2.1): the source image
//!   passed through unchanged, useful when transformative properties
//!   on the derivation differ from those on the source.
//! * **`sato` sample-transform derivations** (av1-avif v1.2.0 §4.2.3):
//!   a postfix-notation expression of integer operators and operands
//!   evaluated per sample to combine pixels from one or more input
//!   image items. See [`SampleTransform`].
//! * **Entity grouping** (HEIF §9.4): `grpl` containing one or more
//!   `EntityToGroupBox` per grouping type. The common groupings are
//!   `altr` (alternates), `ster` (stereo pair), `eqiv` (timeline
//!   equivalence to a track sample).
//!
//! All parsers here operate on raw box payload bytes — they're
//! independent of the [`crate::parser`] file walker and the
//! [`crate::meta`] item-property pipeline, so a caller can apply them
//! to any byte range that follows the documented layout. The
//! [`crate::parser::AvifHeader`] walker now exposes a `grpl` slice
//! through [`crate::meta::Meta::groups`] for callers that need to
//! enumerate AVIF/HEIF alternates without rebuilding the container
//! traversal.

use crate::box_parser::{b, iter_boxes, parse_full_box, read_u16, read_u32, type_str, BoxType};
use crate::error::{AvifError as Error, Result};

/// One placed image inside an `iovl` overlay descriptor (HEIF §6.6.2.2).
/// `horizontal_offset` + `vertical_offset` are signed pixel offsets from
/// the top-left corner of the canvas; per spec, source pixels with a
/// negative coordinate (or coordinates `>= output_width / output_height`)
/// are clipped out of the reconstructed image.
///
/// The actual source image item id isn't stored here — `iovl` payload
/// only carries the offsets; the source ids come from the parallel
/// `dimg` iref's `to_ids` list (in the same order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayEntry {
    pub horizontal_offset: i32,
    pub vertical_offset: i32,
}

/// Parsed `iovl` ImageOverlay descriptor (HEIF §6.6.2.2). Bottom-most
/// input image is `entries[0]`; the top-most is `entries[entries.len()-1]`.
///
/// `canvas_fill_value` is RGBA in sRGB (R, G, B, A) per spec; the A
/// channel runs 0 (transparent) to 65535 (opaque) linearly. RGB values
/// are also 16-bit, padded with zeros if the writer thought in 8-bit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageOverlay {
    pub canvas_fill_value: [u16; 4],
    pub output_width: u32,
    pub output_height: u32,
    pub entries: Vec<OverlayEntry>,
}

impl ImageOverlay {
    /// Parse `iovl` payload bytes against the `reference_count`
    /// argument. `reference_count` is the number of `dimg` `to_ids`
    /// for the overlay item — the `iovl` payload doesn't carry the
    /// count itself, so the caller is responsible for supplying it
    /// from the iref.
    ///
    /// Spec: ISO/IEC 23008-12 §6.6.2.2.2 (Syntax). The first byte is
    /// `version` (must be 0), then `flags`; `flags & 1` selects 32-bit
    /// over 16-bit field widths for `output_*` and `*_offset`.
    pub fn parse(payload: &[u8], reference_count: usize) -> Result<Self> {
        if payload.len() < 2 {
            return Err(Error::invalid("avif: iovl header too short"));
        }
        let version = payload[0];
        if version != 0 {
            return Err(Error::invalid(format!("avif: iovl version {version} != 0")));
        }
        let flags = payload[1];
        // FieldLength = ((flags & 1) + 1) * 16 bits = 2 or 4 bytes.
        let field_len = if flags & 1 != 0 { 4 } else { 2 };
        // Header: canvas_fill_value (4 × u16) + output_width + output_height
        let mut cursor = 2usize;
        let min = 2 + 4 * 2 + 2 * field_len + reference_count * 2 * field_len;
        if payload.len() < min {
            return Err(Error::invalid(format!(
                "avif: iovl too short ({} < {min})",
                payload.len()
            )));
        }
        let mut canvas = [0u16; 4];
        for slot in canvas.iter_mut() {
            *slot = read_u16(payload, cursor)?;
            cursor += 2;
        }
        let output_width = read_field_u32(payload, cursor, field_len)?;
        cursor += field_len;
        let output_height = read_field_u32(payload, cursor, field_len)?;
        cursor += field_len;
        let mut entries = Vec::with_capacity(reference_count);
        for _ in 0..reference_count {
            let h = read_field_i32(payload, cursor, field_len)?;
            cursor += field_len;
            let v = read_field_i32(payload, cursor, field_len)?;
            cursor += field_len;
            entries.push(OverlayEntry {
                horizontal_offset: h,
                vertical_offset: v,
            });
        }
        Ok(ImageOverlay {
            canvas_fill_value: canvas,
            output_width,
            output_height,
            entries,
        })
    }
}

fn read_field_u32(buf: &[u8], cursor: usize, field_len: usize) -> Result<u32> {
    match field_len {
        2 => Ok(read_u16(buf, cursor)? as u32),
        4 => read_u32(buf, cursor),
        n => Err(Error::invalid(format!("avif: iovl field length {n}"))),
    }
}

fn read_field_i32(buf: &[u8], cursor: usize, field_len: usize) -> Result<i32> {
    match field_len {
        2 => Ok(read_u16(buf, cursor)? as i16 as i32),
        4 => Ok(read_u32(buf, cursor)? as i32),
        n => Err(Error::invalid(format!("avif: iovl field length {n}"))),
    }
}

/// One `EntityToGroupBox` entry (HEIF / ISOBMFF §8.15.3 / 23008-12 §9.4.3).
/// `grouping_type` is a four-CC declaring the relationship between the
/// listed entity ids: `altr` (alternates), `ster` (stereo pair), `eqiv`
/// (equivalence to a track sample), and others.
///
/// `entity_ids` are conventionally `item_ID` values from the same `meta`
/// (file-level `grpl` references file-level items; per §9.4.1). When a
/// grouping mixes items and tracks, the resolver chooses based on which
/// id matches — that's a caller-side concern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityGroup {
    pub grouping_type: BoxType,
    pub group_id: u32,
    pub entity_ids: Vec<u32>,
}

impl EntityGroup {
    /// True when the grouping type signals stereo pair (HEIF §9.4.3.1).
    /// Reader convention: `entity_ids[0]` is the left view,
    /// `entity_ids[1]` is the right view.
    pub fn is_stereo_pair(&self) -> bool {
        &self.grouping_type == b"ster"
    }

    /// True when the grouping type signals an alternate set
    /// (HEIF §9.4.3.1) — the reader picks one of `entity_ids` and
    /// discards the others.
    pub fn is_alternates(&self) -> bool {
        &self.grouping_type == b"altr"
    }

    /// True when the grouping type signals timeline equivalence to a
    /// track sample (HEIF §6.8.1).
    pub fn is_equivalence(&self) -> bool {
        &self.grouping_type == b"eqiv"
    }

    /// True when the grouping type signals a set of images captured in
    /// order to create a panorama (HEIF §6.8.8.1), listed in
    /// increasing panorama order. The §6.5.27 `pano` item property
    /// describing the panorama direction `should` only be associated
    /// with an entity group of this type (§6.5.27.1).
    pub fn is_panorama(&self) -> bool {
        &self.grouping_type == b"pano"
    }
}

/// Parse a `GroupsListBox` (`grpl`) payload into its set of entity
/// groups. Spec: ISO/IEC 23008-12 §9.4.2 (file-level grouping).
///
/// `grpl` itself is a plain Box containing one or more `EntityToGroupBox`
/// children, each a FullBox keyed by `grouping_type` four-CC.
pub fn parse_grpl(payload: &[u8]) -> Result<Vec<EntityGroup>> {
    let mut out = Vec::new();
    for hdr in iter_boxes(payload) {
        let hdr = hdr?;
        let child = &payload[hdr.payload_start..hdr.end()];
        let (_version, _flags, body) = parse_full_box(child)?;
        if body.len() < 8 {
            return Err(Error::invalid(format!(
                "avif: EntityToGroupBox '{}' body too short ({} < 8)",
                type_str(&hdr.box_type),
                body.len()
            )));
        }
        let group_id = read_u32(body, 0)?;
        let num_entities = read_u32(body, 4)? as usize;
        let need = 8 + num_entities * 4;
        if body.len() < need {
            return Err(Error::invalid(format!(
                "avif: EntityToGroupBox '{}' truncated entity list ({} < {need})",
                type_str(&hdr.box_type),
                body.len()
            )));
        }
        let mut entity_ids = Vec::with_capacity(num_entities);
        for i in 0..num_entities {
            entity_ids.push(read_u32(body, 8 + i * 4)?);
        }
        out.push(EntityGroup {
            grouping_type: hdr.box_type,
            group_id,
            entity_ids,
        });
    }
    Ok(out)
}

/// Result of a `mif1` brand compliance audit (HEIF §10.2.1.1).
///
/// A `mif1` file must contain a top-level `ftyp` + `meta` and its `meta`
/// must contain `hdlr`, `pitm`, `iinf` + `infe` entries, `iloc`, and
/// `iprp`. The audit is informational: AVIF files in the wild ship
/// `mif1` as a compatible brand without strict compliance (e.g. ones
/// emitted by ImageMagick), and our reader still accepts them. The
/// validator exists so callers that want to enforce strict-mif1 mode
/// can.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Mif1Compliance {
    pub has_hdlr: bool,
    pub has_pitm: bool,
    pub has_iinf: bool,
    pub has_iloc: bool,
    pub has_iprp: bool,
    /// Number of `infe` entries inside `iinf`. mif1 requires at least
    /// one image item (the primary).
    pub infe_count: usize,
    /// Brand carries `mif1` in major_brand or compatible_brands.
    pub claims_mif1: bool,
}

impl Mif1Compliance {
    /// True when every mandatory mif1 reader-side box is present.
    /// Strict spec interpretation per §10.2.1.1 table — does not include
    /// the optional `iref` / `idat` / `iprp` of §10.2.1.2 entries that
    /// are reader-side suggestions only.
    pub fn is_compliant(&self) -> bool {
        self.has_hdlr
            && self.has_pitm
            && self.has_iinf
            && self.has_iloc
            && self.has_iprp
            && self.infe_count > 0
    }

    /// A human-friendly list of missing required boxes, useful for
    /// diagnostics. Returns an empty list when [`Self::is_compliant`].
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.has_hdlr {
            out.push("hdlr");
        }
        if !self.has_pitm {
            out.push("pitm");
        }
        if !self.has_iinf {
            out.push("iinf");
        }
        if self.infe_count == 0 {
            out.push("infe");
        }
        if !self.has_iloc {
            out.push("iloc");
        }
        if !self.has_iprp {
            out.push("iprp");
        }
        out
    }
}

// ---------------------------------------------------------------------------
// AV1-AVIF v1.2.0 §4.2.3 — Sample Transform Derived Image Item (`sato`)
// ---------------------------------------------------------------------------

/// A single token in a [`SampleTransform`] expression. The encoding is
/// the raw 8-bit token value from the bitstream (av1-avif §4.2.3.2,
/// Table 2). The variant carries any decoded payload (`Constant` holds
/// the signed literal extracted from the stream; every other variant is
/// a no-payload tag).
///
/// Token-value ranges (spec Table 2):
///
/// * `0` — `Constant(value)`. The constant is a signed integer of
///   `2^(bit_depth+3)` bits read from the stream (so 8, 16, 32 or
///   64 bits keyed by [`SampleTransform::bit_depth`]). The decoded value
///   is sign-extended to `i64` for in-memory storage; it is pushed to
///   the stack at the [`SampleTransform::num_bits`] intermediate
///   precision when evaluated.
/// * `1..=32` — `Sample(token)`. A 1-based index into the parallel
///   `dimg` iref's `to_ids` list. The actual sample value comes from
///   the named input image item at the same spatial coordinates and
///   channel as the output sample being evaluated.
/// * `33..=63` — reserved. Readers shall reject (av1-avif §4.2.3.3:
///   "Readers shall ignore a Sample Transform Derived Image Item with
///   a reserved token value"). Surfaced as [`Token::Reserved`] from
///   [`SampleTransform::parse_relaxed`] for diagnostics; the strict
///   [`SampleTransform::parse`] errors out instead.
/// * `64..=67` — `Unary(op)`. One stack pop, one stack push.
///   * `64` — negation (`-L`)
///   * `65` — absolute value (`|L|`)
///   * `66` — bitwise not (`¬L`)
///   * `67` — `bsr` (0-based index of the MSB of `L` when `L > 0`,
///     else `0`)
/// * `68..=127` — reserved.
/// * `128..=137` — `Binary(op)`. Two stack pops (right first, then
///   left), one push.
///   * `128` — sum (`L + R`)
///   * `129` — difference (`L - R`)
///   * `130` — product (`L * R`)
///   * `131` — quotient (`L / R` truncated toward zero; `L` if
///     `R == 0`)
///   * `132` — bitwise and (`L ∧ R`)
///   * `133` — bitwise or (`L ∨ R`)
///   * `134` — bitwise xor (`L ⊕ R`)
///   * `135` — pow (`L^R` truncated; `0` if `L == 0`)
///   * `136` — min
///   * `137` — max
/// * `138..=255` — reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// A literal signed constant pushed to the stack.
    Constant(i64),
    /// Push the sample from the `n`th input image item (1-based, so
    /// `1..=32`).
    Sample(u8),
    /// Unary operator (token value `64..=67`). Carries the raw token
    /// byte for round-trip and error reporting.
    Unary(u8),
    /// Binary operator (token value `128..=137`). Carries the raw token
    /// byte.
    Binary(u8),
    /// Reserved token value (`33..=63`, `68..=127`, `138..=255`).
    /// Returned only by [`SampleTransform::parse_relaxed`]; the strict
    /// parser refuses these per av1-avif §4.2.3.3.
    Reserved(u8),
}

impl Token {
    /// True for [`Self::Constant`] and [`Self::Sample`] — tokens that
    /// produce an operand without consuming any.
    pub fn is_operand(&self) -> bool {
        matches!(self, Token::Constant(_) | Token::Sample(_))
    }

    /// True for [`Self::Unary`] — one input pop, one push.
    pub fn is_unary(&self) -> bool {
        matches!(self, Token::Unary(_))
    }

    /// True for [`Self::Binary`] — two input pops, one push.
    pub fn is_binary(&self) -> bool {
        matches!(self, Token::Binary(_))
    }
}

/// Parsed `sato` Sample Transform Derived Image Item descriptor
/// (av1-avif v1.2.0 §4.2.3). The wire format is one header byte
/// (`version:2 | reserved:4 | bit_depth:2`), then `token_count: u8`,
/// then `token_count` tokens (each one byte plus an optional
/// constant-value payload).
///
/// The reconstructed image's samples come from evaluating this
/// expression once per channel per `(x, y)` coordinate, drawing input
/// samples from the input image items named in the parallel `dimg`
/// iref's `to_ids` list (`reference_count` items in declaration order).
/// The result is the single value left on the stack after the last
/// token; it is clamped to fit the reconstructed item's
/// `PixelInformationProperty` bit depth.
///
/// Use [`Self::parse`] to decode the descriptor with strict
/// spec-conformance — reserved token values, version mismatches, and
/// stack-discipline violations all error. The descriptor stores the
/// fully decoded token list so callers can either evaluate the
/// expression themselves (via [`Self::evaluate`] on a per-sample input
/// vector) or hand the parsed structure to a future composition layer
/// when an AV1 decoder is available to produce the input items.
///
/// Caveat: composition is not implemented in oxideav yet — the
/// `oxideav-av1` decoder is the bottleneck (see crate README). This
/// parser unblocks structural inspection, validation, and any future
/// composition work that lands on top.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleTransform {
    /// `version` field. Spec mandates `0`; readers shall ignore items
    /// with an unrecognised version. [`Self::parse`] errors on any
    /// non-zero version (callers that prefer "ignore" semantics can
    /// match on the error and skip the item).
    pub version: u8,
    /// `bit_depth` field — selector for the intermediate precision.
    /// `0..=3` map to 8 / 16 / 32 / 64 bits via [`Self::num_bits`].
    /// [`Self::parse`] errors on any value outside that range (the
    /// wire format reserves only 2 bits so a strict reader can never
    /// observe an out-of-range value, but a relaxed parser fed garbled
    /// bytes might).
    pub bit_depth: u8,
    /// Decoded token list. The first token's evaluation happens first.
    pub tokens: Vec<Token>,
}

impl SampleTransform {
    /// Intermediate bit depth keyed by [`Self::bit_depth`] (av1-avif
    /// Table 1). `0 → 8`, `1 → 16`, `2 → 32`, `3 → 64`.
    pub fn num_bits(&self) -> u32 {
        bit_depth_to_num_bits(self.bit_depth)
    }

    /// Minimum value representable at [`Self::num_bits`] precision
    /// (`-2^(num_bits-1)`). Computed underflows clamp to this value
    /// per spec.
    pub fn min_value(&self) -> i64 {
        let n = self.num_bits();
        if n >= 64 {
            i64::MIN
        } else {
            -(1i64 << (n - 1))
        }
    }

    /// Maximum value representable at [`Self::num_bits`] precision
    /// (`2^(num_bits-1) - 1`). Computed overflows clamp to this value
    /// per spec.
    pub fn max_value(&self) -> i64 {
        let n = self.num_bits();
        if n >= 64 {
            i64::MAX
        } else {
            (1i64 << (n - 1)) - 1
        }
    }

    /// Parse a `sato` payload strictly per av1-avif v1.2.0 §4.2.3.2 /
    /// §4.2.3.4:
    ///
    /// * `version` must be `0`.
    /// * `token_count` must be `>= 1`.
    /// * Each sample-operand token (`1..=token_count_max`) must be
    ///   `<= reference_count` per §4.2.3.4. Pass the `dimg` iref's
    ///   reference count for the item.
    /// * Reserved token values cause an error.
    /// * The expression must evaluate without stack underflow and leave
    ///   exactly one element on the stack.
    pub fn parse(payload: &[u8], reference_count: u32) -> Result<Self> {
        let st = Self::parse_relaxed(payload)?;
        if st.version != 0 {
            return Err(Error::invalid(format!(
                "avif: sato version {} != 0",
                st.version
            )));
        }
        // bit_depth is 2 bits on the wire so always 0..=3, but
        // parse_relaxed accepts whatever it was decoded to. Re-check.
        if st.bit_depth > 3 {
            return Err(Error::invalid(format!(
                "avif: sato bit_depth {} out of 0..=3",
                st.bit_depth
            )));
        }
        // Reject reserved tokens.
        for (i, t) in st.tokens.iter().enumerate() {
            if let Token::Reserved(raw) = *t {
                return Err(Error::invalid(format!(
                    "avif: sato token[{i}] = {raw} is reserved"
                )));
            }
        }
        // Validate every sample reference fits inside reference_count.
        // Spec §4.2.3.4: `token shall be at most reference_count` when
        // 1 <= token <= 32.
        for (i, t) in st.tokens.iter().enumerate() {
            if let Token::Sample(n) = *t {
                if u32::from(n) > reference_count {
                    return Err(Error::invalid(format!(
                        "avif: sato token[{i}] sample index {n} > reference_count {reference_count}"
                    )));
                }
            }
        }
        // Validate stack discipline without evaluating actual values.
        let mut depth: i64 = 0;
        for (i, t) in st.tokens.iter().enumerate() {
            match t {
                Token::Constant(_) | Token::Sample(_) => depth += 1,
                Token::Unary(_) => {
                    if depth < 1 {
                        return Err(Error::invalid(format!(
                            "avif: sato token[{i}] unary op underflows stack"
                        )));
                    }
                    // Pop one, push one: net 0.
                }
                Token::Binary(_) => {
                    if depth < 2 {
                        return Err(Error::invalid(format!(
                            "avif: sato token[{i}] binary op underflows stack"
                        )));
                    }
                    depth -= 1; // Two pops, one push: net -1.
                }
                Token::Reserved(_) => unreachable!(),
            }
        }
        if depth != 1 {
            return Err(Error::invalid(format!(
                "avif: sato expression leaves {depth} elements on stack, expected 1"
            )));
        }
        Ok(st)
    }

    /// Parse a `sato` payload structurally without rejecting reserved
    /// tokens or unrecognised versions. Useful for diagnostic dumps of
    /// experimental files. Stack discipline is still enforced (a
    /// malformed expression can't be round-tripped).
    pub fn parse_relaxed(payload: &[u8]) -> Result<Self> {
        if payload.len() < 2 {
            return Err(Error::invalid("avif: sato header too short"));
        }
        let header = payload[0];
        let version = (header >> 6) & 0x03;
        // bits 5..=2 are reserved (spec says ignored)
        let bit_depth = header & 0x03;
        let token_count = payload[1] as usize;
        if token_count == 0 {
            return Err(Error::invalid("avif: sato token_count = 0"));
        }
        let num_bits = bit_depth_to_num_bits(bit_depth);
        let const_bytes = (num_bits / 8) as usize; // 1, 2, 4, or 8
        let mut tokens = Vec::with_capacity(token_count);
        let mut cursor = 2usize;
        for i in 0..token_count {
            if cursor >= payload.len() {
                return Err(Error::invalid(format!(
                    "avif: sato truncated before token {i} (cursor {cursor} >= {})",
                    payload.len()
                )));
            }
            let raw = payload[cursor];
            cursor += 1;
            let token = if raw == 0 {
                let end = cursor
                    .checked_add(const_bytes)
                    .ok_or_else(|| Error::invalid("avif: sato constant offset overflow"))?;
                if end > payload.len() {
                    return Err(Error::invalid(format!(
                        "avif: sato truncated constant payload for token {i} (need {const_bytes} bytes at {cursor})"
                    )));
                }
                let value = read_sint_be(&payload[cursor..end])?;
                cursor = end;
                Token::Constant(value)
            } else if raw <= 32 {
                Token::Sample(raw)
            } else if (64..=67).contains(&raw) {
                Token::Unary(raw)
            } else if (128..=137).contains(&raw) {
                Token::Binary(raw)
            } else {
                Token::Reserved(raw)
            };
            tokens.push(token);
        }
        Ok(SampleTransform {
            version,
            bit_depth,
            tokens,
        })
    }

    /// Evaluate this expression for one output sample, given the input
    /// sample values from the parallel `dimg` iref's `to_ids` list (in
    /// the same order). Returns the single value left on the stack,
    /// already clamped to the intermediate precision via the
    /// underflow / overflow rules of av1-avif §4.2.3.3.
    ///
    /// `inputs[i]` is the sample from input image item `i + 1` (the
    /// spec's `token` values for sample operands are 1-based; this
    /// helper translates them to 0-based vector indices).
    ///
    /// The caller is responsible for the final clamp into the
    /// reconstructed item's `PixelInformationProperty` bit depth, since
    /// that depth lives outside the `sato` descriptor itself.
    ///
    /// Returns an error if the expression dereferences an out-of-range
    /// sample or trips a stack underflow that wasn't caught by
    /// [`Self::parse`]'s validation (the only path is a caller passing
    /// `inputs.len() < max_sample_index_used` or a `parse_relaxed`
    /// expression that bypassed validation).
    pub fn evaluate(&self, inputs: &[i64]) -> Result<i64> {
        let min = self.min_value();
        let max = self.max_value();
        let clamp = |v: i64| v.clamp(min, max);
        let mut stack: Vec<i64> = Vec::with_capacity(self.tokens.len());
        for (i, t) in self.tokens.iter().enumerate() {
            match t {
                Token::Constant(value) => stack.push(clamp(*value)),
                Token::Sample(n) => {
                    let idx = usize::from(*n).saturating_sub(1);
                    let v = *inputs.get(idx).ok_or_else(|| {
                        Error::invalid(format!(
                            "avif: sato token[{i}] sample {n} out of range (inputs.len()={})",
                            inputs.len()
                        ))
                    })?;
                    stack.push(clamp(v));
                }
                Token::Unary(raw) => {
                    let l = stack
                        .pop()
                        .ok_or_else(|| Error::invalid("avif: sato unary stack underflow"))?;
                    let r = match *raw {
                        64 => l.checked_neg().unwrap_or(i64::MAX), // negation
                        65 => l.checked_abs().unwrap_or(i64::MAX), // abs
                        66 => !l,                                  // bitwise not
                        67 => {
                            // bsr: 0-based index of MSB if l > 0, else 0.
                            if l > 0 {
                                (63 - (l as u64).leading_zeros()) as i64
                            } else {
                                0
                            }
                        }
                        _ => {
                            return Err(Error::invalid(format!(
                                "avif: sato unary token {raw} not implemented",
                            )));
                        }
                    };
                    stack.push(clamp(r));
                }
                Token::Binary(raw) => {
                    let r = stack
                        .pop()
                        .ok_or_else(|| Error::invalid("avif: sato binary right underflow"))?;
                    let l = stack
                        .pop()
                        .ok_or_else(|| Error::invalid("avif: sato binary left underflow"))?;
                    let out = match *raw {
                        // Spec §4.2.3.3: results that underflow / overflow
                        // the intermediate bit depth are replaced by
                        // -2^(num_bits-1) / 2^(num_bits-1)-1. We compute
                        // at i64 saturation and rely on the final
                        // `clamp` call below to narrow into num_bits.
                        128 => l.saturating_add(r),
                        129 => l.saturating_sub(r),
                        130 => l.saturating_mul(r),
                        131 => {
                            if r == 0 {
                                l
                            } else {
                                // Truncate toward zero, which is Rust's
                                // default for `/` on signed integers.
                                // `i64::MIN / -1` overflows; saturate.
                                l.checked_div(r).unwrap_or(i64::MAX)
                            }
                        }
                        132 => l & r,
                        133 => l | r,
                        134 => l ^ r,
                        135 => {
                            if l == 0 {
                                0
                            } else {
                                pow_truncated(l, r)
                            }
                        }
                        136 => l.min(r),
                        137 => l.max(r),
                        _ => {
                            return Err(Error::invalid(format!(
                                "avif: sato binary token {raw} not implemented",
                            )));
                        }
                    };
                    stack.push(clamp(out));
                }
                Token::Reserved(raw) => {
                    return Err(Error::invalid(format!(
                        "avif: sato token[{i}] = {raw} reserved (cannot evaluate)",
                    )));
                }
            }
        }
        if stack.len() != 1 {
            return Err(Error::invalid(format!(
                "avif: sato expression leaves {} elements on stack, expected 1",
                stack.len()
            )));
        }
        Ok(stack[0])
    }
}

// `sato` four-CC lives in [`crate::meta::ITEM_TYPE_SATO`] (alongside
// `iovl`, `iden`, `tmap`, etc.) to keep all item-type constants on the
// `Meta` surface.

/// Map the 2-bit `bit_depth` selector to its intermediate bit depth
/// (av1-avif Table 1).
fn bit_depth_to_num_bits(bit_depth: u8) -> u32 {
    match bit_depth & 0x03 {
        0 => 8,
        1 => 16,
        2 => 32,
        3 => 64,
        _ => unreachable!(),
    }
}

/// Read a signed big-endian integer of `bytes.len()` bytes (must be
/// 1, 2, 4 or 8) and sign-extend to `i64`.
fn read_sint_be(bytes: &[u8]) -> Result<i64> {
    Ok(match bytes.len() {
        1 => bytes[0] as i8 as i64,
        2 => i16::from_be_bytes([bytes[0], bytes[1]]) as i64,
        4 => i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64,
        8 => i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]),
        n => {
            return Err(Error::invalid(format!(
                "avif: sato constant width {n} not in {{1,2,4,8}}"
            )))
        }
    })
}

/// Truncated integer exponentiation per av1-avif Table 2 row 135.
/// `L^R` with the result truncated; the spec defines `0` for the
/// `L == 0` case via the row's own piecewise rule (handled by the
/// caller). Negative `R` truncates to `0` for `|L| > 1` (1/L^|R| < 1)
/// and to `L` itself for `L == ±1` (since `1^anything = 1` and
/// `(-1)^anything = ±1`). The result is saturated to `i64`'s range
/// when intermediate computations would overflow.
fn pow_truncated(base: i64, exp: i64) -> i64 {
    if exp < 0 {
        return match base.abs() {
            1 => {
                // 1^anything = 1; (-1)^anything alternates so just
                // compute by parity.
                if base == 1 || exp % 2 == 0 {
                    1
                } else {
                    -1
                }
            }
            _ => 0,
        };
    }
    if exp == 0 {
        return 1;
    }
    let mut result: i64 = 1;
    let mut base_acc = base;
    let mut e = exp as u64;
    while e > 0 {
        if e & 1 == 1 {
            result = match result.checked_mul(base_acc) {
                Some(v) => v,
                None => {
                    return if (result >= 0) == (base_acc >= 0) {
                        i64::MAX
                    } else {
                        i64::MIN
                    };
                }
            };
        }
        e >>= 1;
        if e > 0 {
            base_acc = match base_acc.checked_mul(base_acc) {
                Some(v) => v,
                None => {
                    return if base_acc >= 0 || (exp & 1 == 0) {
                        i64::MAX
                    } else {
                        i64::MIN
                    };
                }
            };
        }
    }
    result
}

// ---------------------------------------------------------------------------
// AV1-AVIF v1.2.0 §4.2.2 — Tone Map Derived Image Item (`tmap`) compliance
// ---------------------------------------------------------------------------

/// Result of an av1-avif §4.2.2 audit on a single `'tmap'` Tone Map
/// Derived Image Item carried by the file.
///
/// The `tmap` item type itself is registered by the HEIF / MIAF family;
/// the descriptor *body* the item points at via its `iloc` is the ISO
/// 21496-1 gain map metadata payload, parsed by
/// [`GainMapMetadata::parse`]. This audit covers the two file-shape
/// `should` constraints av1-avif §4.2.2 imposes *independently* of that
/// body:
///
/// 1. **`altr` grouping.** The base image item (i.e. the input the
///    tmap item references via `'dimg'`) and the `tmap` item should be
///    grouped together by an `'altr'` entity group, so legacy readers
///    that don't understand `tmap` still pick a valid alternate.
/// 2. **Hidden gain map.** When the tmap derivation references a "gain
///    map" input image item (the additional image input layered onto
///    the base via tone mapping), that input should be a HEIF
///    [hidden image item](crate::meta::ItemInfo::is_hidden) so a
///    legacy reader never surfaces it as a primary picture.
///
/// Both are `should`, not `shall`, so a fail does not invalidate the
/// file — it is purely informational for callers that want strict
/// av1-avif §4.2.2 mode. [`Self::is_compliant`] reports the AND of
/// both signals; [`Self::missing`] lists which checks failed.
///
/// `tmap` items carry their inputs in `'dimg'` iref entries whose
/// `from_item_ID` is the `tmap` item id (per av1-avif §4.2.3.1
/// SingleItemTypeReferenceBox conventions, the same shape `'sato'`
/// uses). Convention in HEIF gain-map layouts is `to_ids[0]` =
/// base image item, `to_ids[1..]` = gain map(s); we treat
/// `to_ids[0]` as the base and every subsequent entry as a gain map.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ToneMapCompliance {
    /// The `'tmap'` item id this audit describes.
    pub tmap_item_id: u32,
    /// The base image item id (input `0` of the tmap's `dimg`), or
    /// `None` when the tmap has no `dimg` iref / no inputs at all.
    pub base_item_id: Option<u32>,
    /// Every additional input image item id (`to_ids[1..]`) — these
    /// are the gain map inputs the spec wants hidden. Empty when the
    /// tmap has exactly one input (base only).
    pub gain_map_item_ids: Vec<u32>,
    /// True when *some* `'altr'` entity group pairs `tmap_item_id`
    /// with `base_item_id` (av1-avif §4.2.2 first `should`).
    /// False when no `grpl` is present, when no `altr` group lists
    /// both ids, or when `base_item_id` is `None`.
    pub paired_in_altr: bool,
    /// True when every id in `gain_map_item_ids` is marked hidden
    /// (`infe` flags low bit set; HEIF §6.4.2). Trivially true when
    /// `gain_map_item_ids` is empty.
    pub gain_maps_hidden: bool,
}

impl ToneMapCompliance {
    /// True when both `should`s pass — there exists an `altr` group
    /// pairing the tmap with its base item, and every gain-map input
    /// is hidden.
    pub fn is_compliant(&self) -> bool {
        self.paired_in_altr && self.gain_maps_hidden
    }

    /// Human-readable list of failed checks. Returns an empty vector
    /// when [`Self::is_compliant`].
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.paired_in_altr {
            out.push("altr-pairs-base-and-tmap");
        }
        if !self.gain_maps_hidden {
            out.push("gain-map-hidden");
        }
        out
    }
}

/// Audit every `'tmap'` item carried in `meta` against the two av1-avif
/// §4.2.2 `should` constraints. Returns one [`ToneMapCompliance`]
/// record per tmap item, in `iinf` declaration order. The returned
/// vector is empty when the file ships no tmap items.
///
/// Spec: av1-avif v1.2.0 §4.2.2 (Tone Map Derived Image Item) — the
/// av1-avif file-shape clauses only. The `tmap` descriptor body itself
/// (the ISO 21496-1 gain map metadata payload pointed at by the item's
/// `iloc`) is parsed separately by [`GainMapMetadata::parse`].
pub fn audit_tone_map(meta: &crate::meta::Meta) -> Vec<ToneMapCompliance> {
    let tmap_type = crate::meta::ITEM_TYPE_TMAP;
    let dimg = b(b"dimg");
    let groups = meta.groups().unwrap_or_default();
    meta.item_ids_of_type(&tmap_type)
        .into_iter()
        .map(|tmap_id| audit_one_tone_map(meta, &groups, &dimg, tmap_id))
        .collect()
}

fn audit_one_tone_map(
    meta: &crate::meta::Meta,
    groups: &[EntityGroup],
    dimg: &BoxType,
    tmap_id: u32,
) -> ToneMapCompliance {
    let inputs = meta.iref_targets(dimg, tmap_id);
    let base_item_id = inputs.first().copied();
    let gain_map_item_ids: Vec<u32> = inputs.iter().skip(1).copied().collect();

    // §4.2.2 first `should`: an `altr` group contains both the tmap and
    // its base image item.
    let paired_in_altr = match base_item_id {
        Some(base) => groups.iter().any(|g| {
            g.is_alternates() && g.entity_ids.contains(&tmap_id) && g.entity_ids.contains(&base)
        }),
        None => false,
    };

    // §4.2.2 second `should`: every gain-map input image item is
    // hidden. Items missing from `iinf` (malformed iref) count as
    // not-hidden — they fail the audit rather than silently passing.
    let gain_maps_hidden = gain_map_item_ids
        .iter()
        .all(|id| meta.item_by_id(*id).is_some_and(|info| info.is_hidden()));

    ToneMapCompliance {
        tmap_item_id: tmap_id,
        base_item_id,
        gain_map_item_ids,
        paired_in_altr,
        gain_maps_hidden,
    }
}

// ---------------------------------------------------------------------------
// ISO 21496-1:2025 Annex C.2 — Gain map metadata binary payload
// ---------------------------------------------------------------------------

/// A signed rational (`numerator / denominator`) read from the gain map
/// metadata payload. The numerator is stored as a signed 32-bit integer
/// and the denominator as an unsigned 32-bit integer per ISO 21496-1
/// Annex C.2.2; the denominator of every rational field "shall not be 0".
///
/// Stored as raw integer components rather than a pre-divided float so
/// the parse stays lossless — callers that want the value compute
/// `numerator as f64 / denominator as f64` themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct GainMapRational {
    /// Signed numerator (the `int(32)` component).
    pub numerator: i32,
    /// Unsigned denominator (the `unsigned int(32)` component). The spec
    /// forbids `0`; [`GainMapMetadata::parse`] rejects any payload that
    /// carries a zero denominator.
    pub denominator: u32,
}

impl GainMapRational {
    /// The rational evaluated as an `f64`. The denominator is guaranteed
    /// non-zero by [`GainMapMetadata::parse`], so this never divides by
    /// zero on a value obtained through the parser.
    pub fn as_f64(&self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }
}

/// One per-channel gain map metadata record (ISO 21496-1 `GainMapChannel`,
/// Annex C.2.2). Each field is a signed/unsigned rational pair; the
/// per-component values are computed as `numerator / denominator`.
///
/// `gamma_numerator` and every `*_denominator` "shall not be 0" per Annex
/// C.2.3; [`GainMapMetadata::parse`] enforces those constraints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct GainMapChannel {
    /// Per-component gain map min value (signed rational; 5.2.5.2).
    pub gain_map_min: GainMapRational,
    /// Per-component gain map max value (signed rational; 5.2.5.3).
    pub gain_map_max: GainMapRational,
    /// Per-component gamma value (unsigned rational; 5.2.5.6). The
    /// numerator is constrained non-zero in addition to the denominator.
    pub gamma: GainMapRational,
    /// Per-component baseline offset constant (signed rational; 5.2.5.4).
    pub base_offset: GainMapRational,
    /// Per-component alternate offset constant (signed rational; 5.2.5.5).
    pub alternate_offset: GainMapRational,
}

impl GainMapChannel {
    /// Unnormalize a stored gain-map sample for this component into the
    /// log2 gain value `G`, applying ISO 21496-1 §6.2.1 Formula (1):
    ///
    /// ```text
    /// G = [max(G) − min(G)] × (Gnormalized ^ (1/γ)) + min(G)
    /// ```
    ///
    /// `normalized` is the stored, gamma-encoded sample in `[0, 1]`
    /// (`Gnormalized_γ` in the spec's notation — the value as carried in
    /// the gain-map image after the §A.3.4 gamma pre-compression). The
    /// `(·)^(1/γ)` term inverts that gamma, then the `[min, max]` range
    /// scaling inverts the §A.3.3 normalization, yielding `G` in log2
    /// (stops) space ready for [`GainMapMetadata::apply_component`].
    ///
    /// `min(G)`, `max(G)` and `γ` are this channel's [`Self::gain_map_min`],
    /// [`Self::gain_map_max`] and [`Self::gamma`]. The parser guarantees
    /// `γ > 0` (non-zero numerator and denominator) so `1/γ` is finite,
    /// and `max(G) ≥ min(G)` so the span is non-negative.
    ///
    /// The stored sample is clamped into `[0, 1]` before the gamma inverse
    /// because `x^(1/γ)` is not real for negative `x` when `1/γ` is
    /// non-integral; §A.3.3 defines `Gnormalized` on `[0, 1]` so an
    /// out-of-range input is a writer error and is saturated rather than
    /// allowed to produce a NaN.
    pub fn unnormalize_log2_gain(&self, normalized: f64) -> f64 {
        let min = self.gain_map_min.as_f64();
        let max = self.gain_map_max.as_f64();
        let gamma = self.gamma.as_f64();
        let g_norm = normalized.clamp(0.0, 1.0);
        // Invert the §A.3.4 gamma: Gnormalized = (Gnormalized_γ)^(1/γ).
        let degammad = g_norm.powf(1.0 / gamma);
        (max - min) * degammad + min
    }
}

/// Parsed ISO 21496-1:2025 gain map metadata payload (Annex C.2) — the
/// binary descriptor body carried by the AVIF / HEIF `'tmap'` (tone map)
/// derived image item.
///
/// av1-avif §4.2.2 (and HEIF) register the `'tmap'` item type and the
/// `altr`/hidden file-shape constraints audited by [`audit_tone_map`];
/// the *body* the item points at via its `iloc` is this structure,
/// specified by ISO 21496-1. Resolve the item bytes with
/// [`crate::inspect::item_payload_bytes`] (or
/// [`crate::parser::item_bytes_owned`]) and hand them to
/// [`GainMapMetadata::parse`].
///
/// The byte order is big-endian regardless of the container, and the
/// 1-bit `is_multichannel` / `use_base_colour_space` flags are read from
/// the most-significant bits of a single byte (Annex C.2.1). Per Annex
/// C.2.1 the structure may carry trailing padding or future optional
/// metadata after the recognised fields; the parser stops after the last
/// recognised field and ignores the remainder, so a longer payload is
/// not an error.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GainMapMetadata {
    /// `minimum_version` — the minimum version a parser must understand to
    /// apply the gain map. Annex C.2.3: this "shall be 0 in this version
    /// of the specification". A reader encountering an unrecognised value
    /// must ignore the payload and display the base image.
    pub minimum_version: u16,
    /// `writer_version` — the version the writer used; "shall be greater
    /// than or equal to `minimum_version`".
    pub writer_version: u16,
    /// `is_multichannel` — when true the per-channel record count is 3
    /// (channels in R, G, B order); when false it is 1.
    pub is_multichannel: bool,
    /// `use_base_colour_space` — true selects the baseline image colour
    /// primaries for the gain map application space; false selects the
    /// alternate image primaries (5.3.4).
    pub use_base_colour_space: bool,
    /// Baseline HDR headroom (unsigned rational; 5.2.6). Stored with its
    /// `int(32)` numerator widened to `i32` for a uniform rational type —
    /// the value is logically unsigned and the denominator is non-zero.
    pub base_hdr_headroom: GainMapRational,
    /// Alternate HDR headroom (unsigned rational; 5.2.7).
    pub alternate_hdr_headroom: GainMapRational,
    /// Per-channel metadata. Length is 3 when [`Self::is_multichannel`],
    /// else 1.
    pub channels: Vec<GainMapChannel>,
}

impl GainMapMetadata {
    /// Parse an ISO 21496-1 Annex C.2.2 `GainMapMetadata` payload from
    /// the raw `'tmap'` item bytes.
    ///
    /// Behaviour:
    ///
    /// * Returns [`AvifError::Unsupported`](crate::error::AvifError::Unsupported)
    ///   when `minimum_version != 0` — Annex C.2.3 requires such a reader
    ///   to ignore the payload, so the caller should fall back to the
    ///   base image rather than treat the bytes as malformed.
    /// * Returns [`AvifError::InvalidData`](crate::error::AvifError::InvalidData)
    ///   when the payload is truncated, when `writer_version <
    ///   minimum_version`, when any rational denominator (or the gamma
    ///   numerator) is `0`, when any channel's `gain_map_max <
    ///   gain_map_min` (§5.2.5.3 "shall be greater than or equal to"),
    ///   or when `alternate_hdr_headroom == base_hdr_headroom` (§5.2.7
    ///   "shall not be equal to") — all `shall`-level constraints from
    ///   §5.2 / Annex C.2.3.
    /// * Trailing bytes after the last recognised field are ignored
    ///   (Annex C.2.1 padding / future-optional-metadata rule).
    pub fn parse(payload: &[u8]) -> Result<Self> {
        // GainMapVersion: minimum_version(16) + writer_version(16).
        let minimum_version = read_u16(payload, 0)?;
        let writer_version = read_u16(payload, 2)?;

        if minimum_version != 0 {
            return Err(Error::unsupported(format!(
                "avif: gain map metadata minimum_version {minimum_version} not understood \
                 (ISO 21496-1 C.2.3 requires 0); caller should display the base image"
            )));
        }
        // C.2.3: writer_version shall be >= minimum_version.
        if writer_version < minimum_version {
            return Err(Error::invalid(format!(
                "avif: gain map metadata writer_version {writer_version} < minimum_version \
                 {minimum_version} (ISO 21496-1 C.2.3)"
            )));
        }

        // One flags byte: is_multichannel(1) | use_base_colour_space(1) |
        // reserved(6), MSB-first (C.2.1).
        let flags = *payload
            .get(4)
            .ok_or_else(|| Error::invalid("avif: gain map metadata truncated at flags byte"))?;
        let is_multichannel = flags & 0x80 != 0;
        let use_base_colour_space = flags & 0x40 != 0;
        let channel_count = if is_multichannel { 3 } else { 1 };

        // base/alternate HDR headroom: 4 × u32 starting at offset 5.
        let base_hdr_headroom = read_unsigned_rational(payload, 5)?;
        let alternate_hdr_headroom = read_unsigned_rational(payload, 13)?;
        // §5.2.7: "H_alternate shall not be equal to H_baseline". The
        // headroom values are unsigned rationals; compare via the
        // cross-multiplied i64 product so a different
        // (numerator, denominator) pair that reduces to the same value
        // is still flagged (e.g. 1/1 == 2/2). Denominators have already
        // been rejected when zero by `read_unsigned_rational`.
        if !rationals_differ(&base_hdr_headroom, &alternate_hdr_headroom) {
            return Err(Error::invalid(
                "avif: gain map metadata alternate_hdr_headroom equals base_hdr_headroom \
                 (ISO 21496-1 §5.2.7)",
            ));
        }

        // GainMapChannel[channel_count] follow, each 10 × 32-bit fields
        // (40 bytes): min, max, gamma, base_offset, alternate_offset.
        let mut channels = Vec::with_capacity(channel_count);
        let mut at = 21usize;
        for _ in 0..channel_count {
            let gain_map_min = read_signed_rational(payload, at)?;
            let gain_map_max = read_signed_rational(payload, at + 8)?;
            // §5.2.5.3: "For each component, max(G) shall be greater
            // than or equal to the min(G) value". Compare via the
            // cross-multiplied i64 product so the predicate is exact
            // for any (numerator, denominator) pair; denominators are
            // already non-zero (and positive, being unsigned), so the
            // product's sign is the sign of the difference.
            if !rational_ge(&gain_map_max, &gain_map_min) {
                return Err(Error::invalid(
                    "avif: gain map metadata gain_map_max < gain_map_min \
                     (ISO 21496-1 §5.2.5.3)",
                ));
            }
            let gamma = read_unsigned_rational(payload, at + 16)?;
            // C.2.3: gamma_numerator shall not be 0.
            if gamma.numerator == 0 {
                return Err(Error::invalid(
                    "avif: gain map metadata gamma_numerator is 0 (ISO 21496-1 C.2.3)",
                ));
            }
            let base_offset = read_signed_rational(payload, at + 24)?;
            let alternate_offset = read_signed_rational(payload, at + 32)?;
            channels.push(GainMapChannel {
                gain_map_min,
                gain_map_max,
                gamma,
                base_offset,
                alternate_offset,
            });
            at += 40;
        }

        Ok(GainMapMetadata {
            minimum_version,
            writer_version,
            is_multichannel,
            use_base_colour_space,
            base_hdr_headroom,
            alternate_hdr_headroom,
            channels,
        })
    }

    /// The per-channel record count implied by [`Self::is_multichannel`]
    /// (3 when multichannel, else 1). Always equal to `channels.len()`
    /// for a value obtained from [`Self::parse`].
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// The weighting factor `W` for a target HDR headroom, per ISO 21496-1
    /// §6.3 Formula (3):
    ///
    /// ```text
    /// W = sign(H_alternate − H_baseline) × clamp((H_target − H_baseline) /
    ///                                            (H_alternate − H_baseline), 0, 1)
    /// ```
    ///
    /// `W` scales how much of the gain map is applied (§6.3 NOTE 4): at
    /// `H_target == H_baseline` the gain map is not applied (`W == 0`),
    /// and at `H_target == H_alternate` it is fully applied (`W == ±1`,
    /// the sign following `sign(H_alternate − H_baseline)`). Targets
    /// outside `[H_baseline, H_alternate]` are clamped so `W` stays in
    /// `[−1, 1]`.
    ///
    /// `H_baseline` and `H_alternate` are [`Self::base_hdr_headroom`] and
    /// [`Self::alternate_hdr_headroom`]; the parser guarantees they are
    /// not equal (§5.2.7), so the denominator is non-zero.
    pub fn weight_factor(&self, target_headroom: f64) -> f64 {
        let h_base = self.base_hdr_headroom.as_f64();
        let h_alt = self.alternate_hdr_headroom.as_f64();
        let span = h_alt - h_base;
        // span != 0 is guaranteed by the §5.2.7 parse check.
        let frac = ((target_headroom - h_base) / span).clamp(0.0, 1.0);
        span.signum() * frac
    }

    /// The per-component metadata record that applies to RGB component
    /// `component` (`0 = R`, `1 = G`, `2 = B`), honouring the §5.2.5.1
    /// broadcast rule: a single-channel metadata record applies to all
    /// three colour components, while a three-channel record uses one
    /// value per component. Returns `None` only when `channels` is empty
    /// (never the case for a value from [`Self::parse`]) or `component`
    /// exceeds 2.
    fn channel_for(&self, component: usize) -> Option<&GainMapChannel> {
        if component > 2 {
            return None;
        }
        match self.channels.len() {
            // Achromatic / single per-component metadata: broadcast.
            1 => self.channels.first(),
            // Per-component metadata: index directly.
            _ => self.channels.get(component),
        }
    }

    /// Apply the gain map to one linear baseline colour component and
    /// recover the corresponding linear alternate component, per
    /// ISO 21496-1 §6.3 Formula (2):
    ///
    /// ```text
    /// Alternate = (Baseline + k_baseline) × 2^(W × G) − k_alternate
    /// ```
    ///
    /// where `G` is the unnormalized log2 gain for this component
    /// ([`GainMapChannel::unnormalize_log2_gain`] applied to the stored
    /// `normalized` sample) and `W` is the weighting factor for
    /// `target_headroom` ([`Self::weight_factor`]). `k_baseline` and
    /// `k_alternate` are the channel's [`GainMapChannel::base_offset`] and
    /// [`GainMapChannel::alternate_offset`].
    ///
    /// `component` is the RGB component index (`0 = R`, `1 = G`,
    /// `2 = B`); the metadata channel is selected via the §5.2.5.1
    /// broadcast rule (single-channel metadata applies to all three).
    /// `baseline` is the linear baseline sample (scaled so HDR reference
    /// white is `1.0`, per §A.2 / Annex B.2) and `normalized` is the
    /// stored gain-map sample in `[0, 1]` for the gain-map component that
    /// applies to this colour component — for an achromatic gain map
    /// (§6.3 NOTE 2) the same sample is supplied for all three colour
    /// components.
    ///
    /// Returns `None` when `component > 2` or `channels` is empty.
    pub fn apply_component(
        &self,
        baseline: f64,
        normalized: f64,
        target_headroom: f64,
        component: usize,
    ) -> Option<f64> {
        let ch = self.channel_for(component)?;
        let g = ch.unnormalize_log2_gain(normalized);
        let w = self.weight_factor(target_headroom);
        let k_base = ch.base_offset.as_f64();
        let k_alt = ch.alternate_offset.as_f64();
        Some((baseline + k_base) * (w * g).exp2() - k_alt)
    }

    /// Apply the gain map to a linear baseline RGB pixel and recover the
    /// linear alternate RGB pixel, per ISO 21496-1 §6.3 Formula (2)
    /// applied to each of the three colour components.
    ///
    /// `baseline` is the linear baseline `[R, G, B]` (HDR reference white
    /// `= 1.0`). `gain` is the stored gain-map sample(s): a 3-element
    /// array carries one sample per colour component, while broadcasting
    /// is the caller's job for an achromatic (single-component) gain-map
    /// image — pass the one decoded gain-map sample in all three slots
    /// (§6.3 NOTE 2). The per-component *metadata* broadcast (§5.2.5.1) is
    /// handled internally, so a single-channel-metadata file still applies
    /// correctly to all three colour components.
    ///
    /// Returns `None` when `channels` is empty (never the case for a value
    /// from [`Self::parse`]).
    pub fn apply_rgb(
        &self,
        baseline: [f64; 3],
        gain: [f64; 3],
        target_headroom: f64,
    ) -> Option<[f64; 3]> {
        Some([
            self.apply_component(baseline[0], gain[0], target_headroom, 0)?,
            self.apply_component(baseline[1], gain[1], target_headroom, 1)?,
            self.apply_component(baseline[2], gain[2], target_headroom, 2)?,
        ])
    }

    /// Apply the gain map across a whole linear baseline RGB image plane,
    /// recovering the linear alternate RGB plane (ISO 21496-1 §6.3
    /// applied pixel-by-pixel).
    ///
    /// `baseline` is `width × height × 3` linear samples, interleaved as
    /// `[R, G, B, R, G, B, …]` in row-major order (HDR reference white
    /// `= 1.0`). `gain` is the decoded gain-map plane: either `width ×
    /// height` samples (a single-component / achromatic gain map — the
    /// same sample is applied to each colour component per §6.3 NOTE 2),
    /// or `width × height × 3` interleaved samples (an RGB gain map).
    /// Gain samples are the stored, normalized `[0, 1]` values
    /// (`Gnormalized_γ`); the §6.2.1 unnormalization is performed
    /// internally.
    ///
    /// The output is a freshly allocated `width × height × 3` interleaved
    /// linear alternate RGB plane.
    ///
    /// Returns [`AvifError::InvalidData`](crate::error::AvifError::InvalidData)
    /// when:
    ///
    /// * `width × height` overflows `usize`, or `baseline.len()` is not
    ///   exactly `width × height × 3`;
    /// * `gain.len()` is neither `width × height` (achromatic) nor
    ///   `width × height × 3` (RGB);
    /// * `channels` is empty (never the case for a [`Self::parse`] value).
    ///
    /// The §6.2.2 resampling step is the **caller's** responsibility: this
    /// method requires the gain plane to already match the baseline
    /// dimensions (§6.2.2 NOTE 3 — the alternate dimensions equal the
    /// baseline dimensions). A gain map stored at a different resolution
    /// must be resampled to `width × height` before being passed here.
    pub fn apply_plane_rgb(
        &self,
        baseline: &[f64],
        gain: &[f64],
        width: u32,
        height: u32,
        target_headroom: f64,
    ) -> Result<Vec<f64>> {
        if self.channels.is_empty() {
            return Err(Error::invalid(
                "avif: gain map metadata has no channel records to apply",
            ));
        }
        let px = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| Error::invalid("avif: gain map plane dimensions overflow usize"))?;
        let rgb_len = px
            .checked_mul(3)
            .ok_or_else(|| Error::invalid("avif: gain map plane dimensions overflow usize"))?;
        if baseline.len() != rgb_len {
            return Err(Error::invalid(format!(
                "avif: baseline plane length {} != width*height*3 = {rgb_len}",
                baseline.len()
            )));
        }
        // Achromatic (one sample/pixel, broadcast to RGB) or interleaved
        // RGB gain plane (§6.3 NOTE 2 vs an RGB gain map).
        let achromatic = match gain.len() {
            n if n == px => true,
            n if n == rgb_len => false,
            other => {
                return Err(Error::invalid(format!(
                    "avif: gain plane length {other} is neither width*height = {px} \
                     (achromatic) nor width*height*3 = {rgb_len} (RGB)"
                )));
            }
        };

        // The weight factor depends only on the target headroom, so it is
        // constant across the whole plane — compute it once (§6.3
        // Formula 3).
        let w = self.weight_factor(target_headroom);

        let mut out = vec![0.0f64; rgb_len];
        for i in 0..px {
            for c in 0..3 {
                let base = baseline[i * 3 + c];
                let g_sample = if achromatic { gain[i] } else { gain[i * 3 + c] };
                // channel_for is Some for c in 0..3 with a non-empty
                // channels vec (guaranteed above).
                let ch = self
                    .channel_for(c)
                    .expect("channel_for is Some for c<3 with non-empty channels");
                let g = ch.unnormalize_log2_gain(g_sample);
                let k_base = ch.base_offset.as_f64();
                let k_alt = ch.alternate_offset.as_f64();
                out[i * 3 + c] = (base + k_base) * (w * g).exp2() - k_alt;
            }
        }
        Ok(out)
    }
}

/// Read a `numerator(int32) / denominator(uint32)` rational and reject a
/// zero denominator (every denominator field "shall not be 0", C.2.3).
fn read_signed_rational(buf: &[u8], at: usize) -> Result<GainMapRational> {
    let numerator = read_u32(buf, at)? as i32;
    let denominator = read_u32(buf, at + 4)?;
    if denominator == 0 {
        return Err(Error::invalid(
            "avif: gain map metadata rational denominator is 0 (ISO 21496-1 C.2.3)",
        ));
    }
    Ok(GainMapRational {
        numerator,
        denominator,
    })
}

/// Read a `numerator(uint32) / denominator(uint32)` rational. The HDR
/// headroom numerators are logically unsigned but stored in the same
/// `GainMapRational` (`i32` numerator) for a uniform type; values up to
/// `i32::MAX` round-trip exactly and the denominator is still rejected
/// when zero.
fn read_unsigned_rational(buf: &[u8], at: usize) -> Result<GainMapRational> {
    read_signed_rational(buf, at)
}

/// True when the rational value of `a` is greater than or equal to
/// the rational value of `b`. Computes the comparison via the
/// cross-multiplied `i64` product so the predicate is exact across
/// any (numerator, denominator) pair without floating-point rounding.
///
/// Both denominators are required to be non-zero (and positive — the
/// underlying field is `unsigned int(32)` per Annex C.2.2 and the
/// reader rejects zero in `read_signed_rational`). With positive
/// denominators, `a/da >= b/db` iff `a*db >= b*da`. Numerators are
/// `i32` and denominators fit in `u32`, so the products always fit in
/// `i64` (max magnitude ~2^31 × 2^32 = 2^63, the i64 limit).
fn rational_ge(a: &GainMapRational, b: &GainMapRational) -> bool {
    let lhs = (a.numerator as i64) * (b.denominator as i64);
    let rhs = (b.numerator as i64) * (a.denominator as i64);
    lhs >= rhs
}

/// True when the rational values of `a` and `b` are not equal. Uses
/// the same exact i64 cross-multiplication as [`rational_ge`] so
/// `1/1` and `2/2` correctly compare as equal (and therefore are not
/// reported as differing).
fn rationals_differ(a: &GainMapRational, b: &GainMapRational) -> bool {
    let lhs = (a.numerator as i64) * (b.denominator as i64);
    let rhs = (b.numerator as i64) * (a.denominator as i64);
    lhs != rhs
}

/// Per-grid `shall`-level compliance against av1-avif §7's
/// transformative-property constraint:
///
/// > Transformative properties shall not be associated with items in a
/// > derivation chain (as defined in [MIAF]) that serves as an input to
/// > a grid derived image item. For example, if a file contains a grid
/// > item and its referenced coded image items, cropping, mirroring or
/// > rotation transformations are only permitted on the grid item itself.
///
/// This is a *file-shape* constraint: the spec lets the grid item itself
/// carry any of `clap` / `irot` / `imir`, but forbids any of its `dimg`
/// input tiles from doing so. A reader that processes a non-compliant
/// file would either silently get the wrong pixels (if it ignored the
/// per-tile transform) or render a torn canvas (if it honoured the
/// per-tile transform before compositing).
///
/// One [`GridDerivationAudit`] record is emitted per `'grid'` item in
/// `iinf` declaration order via [`audit_grid_derivations`]. Each record
/// lists the offending `(tile_item_id, transformative_kind)` pairs found
/// on any `dimg` input. An empty `offenders` vector is the compliant
/// case; [`Self::is_compliant`] is a one-call gate.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GridDerivationAudit {
    /// The `'grid'` item id this audit describes.
    pub grid_item_id: u32,
    /// Tile items referenced by the grid's `dimg` iref, in declaration
    /// order. Empty when the grid has no `dimg` iref (already a malformed
    /// grid; the audit still emits the record).
    pub tile_item_ids: Vec<u32>,
    /// `(tile_item_id, property_kind)` pairs where a tile carries a
    /// transformative property (`'clap'` / `'irot'` / `'imir'` /
    /// `'iscl'`). One entry per (tile, kind) — a tile that carries
    /// all four lands as four entries. Empty when the grid is
    /// compliant.
    pub offenders: Vec<(u32, BoxType)>,
}

impl GridDerivationAudit {
    /// True when no tile in the derivation chain carries any
    /// transformative property — the spec-compliant shape.
    pub fn is_compliant(&self) -> bool {
        self.offenders.is_empty()
    }

    /// Convenience: list the unique offending tile item ids (a tile that
    /// carries multiple transformative properties only appears once).
    pub fn offending_tile_ids(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self.offenders.iter().map(|(id, _)| *id).collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Audit every `'grid'` item carried in `meta` against the av1-avif §7
/// transformative-property `shall`. Returns one [`GridDerivationAudit`]
/// record per grid item, in `iinf` declaration order. The returned
/// vector is empty when the file ships no grid items.
///
/// Spec: av1-avif v1.2.0 §7 General constraints — "Transformative
/// properties shall not be associated with items in a derivation chain
/// that serves as an input to a grid derived image item." The
/// transformative properties this crate recognises are `'clap'`
/// (cropping, HEIF §6.5.5), `'irot'` (rotation, HEIF §6.5.10),
/// `'imir'` (mirroring, HEIF §6.5.11), and `'iscl'` (image scaling,
/// HEIF §6.5.13). `'rref'` is descriptive (not transformative) and
/// is therefore not flagged; an explicit `Property::Other`
/// association on a tile is surfaced by the existing
/// [`crate::meta::Meta::unsupported_essential_properties`] path.
pub fn audit_grid_derivations(meta: &crate::meta::Meta) -> Vec<GridDerivationAudit> {
    let grid_type = crate::parser::ITEM_TYPE_GRID;
    let dimg = b(b"dimg");
    let irot = b(b"irot");
    let imir = b(b"imir");
    let clap = b(b"clap");
    let iscl = b(b"iscl");
    meta.item_ids_of_type(&grid_type)
        .into_iter()
        .map(|grid_id| {
            let tile_item_ids = meta.iref_targets(&dimg, grid_id);
            let mut offenders = Vec::new();
            for tile_id in &tile_item_ids {
                // For each tile we check the four transformative property
                // kinds explicitly so the output preserves a stable
                // (clap, irot, imir, iscl) ordering per offending tile —
                // easier for callers (and tests) to diff than association
                // order, which depends on `ipma` writer choice.
                for kind in [clap, irot, imir, iscl] {
                    if meta.property_for(*tile_id, &kind).is_some() {
                        offenders.push((*tile_id, kind));
                    }
                }
            }
            GridDerivationAudit {
                grid_item_id: grid_id,
                tile_item_ids,
                offenders,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// HEIF §6.6.2.1 — Identity Derived Image Item (`iden`) `shall`-level compliance
// ---------------------------------------------------------------------------

/// Per-iden `shall`-level compliance against ISO/IEC 23008-12 (HEIF) §6.6.2.1
/// (Identity derivation):
///
/// > A derived image item of the `item_type` value `'iden'` (identity
/// > transformation) may be used when it is desired to use transformative
/// > properties to derive an image item. The derived image item **shall**
/// > have no item body (i.e. no extents), and `reference_count` for the
/// > `'dimg'` item reference of a `'iden'` derived image item **shall** be
/// > equal to 1.
///
/// In addition to those two clause-local `shall`s, HEIF §6.6.1 imposes a
/// crosscutting `shall` that applies to every derived image item (`iden`
/// included):
///
/// > The number of `SingleItemTypeReferenceBoxes` with the box type `'dimg'`
/// > and with the same value of `from_item_ID` **shall** not be greater
/// > than 1.
///
/// Together, the constraints check whether a file's `'iden'` items obey
/// the standalone identity-derivation `shall`s without depending on any
/// pixel-side decode (the AV1 OBU pipeline is irrelevant here — `iden`
/// items have no body to decode in the first place).
///
/// One [`IdenCompliance`] record is emitted per `'iden'` item in `iinf`
/// declaration order via [`audit_iden_derivations`]. Each record reports:
///
/// * `dimg_reference_count` — number of `'dimg'` `to_ids` listed for the
///   iden's `from_item_ID`. Compliant value is exactly `1`.
/// * `dimg_iref_count` — how many separate `'dimg'` iref entries share the
///   iden's `from_item_ID`. Compliant value is at most `1` (HEIF §6.6.1).
/// * `has_item_body` — whether the iden's `'iloc'` entry lists any
///   non-empty extent. Compliant value is `false` (no body).
/// * `source_item_id` — the single contributing source item id, when
///   `dimg_reference_count == 1`. Useful for callers that want to resolve
///   which coded image item the iden derives from without re-walking the
///   iref.
///
/// All three checks are `shall`-level, so a fail means the file is
/// non-conformant per HEIF §6.6.2.1 / §6.6.1. [`Self::is_compliant`]
/// reports the AND of every signal; [`Self::missing`] enumerates the
/// failing checks for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct IdenCompliance {
    /// The `'iden'` item id this audit describes.
    pub iden_item_id: u32,
    /// Number of `'dimg'` `to_ids` listed for the iden's `from_item_ID`,
    /// across the single matching iref entry (HEIF §6.6.1 forbids more
    /// than one such entry; when multiple are present this counts the
    /// first only, and `dimg_iref_count` flags the violation separately).
    /// Compliant value: exactly `1`.
    pub dimg_reference_count: usize,
    /// Number of distinct `'dimg'` `SingleItemTypeReferenceBox` entries
    /// whose `from_item_ID` equals the iden item id. Compliant value: at
    /// most `1` (HEIF §6.6.1).
    pub dimg_iref_count: usize,
    /// `true` when the iden item's `'iloc'` entry carries any extent —
    /// HEIF §6.6.2.1 mandates the iden item have no body. Compliant
    /// value: `false`. An iden with no `'iloc'` entry at all is also
    /// compliant (empty extent list).
    pub has_item_body: bool,
    /// The single source image item id contributing to the iden, when
    /// `dimg_reference_count == 1`. `None` when the iden has zero
    /// inputs or when the input count is non-conformant (in which case
    /// the audit doesn't pick "the" source — the file is malformed and
    /// the caller should inspect `dimg_reference_count` to disambiguate).
    pub source_item_id: Option<u32>,
}

impl IdenCompliance {
    /// True when every HEIF §6.6.2.1 + §6.6.1 `shall` passes for this
    /// iden item:
    ///
    /// * exactly one `'dimg'` input (`dimg_reference_count == 1`),
    /// * exactly one `'dimg'` iref entry for the iden's `from_item_ID`
    ///   (`dimg_iref_count <= 1`; zero is a degenerate but still
    ///   non-conformant case),
    /// * no item body (`!has_item_body`).
    pub fn is_compliant(&self) -> bool {
        self.dimg_reference_count == 1 && self.dimg_iref_count == 1 && !self.has_item_body
    }

    /// Human-readable list of failed `shall`s. Returns an empty vector
    /// when [`Self::is_compliant`].
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.dimg_reference_count != 1 {
            out.push("dimg-reference-count-eq-1");
        }
        if self.dimg_iref_count != 1 {
            out.push("dimg-iref-count-eq-1");
        }
        if self.has_item_body {
            out.push("no-item-body");
        }
        out
    }
}

/// Audit every `'iden'` item carried in `meta` against the HEIF §6.6.2.1
/// (Identity derivation) and §6.6.1 (derived-image cross-clause)
/// `shall`-level constraints. Returns one [`IdenCompliance`] record per
/// iden item, in `iinf` declaration order. The returned vector is empty
/// when the file ships no iden items.
///
/// Spec: ISO/IEC 23008-12 (HEIF) §6.6.2.1 — "The derived image item shall
/// have no item body (i.e. no extents), and `reference_count` for the
/// `'dimg'` item reference of a `'iden'` derived image item shall be equal
/// to 1." + §6.6.1 — "The number of `SingleItemTypeReferenceBoxes` with
/// the box type `'dimg'` and with the same value of `from_item_ID` shall
/// not be greater than 1."
pub fn audit_iden_derivations(meta: &crate::meta::Meta) -> Vec<IdenCompliance> {
    let iden_type = crate::meta::ITEM_TYPE_IDEN;
    let dimg = b(b"dimg");
    meta.item_ids_of_type(&iden_type)
        .into_iter()
        .map(|iden_id| audit_one_iden(meta, &dimg, iden_id))
        .collect()
}

fn audit_one_iden(meta: &crate::meta::Meta, dimg: &BoxType, iden_id: u32) -> IdenCompliance {
    // Count how many distinct `'dimg'` iref entries share this iden as
    // their `from_item_ID` (HEIF §6.6.1: shall not be greater than 1).
    let dimg_iref_count = meta
        .irefs
        .iter()
        .filter(|e| &e.reference_type == dimg && e.from_id == iden_id)
        .count();

    // `iref_targets` returns the to_ids of the first matching entry, so
    // when `dimg_iref_count > 1` the reference count reported here is from
    // entry 0 only — that's fine: the `dimg_iref_count` field is what
    // flags the §6.6.1 violation, while `dimg_reference_count` checks the
    // §6.6.2.1 single-input shall against the (possibly first-of-many)
    // observed input list.
    let inputs = meta.iref_targets(dimg, iden_id);
    let dimg_reference_count = inputs.len();
    let source_item_id = if dimg_reference_count == 1 {
        Some(inputs[0])
    } else {
        None
    };

    // HEIF §6.6.2.1 mandates the iden item carry no item body. An item
    // with no `'iloc'` entry at all trivially has no body; an entry with
    // an empty extent list (or every extent length zero) is equivalent.
    // Any non-zero-length extent flags a violation.
    let has_item_body = meta
        .location_by_id(iden_id)
        .map(|loc| loc.extents.iter().any(|x| x.length > 0))
        .unwrap_or(false);

    IdenCompliance {
        iden_item_id: iden_id,
        dimg_reference_count,
        dimg_iref_count,
        has_item_body,
        source_item_id,
    }
}

// ---------------------------------------------------------------------------
// AV1 Alpha Image Item bit-depth match audit (av1-avif v1.2.0 §4.1)
// ---------------------------------------------------------------------------

/// Decode bit-depth from the first three bytes of an `av1C` (AV1
/// CodecConfigurationRecord) payload. Returns `None` when the slice is
/// too short to carry the `high_bitdepth` / `twelve_bit` flag byte.
///
/// Layout per av1-avif §2.2.1 (which references AV1 Bitstream &
/// Decoding Process §6.4): `av1C[2]` packs
/// `high_bitdepth (1) | twelve_bit (1) | monochrome (1) |
///  chroma_subsampling_x (1) | chroma_subsampling_y (1) |
///  chroma_sample_position (2) | reserved (1)`. Bit depth is `8`
/// (neither flag), `10` (high only), or `12` (both).
fn decode_av1c_bit_depth(av1c: &[u8]) -> Option<u8> {
    if av1c.len() < 3 {
        return None;
    }
    let b2 = av1c[2];
    let high_bitdepth = ((b2 >> 6) & 1) != 0;
    let twelve_bit = ((b2 >> 5) & 1) != 0;
    Some(if twelve_bit {
        12
    } else if high_bitdepth {
        10
    } else {
        8
    })
}

/// Per-(alpha, master) `shall`-level compliance against av1-avif v1.2.0
/// §4.1 (Auxiliary Image Items and Sequences):
///
/// > An AV1 Alpha Image Item (respectively an AV1 Alpha Image Sequence)
/// > shall be encoded with the same bit depth as the associated master
/// > AV1 Image Item (respectively AV1 Image Sequence).
///
/// One [`AlphaBitDepthAudit`] record is emitted per `(alpha_item_id,
/// master_item_id)` pair an `auxl` iref's `to_ids` declares, in iref
/// declaration order. The audit is independent of any AV1 OBU decode —
/// the check operates entirely on the `av1C` configuration record
/// surfaced by the box walker.
///
/// Fields:
///
/// * `alpha_bit_depth` / `master_bit_depth` — bit depths extracted from
///   each item's `av1C` property (`8`, `10`, or `12`). `None` when the
///   corresponding item carries no `av1C` (malformed) or when the
///   `av1C` payload is too short to carry the flag byte.
/// * `master_missing_av1c` — `true` when the master item id appearing
///   in the `auxl` iref does not resolve to an item with an `av1C`
///   property. This is also a §2.1 violation (every AV1 Image Item
///   shall carry `av1C`) and surfaces alongside the bit-depth check
///   here for a single point of inspection.
/// * `alpha_missing_av1c` — same for the alpha item.
///
/// The check passes ([`AlphaBitDepthAudit::is_compliant`]) when both
/// items carry an `av1C` and both decoded bit depths agree. Either
/// item missing `av1C`, or any mismatch, fails the audit.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AlphaBitDepthAudit {
    /// The AV1 Alpha Image Item id (the `auxl` iref's `from_id`).
    pub alpha_item_id: u32,
    /// The associated master AV1 Image Item id (one of the `auxl`
    /// iref's `to_ids`).
    pub master_item_id: u32,
    /// Bit depth decoded from the alpha item's `av1C` (`8`, `10`, or
    /// `12`), or `None` when `av1C` is absent or truncated.
    pub alpha_bit_depth: Option<u8>,
    /// Bit depth decoded from the master item's `av1C`, or `None` when
    /// `av1C` is absent or truncated.
    pub master_bit_depth: Option<u8>,
    /// `true` when the alpha item carries no `av1C` association at all.
    /// Distinct from a present-but-truncated `av1C` (which surfaces as
    /// `alpha_bit_depth = None` without setting this flag).
    pub alpha_missing_av1c: bool,
    /// `true` when the master item carries no `av1C` association at
    /// all.
    pub master_missing_av1c: bool,
}

impl AlphaBitDepthAudit {
    /// True when both items carry an `av1C` whose decoded bit depth
    /// agrees — the spec-compliant shape per av1-avif §4.1.
    pub fn is_compliant(&self) -> bool {
        match (self.alpha_bit_depth, self.master_bit_depth) {
            (Some(a), Some(m)) => a == m,
            _ => false,
        }
    }

    /// Human-readable list of failed `shall`s. Returns an empty vector
    /// when [`Self::is_compliant`].
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.alpha_missing_av1c {
            out.push("alpha-item-missing-av1C");
        } else if self.alpha_bit_depth.is_none() {
            out.push("alpha-item-av1C-truncated");
        }
        if self.master_missing_av1c {
            out.push("master-item-missing-av1C");
        } else if self.master_bit_depth.is_none() {
            out.push("master-item-av1C-truncated");
        }
        if let (Some(a), Some(m)) = (self.alpha_bit_depth, self.master_bit_depth) {
            if a != m {
                out.push("alpha-master-bit-depth-mismatch");
            }
        }
        out
    }
}

/// Audit every `(AV1 Alpha Image Item, associated master AV1 Image
/// Item)` pairing carried in `meta` against the av1-avif §4.1 `shall`
/// that they share bit depth. Returns one [`AlphaBitDepthAudit`] record
/// per `(alpha, master)` pair declared by an `auxl` iref. The returned
/// vector is empty when the file ships no AV1 Alpha Image Items.
///
/// An item qualifies as an "AV1 Alpha Image Item" when:
///
/// 1. It is the `from_id` of an `auxl` iref entry, and
/// 2. It carries an `auxC` property whose URN starts with the alpha
///    prefix (`urn:mpeg:mpegB:cicp:systems:auxiliary:alpha`), matching
///    [`crate::alpha::ALPHA_URN_PREFIX`].
///
/// Each `to_id` in the same iref entry is audited as a master. A single
/// alpha item declared against multiple masters (a `to_ids` list of
/// length > 1) emits one record per master, in `to_ids` order — the
/// `shall` applies per pairing. Iref entries are processed in the order
/// they appear inside the source `iref` box.
///
/// Spec: av1-avif v1.2.0 §4.1 — "An AV1 Alpha Image Item (respectively
/// an AV1 Alpha Image Sequence) shall be encoded with the same bit
/// depth as the associated master AV1 Image Item (respectively AV1
/// Image Sequence)."
pub fn audit_alpha_bit_depth(meta: &crate::meta::Meta) -> Vec<AlphaBitDepthAudit> {
    let auxl = b(b"auxl");
    let auxc = b(b"auxC");
    let av1c = b(b"av1C");
    let mut out = Vec::new();
    for entry in &meta.irefs {
        if entry.reference_type != auxl {
            continue;
        }
        let alpha_id = entry.from_id;
        // Classify the candidate as an alpha auxiliary by URN prefix
        // match against the `auxC` property. Non-alpha auxiliaries
        // (depth maps, HDR gain maps) are bound by separate `shall`s
        // beyond §4.1's bit-depth constraint and don't surface here.
        let is_alpha = match meta.property_for(alpha_id, &auxc) {
            Some(crate::meta::Property::AuxC(aux)) => {
                aux.aux_type.starts_with(crate::alpha::ALPHA_URN_PREFIX)
            }
            _ => false,
        };
        if !is_alpha {
            continue;
        }
        let (alpha_bit_depth, alpha_missing_av1c) = match meta.property_for(alpha_id, &av1c) {
            Some(crate::meta::Property::Av1C(bytes)) => (decode_av1c_bit_depth(bytes), false),
            _ => (None, true),
        };
        for &master_id in &entry.to_ids {
            let (master_bit_depth, master_missing_av1c) = match meta.property_for(master_id, &av1c)
            {
                Some(crate::meta::Property::Av1C(bytes)) => (decode_av1c_bit_depth(bytes), false),
                _ => (None, true),
            };
            out.push(AlphaBitDepthAudit {
                alpha_item_id: alpha_id,
                master_item_id: master_id,
                alpha_bit_depth,
                master_bit_depth,
                alpha_missing_av1c,
                master_missing_av1c,
            });
        }
    }
    out
}

/// Outcome of walking a single AV1 Image Item's OBU stream to count
/// Sequence Header OBUs, per av1-avif v1.2.0 §2.1 — "The AV1 Image
/// Item Data shall have exactly one Sequence Header OBU."
///
/// One record per AV1 Image Item (`item_type == 'av01'`) in declaration
/// order. The audit is purely structural: it walks the framing defined
/// in AV1 §5.3.1 (header byte plus leb128 `obu_size`) and reads the
/// `obu_type` field from each OBU header (AV1 §5.3.2), incrementing
/// [`Self::sequence_header_count`] each time the type equals
/// `OBU_SEQUENCE_HEADER` (value `1`, per AV1 §6.2.1's `obu_type`
/// enumeration). The OBU payload bodies themselves are not decoded — the
/// walker only needs the framing.
///
/// Three structural failure modes are surfaced distinctly from a plain
/// `sequence_header_count != 1` mismatch:
///
/// * [`Self::missing_iloc`] — the `iinf` lists the item but no `iloc`
///   resolves its bytes. The OBU walk is not attempted.
/// * [`Self::truncated_obu`] — the OBU framing walker hit the end of
///   the stream mid-OBU (truncated leb128, or a declared `obu_size`
///   that runs past the item payload). Any OBUs successfully walked
///   before truncation are still counted; the flag tells the caller
///   the count may be an undercount of the well-formed file the
///   writer intended.
/// * [`Self::has_size_field_zero`] — at least one OBU in the stream
///   has `obu_has_size_field == 0`. AV1 §5.3.1 requires that OBUs
///   carried inside a container that does not separately frame each
///   OBU (which is the case for AVIF image items, see §2.1's
///   "identical to the content of an AV1 Sample marked as 'sync'"
///   constraint and [AV1-ISOBMFF] §2.3.2's requirement that the
///   has_size flag be set when chaining OBUs inside one sample) must
///   carry `obu_size`. We surface this as a separate signal because
///   the walker cannot frame an OBU without an explicit size when
///   chained — it stops at the first such OBU and the count from
///   that point on is undefined.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SequenceHeaderObuAudit {
    /// AV1 Image Item id whose payload was walked.
    pub item_id: u32,
    /// Number of OBUs whose header byte decoded to
    /// `obu_type == OBU_SEQUENCE_HEADER` (value `1`).
    pub sequence_header_count: u32,
    /// Total number of OBUs walked (successfully framed) in the
    /// stream. Diagnostic; not a `shall` check on its own.
    pub total_obu_count: u32,
    /// `true` when no `iloc` entry resolved the item's bytes. The OBU
    /// walk was skipped; [`Self::sequence_header_count`] and
    /// [`Self::total_obu_count`] are both `0`.
    pub missing_iloc: bool,
    /// `true` when the framing walker hit end-of-stream mid-OBU
    /// (truncated leb128 byte sequence, or a declared `obu_size` that
    /// runs past the end of the item payload).
    pub truncated_obu: bool,
    /// `true` when the walker encountered an OBU whose header byte has
    /// `obu_has_size_field == 0`. The walker stops at that OBU because
    /// it has no in-band length to advance past it. Per AV1 §5.3.1 +
    /// av1-avif §2.1, AV1 Image Item Data chains OBUs into one item
    /// payload and the `has_size` bit is required.
    pub has_size_field_zero: bool,
}

impl SequenceHeaderObuAudit {
    /// True when the walk succeeded structurally (`!missing_iloc &&
    /// !truncated_obu && !has_size_field_zero`) AND the OBU stream
    /// contains exactly one Sequence Header OBU.
    ///
    /// Spec: av1-avif v1.2.0 §2.1.
    pub fn is_compliant(&self) -> bool {
        !self.missing_iloc
            && !self.truncated_obu
            && !self.has_size_field_zero
            && self.sequence_header_count == 1
    }

    /// Human-readable list of `shall`-level failures. Empty when
    /// [`Self::is_compliant`].
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.missing_iloc {
            out.push("av01-item-missing-iloc");
        }
        if self.has_size_field_zero {
            out.push("av01-item-obu-has-size-field-zero");
        }
        if self.truncated_obu {
            out.push("av01-item-obu-stream-truncated");
        }
        if !self.missing_iloc && !self.has_size_field_zero && !self.truncated_obu {
            match self.sequence_header_count {
                0 => out.push("av01-item-missing-sequence-header-obu"),
                1 => {} // compliant
                _ => out.push("av01-item-multiple-sequence-header-obus"),
            }
        }
        out
    }
}

/// `obu_type` value for `OBU_SEQUENCE_HEADER` per AV1 §6.2.1.
const AV1_OBU_TYPE_SEQUENCE_HEADER: u8 = 1;

/// Decode an AV1 leb128 `obu_size` value (AV1 §4.10.5) from `bytes`.
///
/// Returns the decoded value and the number of bytes consumed
/// (`Leb128Bytes`, 1..=8). Returns `Err` when the bitstream ends in
/// the middle of a leb128 byte sequence (continuation bit set on the
/// last available byte) or when the leb128 sequence exceeds 8 bytes
/// without terminating.
fn read_leb128(bytes: &[u8]) -> core::result::Result<(u32, usize), ()> {
    let mut value: u64 = 0;
    for i in 0..8 {
        if i >= bytes.len() {
            return Err(());
        }
        let byte = bytes[i];
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            // §4.10.5 also requires value <= (1 << 32) - 1 for
            // bitstream conformance; we surface a u32 so any wider
            // claim trips the conversion.
            if value > u64::from(u32::MAX) {
                return Err(());
            }
            return Ok((value as u32, i + 1));
        }
    }
    Err(())
}

/// Walk the OBU framing of one AV1 Image Item's payload and count
/// Sequence Header OBUs. `payload` is the raw item bytes resolved via
/// the iloc.
///
/// Per AV1 §5.3.1 / §5.3.2:
///
/// * OBU header byte: `obu_forbidden_bit(1) | obu_type(4) |
///   obu_extension_flag(1) | obu_has_size_field(1) | obu_reserved_1bit(1)`.
/// * If `obu_extension_flag == 1`, one extension header byte follows
///   (§5.3.3 — temporal_id + spatial_id + reserved, total 8 bits).
/// * If `obu_has_size_field == 1`, a `leb128()` `obu_size` follows;
///   that many bytes are the OBU payload.
/// * If `obu_has_size_field == 0` we cannot frame the next OBU — the
///   walker stops and reports [`SequenceHeaderObuAudit::has_size_field_zero`].
fn walk_obu_stream(item_id: u32, payload: &[u8]) -> SequenceHeaderObuAudit {
    let mut out = SequenceHeaderObuAudit {
        item_id,
        ..SequenceHeaderObuAudit::default()
    };
    let mut cursor = 0usize;
    while cursor < payload.len() {
        // 1. OBU header byte.
        let header = payload[cursor];
        cursor += 1;
        let obu_type = (header >> 3) & 0x0f;
        let obu_extension_flag = (header >> 2) & 0x01;
        let obu_has_size_field = (header >> 1) & 0x01;
        // 2. Optional extension header.
        if obu_extension_flag == 1 {
            if cursor >= payload.len() {
                out.truncated_obu = true;
                return out;
            }
            cursor += 1;
        }
        // 3. Size field. AV1 §5.3.1 — when chained in a container that
        // doesn't externally frame each OBU (which is exactly the
        // AVIF Image Item Data case per av1-avif §2.1), this MUST be
        // set or we cannot find the next OBU.
        if obu_has_size_field == 0 {
            out.has_size_field_zero = true;
            // Still count this OBU's type — the header byte was
            // successfully read.
            if obu_type == AV1_OBU_TYPE_SEQUENCE_HEADER {
                out.sequence_header_count = out.sequence_header_count.saturating_add(1);
            }
            out.total_obu_count = out.total_obu_count.saturating_add(1);
            return out;
        }
        let (obu_size, leb_len) = match read_leb128(&payload[cursor..]) {
            Ok(v) => v,
            Err(()) => {
                out.truncated_obu = true;
                return out;
            }
        };
        cursor += leb_len;
        let obu_size = obu_size as usize;
        if cursor
            .checked_add(obu_size)
            .map(|end| end > payload.len())
            .unwrap_or(true)
        {
            out.truncated_obu = true;
            // The header byte itself was readable, count it before
            // bailing — this lets a caller see whether the truncation
            // dropped a Sequence Header.
            if obu_type == AV1_OBU_TYPE_SEQUENCE_HEADER {
                out.sequence_header_count = out.sequence_header_count.saturating_add(1);
            }
            out.total_obu_count = out.total_obu_count.saturating_add(1);
            return out;
        }
        cursor += obu_size;
        if obu_type == AV1_OBU_TYPE_SEQUENCE_HEADER {
            out.sequence_header_count = out.sequence_header_count.saturating_add(1);
        }
        out.total_obu_count = out.total_obu_count.saturating_add(1);
    }
    out
}

/// Audit every AV1 Image Item (`item_type == 'av01'`) in `meta`
/// against the av1-avif v1.2.0 §2.1 `shall` "The AV1 Image Item Data
/// shall have exactly one Sequence Header OBU."
///
/// The function walks each item's payload bytes (resolved via the
/// item's `iloc` entry against `file`) and counts OBUs whose header
/// byte decodes to `obu_type == OBU_SEQUENCE_HEADER` (value `1`, per
/// AV1 §6.2.1). Items are reported in the same order as
/// `Meta::item_ids_of_type(b"av01")`.
///
/// The returned vector is empty when the file ships no AV1 Image
/// Items (a degenerate case, since an AVIF primary is required to
/// have at least one).
///
/// Spec sources:
/// * av1-avif v1.2.0 §2.1 — the `shall` audited.
/// * AV1 (Bitstream & Decoding Process Specification v1.0.0-errata1)
///   §5.3.1 (general OBU framing), §5.3.2 (OBU header byte layout),
///   §4.10.5 (`leb128()` decoder), §6.2.1 (`obu_type` enumeration).
pub fn audit_sequence_header_obu(
    meta: &crate::meta::Meta,
    file: &[u8],
) -> Vec<SequenceHeaderObuAudit> {
    let av01 = b(b"av01");
    let mut out = Vec::new();
    for item_id in meta.item_ids_of_type(&av01) {
        let Some(loc) = meta.location_by_id(item_id) else {
            out.push(SequenceHeaderObuAudit {
                item_id,
                missing_iloc: true,
                ..SequenceHeaderObuAudit::default()
            });
            continue;
        };
        // Reuse the standard iloc resolver. A failure here (e.g.
        // construction_method we don't support, or extent offsets
        // outside `file`) is surfaced the same way as missing iloc —
        // the audit can't reach the bytes either way.
        let Ok(payload) = crate::parser::item_bytes(file, loc) else {
            out.push(SequenceHeaderObuAudit {
                item_id,
                missing_iloc: true,
                ..SequenceHeaderObuAudit::default()
            });
            continue;
        };
        out.push(walk_obu_stream(item_id, payload));
    }
    out
}

// ===========================================================================
// AVIF Profile compliance — av1-avif v1.2.0 §8.2 / §8.3 (MA1B / MA1A)
// ===========================================================================

/// AV1 `seq_level_idx_0` decoded from `av1C[1]` low 5 bits, or `None`
/// when `av1c` is too short to carry byte 1. av1-isobmff §2.3 packs
/// `seq_profile (3) | seq_level_idx_0 (5)` into byte 1.
///
/// Crate-visible so the AVIS audit ([`crate::avis::audit_avis_profile_compliance`])
/// can decode the per-track av1C surfaced via `stsd → av01 → av1C`.
pub(crate) fn decode_av1c_seq_level_idx_0(av1c: &[u8]) -> Option<u8> {
    if av1c.len() < 2 {
        return None;
    }
    Some(av1c[1] & 0x1F)
}

/// AV1 `seq_profile` decoded from `av1C[1]` high 3 bits, or `None`
/// when `av1c` is too short. Per AV1 Annex A.2: `0` = Main,
/// `1` = High, `2` = Professional.
///
/// Crate-visible so the AVIS audit ([`crate::avis::audit_avis_profile_compliance`])
/// can decode the per-track av1C surfaced via `stsd → av01 → av1C`.
pub(crate) fn decode_av1c_seq_profile(av1c: &[u8]) -> Option<u8> {
    if av1c.len() < 2 {
        return None;
    }
    Some((av1c[1] >> 5) & 0x7)
}

/// Which AVIF profile brand a record was audited against — see
/// [`AvifProfileCompliance::profile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvifProfile {
    /// `MA1B` — AVIF Baseline Profile (av1-avif v1.2.0 §8.2). Requires
    /// AV1 Main Profile (`seq_profile == 0`) at level 5.1 or lower
    /// (`seq_level_idx_0 <= 13`).
    Baseline,
    /// `MA1A` — AVIF Advanced Profile (av1-avif v1.2.0 §8.3). For AV1
    /// Image Items requires AV1 High Profile (`seq_profile <= 1`) at
    /// level 6.0 or lower (`seq_level_idx_0 <= 16`).
    Advanced,
}

/// Per-AV1-Image-Item compliance against the av1-avif v1.2.0 §8.2 /
/// §8.3 profile `shall`-level constraints when the file's `ftyp`
/// declares the corresponding brand.
///
/// One record is emitted by [`audit_avif_profile_compliance`] per
/// `(AV1 Image Item, declared profile)` pairing — when a file
/// declares both `MA1B` and `MA1A` brands (unusual but legal per §8.1
/// since both are independent profile claims), each AV1 Image Item
/// emits one record per declared brand.
///
/// The fields:
///
/// * [`Self::profile`] — which AVIF profile this record is checking.
/// * [`Self::item_id`] — the AV1 Image Item's id.
/// * [`Self::seq_profile`] — the AV1 `seq_profile` value decoded from
///   the item's `av1C[1]` high 3 bits (`0` = Main, `1` = High,
///   `2` = Professional). `None` when `av1C` is absent or truncated.
/// * [`Self::seq_level_idx_0`] — the AV1 `seq_level_idx_0` value
///   decoded from `av1C[1]` low 5 bits (`13` = level 5.1,
///   `16` = level 6.0, `31` = unconstrained per AV1 §A.3). `None` when
///   `av1C` is absent or truncated.
/// * [`Self::missing_av1c`] — `true` when the item carries no `av1C`
///   association at all (also a §2.1 violation; surfaced here so a
///   single point of inspection covers both).
///
/// The check passes ([`Self::is_compliant`]) when the `av1C` is
/// present and the (seq_profile, seq_level_idx_0) pair satisfies the
/// declared profile's constraints. The seq_level_idx_0 = 31 "Maximum
/// parameters" value is treated as not-compliant for either profile:
/// the spec's profile clauses both bound the level (5.1 / 6.0), and
/// the level-31 carve-out signals unconstrained sizing, which is
/// outside either profile's reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvifProfileCompliance {
    /// The AVIF profile this record is checking the item against.
    pub profile: AvifProfile,
    /// The AV1 Image Item id whose `av1C` was inspected.
    pub item_id: u32,
    /// `seq_profile` decoded from `av1C[1]` high 3 bits, or `None`
    /// when `av1C` is absent or truncated.
    pub seq_profile: Option<u8>,
    /// `seq_level_idx_0` decoded from `av1C[1]` low 5 bits, or `None`
    /// when `av1C` is absent or truncated.
    pub seq_level_idx_0: Option<u8>,
    /// `true` when the item has no `av1C` property association at all
    /// (distinct from a present-but-truncated `av1C`, which surfaces
    /// as both fields `None` without setting this flag).
    pub missing_av1c: bool,
}

impl AvifProfileCompliance {
    /// True when the AV1 Image Item satisfies the declared AVIF
    /// profile's `shall`-level constraints on AV1 profile + level.
    ///
    /// Baseline (`MA1B`, §8.2): `seq_profile == 0` (AV1 Main) AND
    /// `seq_level_idx_0 <= 13` (level ≤ 5.1).
    ///
    /// Advanced (`MA1A`, §8.3): `seq_profile <= 1` (AV1 Main or
    /// High) AND `seq_level_idx_0 <= 16` (level ≤ 6.0). The §8.3
    /// `shall` is "The AV1 profile shall be the High Profile" — the
    /// AV1 Annex A.2 definition of "High Profile decoders" includes
    /// streams with `seq_profile == 0` (Main is a subset of High),
    /// so a Main-Profile stream is also accepted under `MA1A`.
    ///
    /// Returns `false` when `av1C` is missing or truncated.
    pub fn is_compliant(&self) -> bool {
        match (self.seq_profile, self.seq_level_idx_0) {
            (Some(p), Some(l)) => match self.profile {
                AvifProfile::Baseline => p == 0 && l <= 13,
                AvifProfile::Advanced => p <= 1 && l <= 16,
            },
            _ => false,
        }
    }

    /// Human-readable list of failed `shall`s. Returns an empty vector
    /// when [`Self::is_compliant`]. Tokens:
    /// `item-missing-av1C`, `item-av1C-truncated`,
    /// `seq-profile-out-of-range`, `seq-level-idx-out-of-range`.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.missing_av1c {
            out.push("item-missing-av1C");
            return out;
        }
        if self.seq_profile.is_none() || self.seq_level_idx_0.is_none() {
            out.push("item-av1C-truncated");
            return out;
        }
        let p = self.seq_profile.unwrap();
        let l = self.seq_level_idx_0.unwrap();
        let (max_p, max_l) = match self.profile {
            AvifProfile::Baseline => (0u8, 13u8),
            AvifProfile::Advanced => (1u8, 16u8),
        };
        if p > max_p {
            out.push("seq-profile-out-of-range");
        }
        if l > max_l {
            out.push("seq-level-idx-out-of-range");
        }
        out
    }
}

/// Audit every AV1 Image Item in `meta` against the av1-avif v1.2.0
/// §8.2 / §8.3 profile `shall`-level constraints, but only when the
/// file's `ftyp` declares the corresponding brand. Items are reported
/// in `iinf` declaration order; for files declaring both `MA1B` and
/// `MA1A`, each item emits one record per declared brand (Baseline
/// before Advanced).
///
/// The audit is independent of any AV1 OBU decode — it operates
/// entirely on `av1C[1]` (which packs `seq_profile (3) |
/// seq_level_idx_0 (5)` per av1-isobmff §2.3). For grid primaries the
/// constraint is on the tile items, not the grid item itself — `'grid'`
/// items carry no `av1C` and are skipped here (the per-tile records
/// give the same coverage, since av1-avif §4.2 keeps the AV1 profile +
/// level uniform across grid tiles).
///
/// The returned vector is empty when (a) the file ships no AV1 Image
/// Items, or (b) the file declares neither `MA1B` nor `MA1A` in its
/// `ftyp` compatible-brands list. The second case is a deliberate
/// no-op: a file that doesn't claim a profile has nothing to fail.
///
/// Spec sources:
/// * av1-avif v1.2.0 §8.2 — `MA1B` Baseline Profile constraints.
/// * av1-avif v1.2.0 §8.3 — `MA1A` Advanced Profile constraints.
/// * AV1 §A.2 — Profiles (Main / High / Professional).
/// * AV1 §A.3 — Levels (seq_level_idx_0 ↔ X.Y mapping; 13 = 5.1,
///   16 = 6.0, 31 = unconstrained).
/// * av1-isobmff §2.3 — `av1C` byte layout.
pub fn audit_avif_profile_compliance(
    meta: &crate::meta::Meta,
    brands: &crate::parser::BrandClass,
) -> Vec<AvifProfileCompliance> {
    let av01 = b(b"av01");
    let av1c_kind = b(b"av1C");
    let mut out = Vec::new();
    if !brands.is_baseline_profile && !brands.is_advanced_profile {
        return out;
    }
    for item_id in meta.item_ids_of_type(&av01) {
        let (seq_profile, seq_level_idx_0, missing_av1c) =
            match meta.property_for(item_id, &av1c_kind) {
                Some(crate::meta::Property::Av1C(bytes)) => (
                    decode_av1c_seq_profile(bytes),
                    decode_av1c_seq_level_idx_0(bytes),
                    false,
                ),
                _ => (None, None, true),
            };
        if brands.is_baseline_profile {
            out.push(AvifProfileCompliance {
                profile: AvifProfile::Baseline,
                item_id,
                seq_profile,
                seq_level_idx_0,
                missing_av1c,
            });
        }
        if brands.is_advanced_profile {
            out.push(AvifProfileCompliance {
                profile: AvifProfile::Advanced,
                item_id,
                seq_profile,
                seq_level_idx_0,
                missing_av1c,
            });
        }
    }
    out
}

// ===========================================================================
// Derived-image geometry resolution (HEIF §6.3 / §6.6.2)
// ===========================================================================
//
// §6.3 defines the *output image* of any image item as the result of
// applying its transformative item properties — in ItemPropertyAssociation
// order — to the item's *reconstructed image*. For a coded item the
// reconstructed image is the decoded picture (`ispe` dimensions); for a
// derived item it is the result of the derivation operation (the
// descriptor's `output_width`/`output_height` for `grid` / `iovl`, or the
// single input's output image for `iden`).
//
// The helpers below resolve this geometry **without decoding any AV1
// bitstream** — they walk only the box-level item-property graph. They are
// the dimension half of derived-image evaluation: a caller that later wires
// in a real AV1 decoder uses them to size the output canvas, place overlay
// inputs, and crop/rotate identity derivations, and to validate that the
// declared geometry is self-consistent before any pixels exist.

/// A transformative item property that changes (or preserves) the pixel
/// dimensions of the image it is applied to, in the order it appears in the
/// item's `ipma` association. Descriptive properties (`ispe`, `colr`,
/// `pixi`, …) do not appear here — only the four transformative properties
/// HEIF defines that affect output geometry.
///
/// Spec: ISO/IEC 23008-12 §6.5.10 (`irot`), §6.5.12 (`imir`), §6.5.8 /
/// §6.5.9 (`clap`), §6.5.13 (`iscl`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DimTransform {
    /// Counter-clockwise rotation, `angle` × 90°. 90°/270° swap width and
    /// height; 0°/180° preserve them.
    Rotate { angle: u8 },
    /// Mirror about an axis. Does not change dimensions.
    Mirror { axis: u8 },
    /// Clean-aperture crop. The output dimensions are the clean-aperture
    /// width/height rounded to nearest integer (`§6.5.9` rationals), or the
    /// input unchanged when the crop falls outside the input rectangle.
    Crop {
        width_n: i32,
        width_d: i32,
        height_n: i32,
        height_d: i32,
    },
    /// Rational scale per §6.5.13: `ceil(input * num / den)` independently
    /// on each axis.
    Scale {
        target_width_numerator: u16,
        target_width_denominator: u16,
        target_height_numerator: u16,
        target_height_denominator: u16,
    },
}

impl DimTransform {
    /// Apply this transform's effect on dimensions to `(w, h)`. Returns the
    /// post-transform `(width, height)`. Defensive: a degenerate crop or a
    /// zero-denominator scale leaves the dimensions unchanged (mirroring the
    /// no-op fall-throughs in [`crate::transform`]).
    pub fn apply_dims(&self, w: u32, h: u32) -> (u32, u32) {
        match *self {
            DimTransform::Rotate { angle } => {
                if angle % 2 == 1 {
                    (h, w)
                } else {
                    (w, h)
                }
            }
            DimTransform::Mirror { .. } => (w, h),
            DimTransform::Crop {
                width_n,
                width_d,
                height_n,
                height_d,
            } => {
                if width_d == 0 || height_d == 0 {
                    return (w, h);
                }
                // §6.5.9: clean-aperture width/height are signed rationals;
                // round to nearest integer (same rounding as
                // `transform::apply_clap`).
                let cw = (width_n as i64 + width_d as i64 / 2) / width_d as i64;
                let ch = (height_n as i64 + height_d as i64 / 2) / height_d as i64;
                if cw <= 0 || ch <= 0 || cw > w as i64 || ch > h as i64 {
                    return (w, h);
                }
                (cw as u32, ch as u32)
            }
            DimTransform::Scale {
                target_width_numerator,
                target_width_denominator,
                target_height_numerator,
                target_height_denominator,
            } => {
                let iscl = crate::meta::Iscl {
                    target_width_numerator,
                    target_width_denominator,
                    target_height_numerator,
                    target_height_denominator,
                };
                iscl.scaled_dims(w, h).unwrap_or((w, h))
            }
        }
    }
}

/// The ordered list of dimension-affecting transformative item properties
/// associated with `item_id`, in `ipma` association order. Non-transformative
/// properties are skipped. An item with no transformative properties yields
/// an empty vector (its output image equals its reconstructed image — §6.3).
///
/// Per §6.5.11, a `lsel` (layer selector) — when present — precedes every
/// transformative property in the association order; it carries no geometry
/// effect and so does not appear in the returned list, but the relative
/// order of the transformative properties that follow it is preserved.
pub fn transform_chain(meta: &crate::meta::Meta, item_id: u32) -> Vec<DimTransform> {
    use crate::meta::Property;
    let Some(assoc) = meta.assoc_by_id(item_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for pa in &assoc.entries {
        let Some(prop) = meta.properties.get(pa.index as usize) else {
            continue;
        };
        match prop {
            Property::Irot(r) => out.push(DimTransform::Rotate { angle: r.angle }),
            Property::Imir(m) => out.push(DimTransform::Mirror { axis: m.axis }),
            Property::Clap(c) => out.push(DimTransform::Crop {
                width_n: c.clean_aperture_width_n,
                width_d: c.clean_aperture_width_d,
                height_n: c.clean_aperture_height_n,
                height_d: c.clean_aperture_height_d,
            }),
            Property::Iscl(s) => out.push(DimTransform::Scale {
                target_width_numerator: s.target_width_numerator,
                target_width_denominator: s.target_width_denominator,
                target_height_numerator: s.target_height_numerator,
                target_height_denominator: s.target_height_denominator,
            }),
            _ => {}
        }
    }
    out
}

/// Apply an item's transform chain to a reconstructed `(width, height)`,
/// returning the **output-image** dimensions per HEIF §6.3. Equivalent to
/// folding [`DimTransform::apply_dims`] over [`transform_chain`].
pub fn output_dims_from_reconstructed(
    meta: &crate::meta::Meta,
    item_id: u32,
    reconstructed_w: u32,
    reconstructed_h: u32,
) -> (u32, u32) {
    let mut dims = (reconstructed_w, reconstructed_h);
    for t in transform_chain(meta, item_id) {
        dims = t.apply_dims(dims.0, dims.1);
    }
    dims
}

/// Resolve the **reconstructed-image** dimensions of an item from the
/// box-level graph alone (no AV1 decode):
///
/// * a `grid` item → its descriptor `output_width`/`output_height`;
/// * an `iovl` item → its descriptor `output_width`/`output_height`;
/// * an `iden` item → the output dimensions of its single `dimg` input
///   (recursively resolved);
/// * any other item (coded `av01`, etc.) → its `ispe` dimensions.
///
/// Returns `None` when the dimensions cannot be determined: a coded item
/// without an `ispe`, a `grid`/`iovl` whose descriptor payload can't be
/// resolved or parsed, an `iden` without exactly one input, or a derivation
/// chain that exceeds [`MAX_DERIVATION_DEPTH`] (cycle guard). `idat`/`mdat`
/// resolution of the `grid`/`iovl` descriptor bytes uses `file_bytes` and
/// the optional `idat` slice via the standard `iloc` resolver.
pub fn reconstructed_dims(
    meta: &crate::meta::Meta,
    item_id: u32,
    file_bytes: &[u8],
    idat: Option<&[u8]>,
) -> Option<(u32, u32)> {
    reconstructed_dims_inner(meta, item_id, file_bytes, idat, 0)
}

/// Resolve a derived-item descriptor's payload bytes (`grid` / `iovl` body)
/// from its `iloc` entry, handling both `construction_method == 0`
/// (file-offset, the bytes live in `mdat`/elsewhere in `file_bytes`) and
/// `construction_method == 1` (idat-offset, the bytes live in the `meta`
/// box's `idat` and are passed in via `idat`). Returns `None` when the
/// item has no `iloc`, when an idat extent is referenced without an `idat`
/// slice, or when any extent runs past the backing buffer.
///
/// Spec: ISO/IEC 14496-12 §8.11.3 (`iloc` construction methods); HEIF
/// §6.6.2 derived-item payloads conventionally use `idat` (cm=1).
fn resolve_descriptor_bytes(
    meta: &crate::meta::Meta,
    item_id: u32,
    file_bytes: &[u8],
    idat: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let loc = meta.location_by_id(item_id)?;
    let backing: &[u8] = match loc.construction_method {
        0 => file_bytes,
        1 => idat?,
        // construction_method 2 (item-offset) is not resolved here.
        _ => return None,
    };
    if loc.extents.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for e in &loc.extents {
        let start = loc.base_offset.checked_add(e.offset)?;
        let end = start.checked_add(e.length)?;
        let (start, end) = (usize::try_from(start).ok()?, usize::try_from(end).ok()?);
        if end > backing.len() || start > end {
            return None;
        }
        out.extend_from_slice(&backing[start..end]);
    }
    Some(out)
}

/// Maximum derivation-chain recursion depth before [`reconstructed_dims`]
/// gives up. A well-formed file nests at most a handful of derivations; a
/// deeper chain is almost certainly a `dimg` cycle, which this bound breaks
/// without unbounded recursion.
pub const MAX_DERIVATION_DEPTH: u32 = 16;

fn reconstructed_dims_inner(
    meta: &crate::meta::Meta,
    item_id: u32,
    file_bytes: &[u8],
    idat: Option<&[u8]>,
    depth: u32,
) -> Option<(u32, u32)> {
    if depth > MAX_DERIVATION_DEPTH {
        return None;
    }
    let item = meta.item_by_id(item_id)?;
    let grid = b(b"grid");
    let dimg = b(b"dimg");
    if item.item_type == grid {
        let bytes = resolve_descriptor_bytes(meta, item_id, file_bytes, idat)?;
        let g = crate::grid::ImageGrid::parse(&bytes).ok()?;
        return Some((g.output_width, g.output_height));
    }
    if item.item_type == crate::meta::ITEM_TYPE_IOVL {
        let bytes = resolve_descriptor_bytes(meta, item_id, file_bytes, idat)?;
        let refs = meta.iref_targets(&dimg, item_id).len();
        let o = ImageOverlay::parse(&bytes, refs).ok()?;
        return Some((o.output_width, o.output_height));
    }
    if item.item_type == crate::meta::ITEM_TYPE_IDEN {
        let inputs = meta.iref_targets(&dimg, item_id);
        if inputs.len() != 1 {
            return None;
        }
        // §6.6.2.1 + §6.3: the iden's reconstructed image is the *output
        // image* of its single input.
        let (iw, ih) = reconstructed_dims_inner(meta, inputs[0], file_bytes, idat, depth + 1)?;
        return Some(output_dims_from_reconstructed(meta, inputs[0], iw, ih));
    }
    // Coded item: reconstructed dimensions are the `ispe` extents.
    match meta.property_for(item_id, &b(b"ispe")) {
        Some(crate::meta::Property::Ispe(ispe)) => Some((ispe.width, ispe.height)),
        _ => None,
    }
}

/// Where one input image of an `iovl` overlay lands on the canvas, and what
/// portion of it is visible after clipping. All coordinates are canvas-space
/// pixels with the origin at the top-left corner (§6.6.2.2.3).
///
/// The input image occupies `[offset_x, offset_x + input_width)` ×
/// `[offset_y, offset_y + input_height)`. Per §6.6.2.2.3 a pixel is included
/// in the reconstructed image only when its canvas coordinate is in
/// `[0, output_width)` × `[0, output_height)`; the [`visible`](Self::visible)
/// rectangle is that intersection, expressed back in canvas coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayPlacement {
    /// The source image item id (from the parallel `dimg` iref `to_ids`).
    pub source_item_id: u32,
    /// Declared horizontal offset of the input's left column on the canvas.
    pub offset_x: i64,
    /// Declared vertical offset of the input's top row on the canvas.
    pub offset_y: i64,
    /// Input image output-image width (post its own transforms).
    pub input_width: u32,
    /// Input image output-image height.
    pub input_height: u32,
}

impl OverlayPlacement {
    /// The visible canvas rectangle as `(x, y, width, height)` after
    /// clipping the input against `[0, canvas_w) × [0, canvas_h)`. Returns
    /// `None` when the input lands entirely off-canvas (no visible pixels).
    pub fn visible(&self, canvas_w: u32, canvas_h: u32) -> Option<(u32, u32, u32, u32)> {
        let left = self.offset_x.max(0);
        let top = self.offset_y.max(0);
        let right = (self.offset_x + i64::from(self.input_width)).min(i64::from(canvas_w));
        let bottom = (self.offset_y + i64::from(self.input_height)).min(i64::from(canvas_h));
        if right <= left || bottom <= top {
            return None;
        }
        Some((
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        ))
    }

    /// True when the input image is wholly inside the canvas (no clipping
    /// occurs).
    pub fn fully_visible(&self, canvas_w: u32, canvas_h: u32) -> bool {
        self.offset_x >= 0
            && self.offset_y >= 0
            && self.offset_x + i64::from(self.input_width) <= i64::from(canvas_w)
            && self.offset_y + i64::from(self.input_height) <= i64::from(canvas_h)
    }

    /// True when no pixel of the input lands on the canvas.
    pub fn off_canvas(&self, canvas_w: u32, canvas_h: u32) -> bool {
        self.visible(canvas_w, canvas_h).is_none()
    }
}

/// A fully resolved `iovl` overlay derivation: the parsed descriptor plus
/// each input's resolved placement against the canvas (§6.6.2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayResolution {
    /// The `iovl` derived item id.
    pub iovl_item_id: u32,
    /// The parsed descriptor (canvas fill, output dimensions, offsets).
    pub descriptor: ImageOverlay,
    /// One placement per input, bottom-most first (layering order matches
    /// the `dimg` iref `to_ids` order, §6.6.2.2.1).
    pub placements: Vec<OverlayPlacement>,
}

impl OverlayResolution {
    /// Canvas dimensions `(output_width, output_height)`.
    pub fn canvas(&self) -> (u32, u32) {
        (self.descriptor.output_width, self.descriptor.output_height)
    }

    /// True when at least one canvas pixel is left to the fill colour — i.e.
    /// the union of every input's visible rectangle does not cover the whole
    /// canvas. Computed conservatively from a coverage scan bounded by the
    /// canvas area; for very large canvases (> 4M pixels) it returns `true`
    /// without scanning, since a partial fill is the common case and the
    /// exact answer isn't worth a multi-megabyte allocation.
    pub fn canvas_partially_filled(&self) -> bool {
        let (cw, ch) = self.canvas();
        let area = u64::from(cw) * u64::from(ch);
        if area == 0 {
            return false;
        }
        if area > 4_000_000 {
            return true;
        }
        let mut covered = vec![false; area as usize];
        for p in &self.placements {
            if let Some((x, y, w, h)) = p.visible(cw, ch) {
                for row in y..y + h {
                    let base = (u64::from(row) * u64::from(cw)) as usize;
                    for col in x..x + w {
                        covered[base + col as usize] = true;
                    }
                }
            }
        }
        covered.iter().any(|&c| !c)
    }
}

/// Resolve every `iovl` overlay item in `meta` end-to-end: parse the
/// descriptor, pair each placement with its `dimg` source item, and resolve
/// each source's output dimensions from the box graph. Returns one
/// [`OverlayResolution`] per `iovl` item, in `iinf` declaration order;
/// `iovl` items whose descriptor can't be resolved/parsed are skipped.
///
/// Spec: ISO/IEC 23008-12 §6.6.2.2. `file_bytes` + the optional `idat`
/// slice resolve the descriptor payload via the standard `iloc` resolver;
/// each input's dimensions come from [`reconstructed_dims`] +
/// [`output_dims_from_reconstructed`] so inputs that are themselves grids,
/// overlays, or transformed coded items resolve correctly.
pub fn resolve_overlays(
    meta: &crate::meta::Meta,
    file_bytes: &[u8],
    idat: Option<&[u8]>,
) -> Vec<OverlayResolution> {
    let dimg = b(b"dimg");
    let mut out = Vec::new();
    for iovl_id in meta.item_ids_of_type(&crate::meta::ITEM_TYPE_IOVL) {
        let Some(bytes) = resolve_descriptor_bytes(meta, iovl_id, file_bytes, idat) else {
            continue;
        };
        let sources = meta.iref_targets(&dimg, iovl_id);
        let Ok(descriptor) = ImageOverlay::parse(&bytes, sources.len()) else {
            continue;
        };
        let mut placements = Vec::with_capacity(descriptor.entries.len());
        for (entry, &src) in descriptor.entries.iter().zip(sources.iter()) {
            let (iw, ih) = reconstructed_dims(meta, src, file_bytes, idat)
                .map(|(w, h)| output_dims_from_reconstructed(meta, src, w, h))
                .unwrap_or((0, 0));
            placements.push(OverlayPlacement {
                source_item_id: src,
                offset_x: i64::from(entry.horizontal_offset),
                offset_y: i64::from(entry.vertical_offset),
                input_width: iw,
                input_height: ih,
            });
        }
        out.push(OverlayResolution {
            iovl_item_id: iovl_id,
            descriptor,
            placements,
        });
    }
    out
}

/// A fully resolved `iden` identity derivation: the single source item, its
/// reconstructed dimensions, the transform chain applied by the iden item
/// itself, and the resulting output dimensions (§6.6.2.1 + §6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdenResolution {
    /// The `iden` derived item id.
    pub iden_item_id: u32,
    /// The single `dimg` source item id (the image the identity derivation
    /// re-presents), or `None` when the iden does not have exactly one input.
    pub source_item_id: Option<u32>,
    /// The source's reconstructed dimensions, or `None` when unresolvable.
    pub source_dims: Option<(u32, u32)>,
    /// The dimension-affecting transformative properties on the **iden item
    /// itself**, in `ipma` order. These are the whole point of an identity
    /// derivation (§6.6.2.1 NOTE 2: e.g. a `clap` crop of the original).
    pub transforms: Vec<DimTransform>,
    /// The iden item's output dimensions after applying `transforms` to
    /// `source_dims`, or `None` when `source_dims` is `None`.
    pub output_dims: Option<(u32, u32)>,
}

/// Resolve every `iden` identity-derivation item in `meta` end-to-end:
/// find the single `dimg` source, resolve its reconstructed dimensions from
/// the box graph, collect the transformative properties on the iden item,
/// and compute the resulting output dimensions. Returns one
/// [`IdenResolution`] per `iden` item, in `iinf` declaration order.
///
/// Spec: ISO/IEC 23008-12 §6.6.2.1. The reconstructed image of an `iden`
/// item is the *output image* of its single input (§6.3); the iden's own
/// output image then applies the iden item's transformative properties.
/// Because the source's output dimensions already fold in *its* transforms,
/// `source_dims` here is the source's output image, and `output_dims` folds
/// the iden's transforms on top.
pub fn resolve_iden_derivations(
    meta: &crate::meta::Meta,
    file_bytes: &[u8],
    idat: Option<&[u8]>,
) -> Vec<IdenResolution> {
    let dimg = b(b"dimg");
    let mut out = Vec::new();
    for iden_id in meta.item_ids_of_type(&crate::meta::ITEM_TYPE_IDEN) {
        let inputs = meta.iref_targets(&dimg, iden_id);
        let source_item_id = if inputs.len() == 1 {
            Some(inputs[0])
        } else {
            None
        };
        let source_dims = source_item_id.and_then(|src| {
            reconstructed_dims(meta, src, file_bytes, idat)
                .map(|(w, h)| output_dims_from_reconstructed(meta, src, w, h))
        });
        let transforms = transform_chain(meta, iden_id);
        let output_dims = source_dims.map(|(w, h)| {
            let mut d = (w, h);
            for t in &transforms {
                d = t.apply_dims(d.0, d.1);
            }
            d
        });
        out.push(IdenResolution {
            iden_item_id: iden_id,
            source_item_id,
            source_dims,
            transforms,
            output_dims,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16-bit `iovl` with two source images stacked at `(10, 20)` and
    /// `(30, 40)` on a 256×256 canvas filled white-opaque.
    #[test]
    fn iovl_parses_two_entries_16bit_fields() {
        let mut buf = Vec::new();
        buf.push(0); // version
        buf.push(0); // flags = 0 → 16-bit fields
        for v in [65535u16, 65535, 65535, 65535] {
            buf.extend_from_slice(&v.to_be_bytes());
        }
        buf.extend_from_slice(&256u16.to_be_bytes()); // output_width
        buf.extend_from_slice(&256u16.to_be_bytes()); // output_height
        buf.extend_from_slice(&10i16.to_be_bytes()); // h
        buf.extend_from_slice(&20i16.to_be_bytes()); // v
        buf.extend_from_slice(&30i16.to_be_bytes()); // h
        buf.extend_from_slice(&40i16.to_be_bytes()); // v
        let o = ImageOverlay::parse(&buf, 2).unwrap();
        assert_eq!(o.canvas_fill_value, [65535, 65535, 65535, 65535]);
        assert_eq!(o.output_width, 256);
        assert_eq!(o.output_height, 256);
        assert_eq!(
            o.entries,
            vec![
                OverlayEntry {
                    horizontal_offset: 10,
                    vertical_offset: 20
                },
                OverlayEntry {
                    horizontal_offset: 30,
                    vertical_offset: 40
                }
            ]
        );
    }

    /// 32-bit field variant (`flags & 1 == 1`) — needed for canvases
    /// larger than 65535 pixels.
    #[test]
    fn iovl_parses_32bit_fields() {
        let mut buf = Vec::new();
        buf.push(0); // version
        buf.push(1); // flags = 1 → 32-bit fields
        for v in [0u16, 0, 0, 0] {
            buf.extend_from_slice(&v.to_be_bytes());
        }
        buf.extend_from_slice(&100_000u32.to_be_bytes()); // output_width
        buf.extend_from_slice(&80_000u32.to_be_bytes()); // output_height
        buf.extend_from_slice(&(-5i32).to_be_bytes()); // h (negative clips)
        buf.extend_from_slice(&10_000i32.to_be_bytes()); // v
        let o = ImageOverlay::parse(&buf, 1).unwrap();
        assert_eq!(o.output_width, 100_000);
        assert_eq!(o.output_height, 80_000);
        assert_eq!(o.entries[0].horizontal_offset, -5);
        assert_eq!(o.entries[0].vertical_offset, 10_000);
    }

    /// Negative `horizontal_offset` (signed) decoded correctly in
    /// 16-bit mode — a placement intentionally clipped at the left
    /// edge.
    #[test]
    fn iovl_negative_offset_signed_round_trip() {
        let mut buf = Vec::new();
        buf.push(0);
        buf.push(0);
        for _ in 0..4 {
            buf.extend_from_slice(&0u16.to_be_bytes());
        }
        buf.extend_from_slice(&64u16.to_be_bytes());
        buf.extend_from_slice(&64u16.to_be_bytes());
        buf.extend_from_slice(&(-3i16).to_be_bytes());
        buf.extend_from_slice(&(-4i16).to_be_bytes());
        let o = ImageOverlay::parse(&buf, 1).unwrap();
        assert_eq!(o.entries[0].horizontal_offset, -3);
        assert_eq!(o.entries[0].vertical_offset, -4);
    }

    /// `iovl` with `reference_count` larger than payload is rejected
    /// before allocating.
    #[test]
    fn iovl_rejects_oversized_reference_count() {
        let mut buf = Vec::new();
        buf.push(0);
        buf.push(0);
        for _ in 0..4 {
            buf.extend_from_slice(&0u16.to_be_bytes());
        }
        buf.extend_from_slice(&16u16.to_be_bytes());
        buf.extend_from_slice(&16u16.to_be_bytes());
        // Payload only has room for 0 entries; claim 100.
        assert!(ImageOverlay::parse(&buf, 100).is_err());
    }

    /// `iovl` rejects unrecognised versions.
    #[test]
    fn iovl_rejects_nonzero_version() {
        let buf = vec![1u8, 0]; // version=1
        assert!(ImageOverlay::parse(&buf, 0).is_err());
    }

    /// Build a minimal `grpl` containing one `altr` group with three
    /// alternate item ids.
    fn build_grpl_altr() -> Vec<u8> {
        let mut buf = Vec::new();
        // EntityToGroupBox: size(4) + 'altr' + FullBox(v=0,f=0) + group_id(4) + count(4) + ids
        let mut child = vec![0u8; 4]; // FullBox
        child.extend_from_slice(&42u32.to_be_bytes()); // group_id
        child.extend_from_slice(&3u32.to_be_bytes()); // num_entities
        child.extend_from_slice(&1u32.to_be_bytes());
        child.extend_from_slice(&2u32.to_be_bytes());
        child.extend_from_slice(&3u32.to_be_bytes());
        let size = (8 + child.len()) as u32;
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"altr");
        buf.extend_from_slice(&child);
        buf
    }

    /// `parse_grpl` extracts an `altr` group and surfaces its entity
    /// list in declaration order.
    #[test]
    fn grpl_parses_altr_group() {
        let grpl = build_grpl_altr();
        let groups = parse_grpl(&grpl).unwrap();
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(&g.grouping_type, b"altr");
        assert!(g.is_alternates());
        assert!(!g.is_stereo_pair());
        assert!(!g.is_equivalence());
        assert!(!g.is_panorama());
        assert_eq!(g.group_id, 42);
        assert_eq!(g.entity_ids, vec![1, 2, 3]);
    }

    /// `pano` group (HEIF §6.8.8.1): entities listed in increasing
    /// panorama order; the helper classifies the grouping type.
    #[test]
    fn grpl_parses_pano_group() {
        let mut buf = Vec::new();
        let mut child = vec![0u8; 4]; // FullBox
        child.extend_from_slice(&5u32.to_be_bytes()); // group_id
        child.extend_from_slice(&3u32.to_be_bytes()); // num_entities
        child.extend_from_slice(&21u32.to_be_bytes()); // first in panorama order
        child.extend_from_slice(&22u32.to_be_bytes());
        child.extend_from_slice(&23u32.to_be_bytes());
        let size = (8 + child.len()) as u32;
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"pano");
        buf.extend_from_slice(&child);
        let groups = parse_grpl(&buf).unwrap();
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert!(g.is_panorama());
        assert!(!g.is_alternates());
        assert!(!g.is_stereo_pair());
        assert!(!g.is_equivalence());
        assert_eq!(g.entity_ids, vec![21, 22, 23]);
    }

    /// `ster` group convention: two entities, first is left view.
    #[test]
    fn grpl_parses_ster_pair() {
        let mut buf = Vec::new();
        let mut child = vec![0u8; 4]; // FullBox
        child.extend_from_slice(&7u32.to_be_bytes()); // group_id
        child.extend_from_slice(&2u32.to_be_bytes()); // num_entities
        child.extend_from_slice(&10u32.to_be_bytes()); // left view
        child.extend_from_slice(&11u32.to_be_bytes()); // right view
        let size = (8 + child.len()) as u32;
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"ster");
        buf.extend_from_slice(&child);
        let groups = parse_grpl(&buf).unwrap();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].is_stereo_pair());
        assert_eq!(groups[0].entity_ids, vec![10, 11]);
    }

    /// Multiple groups in one `grpl` come out in declaration order.
    #[test]
    fn grpl_parses_multiple_groups() {
        let mut buf = Vec::new();
        // altr group
        let mut a = vec![0u8; 4];
        a.extend_from_slice(&1u32.to_be_bytes());
        a.extend_from_slice(&1u32.to_be_bytes());
        a.extend_from_slice(&100u32.to_be_bytes());
        let asz = (8 + a.len()) as u32;
        buf.extend_from_slice(&asz.to_be_bytes());
        buf.extend_from_slice(b"altr");
        buf.extend_from_slice(&a);
        // eqiv group
        let mut e = vec![0u8; 4];
        e.extend_from_slice(&2u32.to_be_bytes());
        e.extend_from_slice(&0u32.to_be_bytes()); // empty group
        let esz = (8 + e.len()) as u32;
        buf.extend_from_slice(&esz.to_be_bytes());
        buf.extend_from_slice(b"eqiv");
        buf.extend_from_slice(&e);
        let groups = parse_grpl(&buf).unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups[0].is_alternates());
        assert!(groups[1].is_equivalence());
        assert!(groups[1].entity_ids.is_empty());
    }

    /// Truncated entity list is rejected before allocation overflow.
    #[test]
    fn grpl_rejects_truncated_entity_list() {
        let mut buf = Vec::new();
        let mut child = vec![0u8; 4];
        child.extend_from_slice(&1u32.to_be_bytes()); // group_id
        child.extend_from_slice(&5u32.to_be_bytes()); // claims 5 entities…
        child.extend_from_slice(&100u32.to_be_bytes()); // …but ships only 1
        let size = (8 + child.len()) as u32;
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"altr");
        buf.extend_from_slice(&child);
        assert!(parse_grpl(&buf).is_err());
    }

    /// Mif1Compliance flags every missing required box.
    #[test]
    fn mif1_compliance_missing_list() {
        let bare = Mif1Compliance::default();
        assert!(!bare.is_compliant());
        let m = bare.missing();
        // Order is fixed; every required box should appear.
        assert!(m.contains(&"hdlr"));
        assert!(m.contains(&"pitm"));
        assert!(m.contains(&"iinf"));
        assert!(m.contains(&"infe"));
        assert!(m.contains(&"iloc"));
        assert!(m.contains(&"iprp"));
    }

    /// Mif1Compliance with every required flag set reports compliant.
    #[test]
    fn mif1_compliance_full() {
        let m = Mif1Compliance {
            has_hdlr: true,
            has_pitm: true,
            has_iinf: true,
            has_iloc: true,
            has_iprp: true,
            infe_count: 1,
            claims_mif1: true,
        };
        assert!(m.is_compliant());
        assert!(m.missing().is_empty());
    }

    // ---- sato (Sample Transform Derived Image Item) -----------------

    /// Build a `sato` payload (header byte + token_count + token bytes).
    /// `bit_depth` is the 2-bit selector; `tokens` is the raw byte
    /// stream (caller is responsible for appending constant payloads).
    fn build_sato_raw(bit_depth: u8, tokens: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + tokens.len());
        // version = 0 in the high 2 bits, bit_depth in the low 2.
        buf.push(bit_depth & 0x03);
        assert!(tokens.iter().filter(|&&t| t != 0).count() <= 255);
        // token_count = number of *tokens* (not bytes); the caller has
        // already serialised constant payloads inline. We need the
        // count of token bytes (each token is one byte plus optional
        // constant payload), which is hard to recover from bytes
        // alone, so the helpers below count it themselves.
        // Inserted by callers via build_sato_tokens.
        buf.push(0);
        buf.extend_from_slice(tokens);
        buf
    }

    /// Build a well-formed `sato` payload with the given header
    /// `bit_depth` and an iterable of `Token`s. Constants serialise to
    /// the bit-depth-keyed byte width per spec Table 1.
    fn build_sato(bit_depth: u8, tokens: &[Token]) -> Vec<u8> {
        let const_bytes = match bit_depth & 0x03 {
            0 => 1,
            1 => 2,
            2 => 4,
            3 => 8,
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        for t in tokens {
            match t {
                Token::Constant(v) => {
                    body.push(0);
                    match const_bytes {
                        1 => body.push(*v as i8 as u8),
                        2 => body.extend_from_slice(&(*v as i16).to_be_bytes()),
                        4 => body.extend_from_slice(&(*v as i32).to_be_bytes()),
                        8 => body.extend_from_slice(&v.to_be_bytes()),
                        _ => unreachable!(),
                    }
                }
                Token::Sample(n) => body.push(*n),
                Token::Unary(raw) | Token::Binary(raw) | Token::Reserved(raw) => body.push(*raw),
            }
        }
        let mut buf = build_sato_raw(bit_depth, &body);
        buf[1] = tokens.len() as u8;
        buf
    }

    /// Round-trip a minimal constant-only expression at every supported
    /// bit_depth (8 / 16 / 32 / 64 bits intermediate).
    #[test]
    fn sato_parses_constant_only_each_bit_depth() {
        for (bd, expected_bits) in [(0, 8u32), (1, 16), (2, 32), (3, 64)] {
            let payload = build_sato(bd, &[Token::Constant(-1)]);
            let st = SampleTransform::parse(&payload, 0).unwrap();
            assert_eq!(st.version, 0);
            assert_eq!(st.bit_depth, bd);
            assert_eq!(st.num_bits(), expected_bits);
            assert_eq!(st.tokens, vec![Token::Constant(-1)]);
            // Evaluating with no inputs yields the constant.
            assert_eq!(st.evaluate(&[]).unwrap(), -1);
        }
    }

    /// A two-sample postfix sum expression — Sample(1), Sample(2),
    /// Binary(128) — evaluates to L + R.
    #[test]
    fn sato_evaluates_sum_of_two_samples() {
        let payload = build_sato(0, &[Token::Sample(1), Token::Sample(2), Token::Binary(128)]);
        let st = SampleTransform::parse(&payload, 2).unwrap();
        assert_eq!(st.tokens.len(), 3);
        assert_eq!(st.evaluate(&[40, 25]).unwrap(), 65);
        // Overflow clamps to max_value for 8-bit intermediate.
        assert_eq!(st.evaluate(&[120, 120]).unwrap(), 127);
    }

    /// Difference is right-popped first — av1-avif Table 2 row 129.
    /// `Sample(1) Sample(2) Binary(129)` is `L - R = inputs[0] -
    /// inputs[1]`.
    #[test]
    fn sato_evaluates_difference_with_right_pop_first() {
        let payload = build_sato(
            1, // 16-bit intermediate avoids 8-bit clamps
            &[Token::Sample(1), Token::Sample(2), Token::Binary(129)],
        );
        let st = SampleTransform::parse(&payload, 2).unwrap();
        assert_eq!(st.evaluate(&[100, 30]).unwrap(), 70);
        assert_eq!(st.evaluate(&[30, 100]).unwrap(), -70);
    }

    /// Worked example from av1-avif Appendix A — combine an 8-bit MSB
    /// item and an 8-bit residual to produce a 16-bit result via
    /// `(msb << 8) | residual` ≡ Sample(1) Constant(256) * Sample(2) +.
    /// Verified via the `or` operator (bitwise) — spec accepts either.
    #[test]
    fn sato_evaluates_msb_residual_recombine() {
        // Tokens: Sample(1), Constant(8), Binary(135 pow → 2^8=256),
        //         Binary(130 product), Sample(2), Binary(128 sum)
        let payload = build_sato(
            1, // 16-bit intermediate
            &[
                Token::Sample(1),
                Token::Constant(2),
                Token::Constant(8),
                Token::Binary(135), // 2^8 = 256
                Token::Binary(130), // sample(1) * 256
                Token::Sample(2),
                Token::Binary(128), // + sample(2)
            ],
        );
        let st = SampleTransform::parse(&payload, 2).unwrap();
        // msb=0x12, lsb=0x34 → 0x1234 = 4660
        assert_eq!(st.evaluate(&[0x12, 0x34]).unwrap(), 0x1234);
    }

    /// Unary `negation` (token 64) flips the sign of the top of stack.
    #[test]
    fn sato_evaluates_unary_negation() {
        let payload = build_sato(1, &[Token::Sample(1), Token::Unary(64)]);
        let st = SampleTransform::parse(&payload, 1).unwrap();
        assert_eq!(st.evaluate(&[42]).unwrap(), -42);
    }

    /// Unary `bsr` (token 67) returns the MSB index for L > 0, else 0.
    #[test]
    fn sato_evaluates_unary_bsr() {
        let payload = build_sato(1, &[Token::Sample(1), Token::Unary(67)]);
        let st = SampleTransform::parse(&payload, 1).unwrap();
        assert_eq!(st.evaluate(&[1]).unwrap(), 0); // log2(1) = 0
        assert_eq!(st.evaluate(&[2]).unwrap(), 1);
        assert_eq!(st.evaluate(&[255]).unwrap(), 7);
        assert_eq!(st.evaluate(&[0]).unwrap(), 0); // spec: 0 for L <= 0
        assert_eq!(st.evaluate(&[-5]).unwrap(), 0);
    }

    /// Quotient (token 131) returns L for R == 0, else L/R truncated
    /// toward zero.
    #[test]
    fn sato_evaluates_quotient_with_zero_divisor_returns_left() {
        let payload = build_sato(1, &[Token::Sample(1), Token::Sample(2), Token::Binary(131)]);
        let st = SampleTransform::parse(&payload, 2).unwrap();
        assert_eq!(st.evaluate(&[10, 3]).unwrap(), 3); // 10/3 → 3
        assert_eq!(st.evaluate(&[-10, 3]).unwrap(), -3); // truncate toward zero
        assert_eq!(st.evaluate(&[10, 0]).unwrap(), 10); // R == 0 → L
    }

    /// Pow (token 135) returns 0 for L == 0, otherwise L^R truncated.
    #[test]
    fn sato_evaluates_pow_zero_base() {
        let payload = build_sato(1, &[Token::Sample(1), Token::Sample(2), Token::Binary(135)]);
        let st = SampleTransform::parse(&payload, 2).unwrap();
        assert_eq!(st.evaluate(&[0, 5]).unwrap(), 0);
        assert_eq!(st.evaluate(&[2, 3]).unwrap(), 8);
        assert_eq!(st.evaluate(&[3, 0]).unwrap(), 1);
    }

    /// Min / max (tokens 136 / 137) pick the appropriate operand.
    #[test]
    fn sato_evaluates_min_max() {
        let min_p = build_sato(1, &[Token::Sample(1), Token::Sample(2), Token::Binary(136)]);
        let max_p = build_sato(1, &[Token::Sample(1), Token::Sample(2), Token::Binary(137)]);
        let min_st = SampleTransform::parse(&min_p, 2).unwrap();
        let max_st = SampleTransform::parse(&max_p, 2).unwrap();
        assert_eq!(min_st.evaluate(&[10, 3]).unwrap(), 3);
        assert_eq!(max_st.evaluate(&[10, 3]).unwrap(), 10);
        assert_eq!(min_st.evaluate(&[-5, -2]).unwrap(), -5);
        assert_eq!(max_st.evaluate(&[-5, -2]).unwrap(), -2);
    }

    /// `token_count = 0` is rejected per av1-avif §4.2.3.3 assert
    /// `66976029`.
    #[test]
    fn sato_rejects_zero_token_count() {
        let buf = vec![0u8, 0]; // header, token_count=0
        let err = SampleTransform::parse(&buf, 0).unwrap_err();
        assert!(format!("{err:?}").contains("token_count"));
    }

    /// version != 0 is rejected by the strict parser (spec: "Readers
    /// shall ignore" — we map that to an error so the caller decides).
    #[test]
    fn sato_rejects_nonzero_version() {
        let mut buf = build_sato(0, &[Token::Constant(0)]);
        buf[0] = 1u8 << 6; // version=1
        assert!(SampleTransform::parse(&buf, 0).is_err());
        // parse_relaxed still surfaces it.
        let relaxed = SampleTransform::parse_relaxed(&buf).unwrap();
        assert_eq!(relaxed.version, 1);
    }

    /// A sample-operand token whose value exceeds reference_count is
    /// rejected per av1-avif §4.2.3.4 assert `1f569fa5`.
    #[test]
    fn sato_rejects_sample_index_over_reference_count() {
        let payload = build_sato(0, &[Token::Sample(5)]);
        assert!(SampleTransform::parse(&payload, 3).is_err());
        assert!(SampleTransform::parse(&payload, 5).is_ok());
    }

    /// Reserved token values (33..=63, 68..=127, 138..=255) are
    /// rejected by `parse` per av1-avif §4.2.3.3.
    #[test]
    fn sato_rejects_reserved_token_values() {
        for raw in [33u8, 50, 63, 68, 100, 127, 138, 200, 255] {
            let payload = build_sato_raw(0, &[raw]);
            // Fix token_count to 1.
            let mut p = payload;
            p[1] = 1;
            assert!(
                SampleTransform::parse(&p, 32).is_err(),
                "expected error for reserved token {raw}"
            );
            // parse_relaxed surfaces it as a Reserved variant.
            let relaxed = SampleTransform::parse_relaxed(&p).unwrap();
            assert_eq!(relaxed.tokens, vec![Token::Reserved(raw)]);
        }
    }

    /// A binary operator on a single-operand stack is caught by the
    /// stack-discipline check.
    #[test]
    fn sato_rejects_binary_op_without_enough_operands() {
        let payload = build_sato(0, &[Token::Sample(1), Token::Binary(128)]);
        assert!(SampleTransform::parse(&payload, 32).is_err());
    }

    /// An expression that leaves more than one element on the stack
    /// is rejected (av1-avif §4.2.3.4 assert `bac41e3a`).
    #[test]
    fn sato_rejects_expression_with_leftover_stack() {
        let payload = build_sato(0, &[Token::Sample(1), Token::Sample(2)]);
        let err = SampleTransform::parse(&payload, 32).unwrap_err();
        assert!(format!("{err:?}").contains("leaves 2"));
    }

    /// Truncated payload (claims `token_count = 3` but only ships 1
    /// token byte) is rejected.
    #[test]
    fn sato_rejects_truncated_token_stream() {
        let payload = vec![0u8, 3, 1]; // header, token_count=3, only one token
        assert!(SampleTransform::parse(&payload, 32).is_err());
        assert!(SampleTransform::parse_relaxed(&payload).is_err());
    }

    /// Constant payload that runs off the end of the buffer is
    /// rejected (8-byte constant at `bit_depth=3` but only 4 bytes
    /// remaining).
    #[test]
    fn sato_rejects_truncated_constant_payload() {
        // bit_depth=3 → 8-byte constants
        let mut buf = vec![3u8, 1, 0]; // bit_depth=3, token_count=1, token=Constant
        buf.extend_from_slice(&[0u8, 0, 0, 0]); // only 4 bytes of the needed 8
        assert!(SampleTransform::parse(&buf, 0).is_err());
    }

    /// Number-of-bits mapping matches av1-avif Table 1 verbatim.
    #[test]
    fn sato_num_bits_table_1() {
        let mk = |bd: u8| SampleTransform {
            version: 0,
            bit_depth: bd,
            tokens: vec![Token::Constant(0)],
        };
        assert_eq!(mk(0).num_bits(), 8);
        assert_eq!(mk(1).num_bits(), 16);
        assert_eq!(mk(2).num_bits(), 32);
        assert_eq!(mk(3).num_bits(), 64);
    }

    /// min_value / max_value cover every supported bit depth.
    #[test]
    fn sato_min_max_value_per_bit_depth() {
        let mk = |bd: u8| SampleTransform {
            version: 0,
            bit_depth: bd,
            tokens: vec![Token::Constant(0)],
        };
        assert_eq!(mk(0).min_value(), -128);
        assert_eq!(mk(0).max_value(), 127);
        assert_eq!(mk(1).min_value(), -32_768);
        assert_eq!(mk(1).max_value(), 32_767);
        assert_eq!(mk(2).min_value(), i32::MIN as i64);
        assert_eq!(mk(2).max_value(), i32::MAX as i64);
        assert_eq!(mk(3).min_value(), i64::MIN);
        assert_eq!(mk(3).max_value(), i64::MAX);
    }

    /// `Token::is_operand` / `is_unary` / `is_binary` classification
    /// matches the variant the parser yields.
    #[test]
    fn sato_token_classification_helpers() {
        assert!(Token::Constant(0).is_operand());
        assert!(Token::Sample(1).is_operand());
        assert!(!Token::Unary(64).is_operand());
        assert!(!Token::Binary(128).is_operand());
        assert!(Token::Unary(64).is_unary());
        assert!(!Token::Binary(128).is_unary());
        assert!(Token::Binary(128).is_binary());
        assert!(!Token::Unary(64).is_binary());
    }

    /// `evaluate` errors when given fewer inputs than the expression
    /// requires (defence in depth — parse() also enforces the
    /// reference_count constraint).
    #[test]
    fn sato_evaluate_errors_when_inputs_short() {
        let payload = build_sato(0, &[Token::Sample(3)]);
        let st = SampleTransform::parse(&payload, 3).unwrap();
        let err = st.evaluate(&[1, 2]).unwrap_err();
        assert!(format!("{err:?}").contains("out of range"));
    }

    // -------------------------------------------------------------------
    // Tone Map compliance audit (av1-avif v1.2.0 §4.2.2)
    // -------------------------------------------------------------------

    use crate::meta::{IrefEntry, ItemInfo, Meta};

    fn make_infe(id: u32, item_type: &[u8; 4], flags: u32) -> ItemInfo {
        ItemInfo {
            id,
            item_type: *item_type,
            name: String::new(),
            content_type: None,
            content_encoding: None,
            item_uri_type: None,
            flags,
        }
    }

    /// Build a `grpl` payload containing one `altr` EntityToGroupBox
    /// over the given ids.
    fn build_altr_grpl(group_id: u32, ids: &[u32]) -> Vec<u8> {
        let mut child = vec![0u8; 4]; // FullBox(version=0, flags=0)
        child.extend_from_slice(&group_id.to_be_bytes());
        child.extend_from_slice(&(ids.len() as u32).to_be_bytes());
        for id in ids {
            child.extend_from_slice(&id.to_be_bytes());
        }
        let size = (8 + child.len()) as u32;
        let mut buf = Vec::new();
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"altr");
        buf.extend_from_slice(&child);
        buf
    }

    /// Happy path: one tmap item with one base (no gain map), grouped
    /// with the base by an `altr` entity group. Compliant.
    #[test]
    fn audit_tone_map_altr_pairing_compliant() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0), // base, visible
                make_infe(2, b"tmap", 0), // tmap derived item
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 2,
                to_ids: vec![1],
            }],
            grpl: Some(build_altr_grpl(7, &[1, 2])),
            ..Meta::default()
        };
        let results = audit_tone_map(&meta);
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.tmap_item_id, 2);
        assert_eq!(r.base_item_id, Some(1));
        assert!(r.gain_map_item_ids.is_empty());
        assert!(r.paired_in_altr);
        assert!(r.gain_maps_hidden);
        assert!(r.is_compliant());
        assert!(r.missing().is_empty());
    }

    /// tmap + base + gain map, where the gain map is hidden as the
    /// spec recommends. Should pass when there's also an `altr` group.
    #[test]
    fn audit_tone_map_hidden_gain_map_compliant() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0),    // base, visible
                make_infe(2, b"av01", 0x01), // gain map, hidden
                make_infe(3, b"tmap", 0),    // tmap derived item
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 3,
                to_ids: vec![1, 2],
            }],
            grpl: Some(build_altr_grpl(9, &[1, 3])),
            ..Meta::default()
        };
        let r = &audit_tone_map(&meta)[0];
        assert_eq!(r.base_item_id, Some(1));
        assert_eq!(r.gain_map_item_ids, vec![2]);
        assert!(r.paired_in_altr);
        assert!(r.gain_maps_hidden);
        assert!(r.is_compliant());
    }

    /// No `grpl` at all → `altr` pairing fails; visible gain map →
    /// gain-map hidden check fails. Both `missing` entries surface.
    #[test]
    fn audit_tone_map_flags_both_failures() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0), // base, visible
                make_infe(2, b"av01", 0), // gain map NOT hidden
                make_infe(3, b"tmap", 0),
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 3,
                to_ids: vec![1, 2],
            }],
            grpl: None,
            ..Meta::default()
        };
        let r = &audit_tone_map(&meta)[0];
        assert!(!r.paired_in_altr);
        assert!(!r.gain_maps_hidden);
        assert!(!r.is_compliant());
        let m = r.missing();
        assert!(m.contains(&"altr-pairs-base-and-tmap"));
        assert!(m.contains(&"gain-map-hidden"));
    }

    /// `grpl` present but the `altr` group lists only the base — the
    /// pairing check should still fail when the tmap id is absent.
    #[test]
    fn audit_tone_map_altr_without_tmap_id_fails_pairing() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"tmap", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 2,
                to_ids: vec![1],
            }],
            grpl: Some(build_altr_grpl(11, &[1, 99])), // tmap not in altr
            ..Meta::default()
        };
        let r = &audit_tone_map(&meta)[0];
        assert!(!r.paired_in_altr);
        // No gain map → hidden check trivially true.
        assert!(r.gain_maps_hidden);
        assert!(!r.is_compliant());
    }

    /// A tmap with no `dimg` iref at all surfaces `base_item_id =
    /// None` and fails the altr-pairing check (nothing to pair with).
    #[test]
    fn audit_tone_map_no_dimg_iref() {
        let meta = Meta {
            items: vec![make_infe(2, b"tmap", 0)],
            irefs: vec![],
            grpl: None,
            ..Meta::default()
        };
        let r = &audit_tone_map(&meta)[0];
        assert_eq!(r.base_item_id, None);
        assert!(r.gain_map_item_ids.is_empty());
        assert!(!r.paired_in_altr);
        // No gain map → hidden check trivially true.
        assert!(r.gain_maps_hidden);
    }

    /// File with no `tmap` items returns an empty audit list.
    #[test]
    fn audit_tone_map_empty_when_no_tmap_items() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            ..Meta::default()
        };
        assert!(audit_tone_map(&meta).is_empty());
    }

    /// Multiple `tmap` items audited in `iinf` declaration order.
    #[test]
    fn audit_tone_map_reports_each_tmap_item() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0),
                make_infe(2, b"tmap", 0),
                make_infe(3, b"av01", 0x01), // hidden
                make_infe(4, b"tmap", 0),
            ],
            irefs: vec![
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 2,
                    to_ids: vec![1],
                },
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 4,
                    to_ids: vec![1, 3],
                },
            ],
            grpl: Some(build_altr_grpl(1, &[1, 2, 4])),
            ..Meta::default()
        };
        let r = audit_tone_map(&meta);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].tmap_item_id, 2);
        assert_eq!(r[1].tmap_item_id, 4);
        assert!(r[0].is_compliant());
        assert!(r[1].is_compliant());
    }

    /// `ItemInfo::is_hidden` honours `flags & 0x01` and ignores higher
    /// bits.
    #[test]
    fn item_info_is_hidden_reads_low_bit_only() {
        assert!(!make_infe(1, b"av01", 0).is_hidden());
        assert!(make_infe(1, b"av01", 0x01).is_hidden());
        // High bits set but bit 0 clear → not hidden.
        assert!(!make_infe(1, b"av01", 0xfffe).is_hidden());
        // Mixed: bit 0 set + other bits set → hidden.
        assert!(make_infe(1, b"av01", 0xff03).is_hidden());
    }

    // -------------------------------------------------------------------
    // Grid derivation chain — transformative-property audit (av1-avif §7)
    // -------------------------------------------------------------------

    use crate::meta::{Clap, Imir, Irot, ItemPropertyAssociation, Property, PropertyAssociation};

    /// Sample Clap value — arbitrary, the audit only cares about presence.
    fn sample_clap() -> Clap {
        Clap {
            clean_aperture_width_n: 1,
            clean_aperture_width_d: 1,
            clean_aperture_height_n: 1,
            clean_aperture_height_d: 1,
            horiz_off_n: 0,
            horiz_off_d: 1,
            vert_off_n: 0,
            vert_off_d: 1,
        }
    }

    fn assoc(item_id: u32, indices: &[u16]) -> ItemPropertyAssociation {
        ItemPropertyAssociation {
            item_id,
            entries: indices
                .iter()
                .map(|i| PropertyAssociation {
                    index: *i,
                    essential: false,
                })
                .collect(),
        }
    }

    /// Compliant grid: no tile carries a transformative property — the
    /// grid item may itself carry `irot` / `imir` / `clap` (we put `irot`
    /// on the grid here to prove the audit ignores grid-level transforms).
    #[test]
    fn audit_grid_derivations_clean_chain_compliant() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"grid", 0),
                make_infe(2, b"av01", 0),
                make_infe(3, b"av01", 0),
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 1,
                to_ids: vec![2, 3],
            }],
            // Index 0: irot on the grid item (permitted by §7).
            properties: vec![Property::Irot(Irot { angle: 1 })],
            associations: vec![assoc(1, &[0])],
            ..Meta::default()
        };
        let r = audit_grid_derivations(&meta);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].grid_item_id, 1);
        assert_eq!(r[0].tile_item_ids, vec![2, 3]);
        assert!(
            r[0].offenders.is_empty(),
            "grid-level transforms must not flag the chain"
        );
        assert!(r[0].is_compliant());
        assert!(r[0].offending_tile_ids().is_empty());
    }

    /// Non-compliant grid: a tile carries `irot`. The audit reports the
    /// offending tile + `irot` kind, and `is_compliant` flips to false.
    #[test]
    fn audit_grid_derivations_tile_irot_flagged() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"grid", 0),
                make_infe(2, b"av01", 0),
                make_infe(3, b"av01", 0),
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 1,
                to_ids: vec![2, 3],
            }],
            properties: vec![Property::Irot(Irot { angle: 2 })],
            // Tile 3 carries the property — chain violation.
            associations: vec![assoc(3, &[0])],
            ..Meta::default()
        };
        let r = &audit_grid_derivations(&meta)[0];
        assert!(!r.is_compliant());
        assert_eq!(r.offenders, vec![(3, *b"irot")]);
        assert_eq!(r.offending_tile_ids(), vec![3]);
    }

    /// A single tile carrying all three transformative kinds surfaces
    /// three offender entries in the stable `(clap, irot, imir)` order
    /// the audit produces.
    #[test]
    fn audit_grid_derivations_tile_with_all_four_kinds() {
        let meta = Meta {
            items: vec![make_infe(1, b"grid", 0), make_infe(2, b"av01", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 1,
                to_ids: vec![2],
            }],
            properties: vec![
                Property::Clap(sample_clap()),
                Property::Irot(Irot { angle: 1 }),
                Property::Imir(Imir { axis: 0 }),
                Property::Iscl(crate::meta::Iscl {
                    target_width_numerator: 1,
                    target_width_denominator: 2,
                    target_height_numerator: 1,
                    target_height_denominator: 2,
                }),
            ],
            associations: vec![assoc(2, &[0, 1, 2, 3])],
            ..Meta::default()
        };
        let r = &audit_grid_derivations(&meta)[0];
        assert_eq!(
            r.offenders,
            vec![(2, *b"clap"), (2, *b"irot"), (2, *b"imir"), (2, *b"iscl"),]
        );
        // Tile id is unique even though it offends four times.
        assert_eq!(r.offending_tile_ids(), vec![2]);
    }

    /// HEIF §6.5.13 `'iscl'` is a transformative item property; av1-avif
    /// §7 forbids any transformative property on a `'dimg'` input tile.
    /// A solo `'iscl'` on a tile must surface as an offender.
    #[test]
    fn audit_grid_derivations_tile_iscl_flagged() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"grid", 0),
                make_infe(2, b"av01", 0),
                make_infe(3, b"av01", 0),
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 1,
                to_ids: vec![2, 3],
            }],
            properties: vec![Property::Iscl(crate::meta::Iscl {
                target_width_numerator: 1,
                target_width_denominator: 2,
                target_height_numerator: 1,
                target_height_denominator: 2,
            })],
            // Only tile 2 carries the iscl; tile 3 is clean.
            associations: vec![assoc(2, &[0])],
            ..Meta::default()
        };
        let r = &audit_grid_derivations(&meta)[0];
        assert!(!r.is_compliant());
        assert_eq!(r.offenders, vec![(2, *b"iscl")]);
        assert_eq!(r.offending_tile_ids(), vec![2]);
    }

    /// HEIF §6.5.17 `'rref'` is descriptive (not transformative). A
    /// tile carrying `'rref'` must NOT be reported by the §7 audit —
    /// the §7 `shall` is scoped to transformative properties only.
    #[test]
    fn audit_grid_derivations_tile_rref_not_flagged() {
        let meta = Meta {
            items: vec![make_infe(1, b"grid", 0), make_infe(2, b"av01", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 1,
                to_ids: vec![2],
            }],
            properties: vec![Property::Rref(crate::meta::Rref {
                reference_types: vec![*b"dimg"],
            })],
            associations: vec![assoc(2, &[0])],
            ..Meta::default()
        };
        let r = &audit_grid_derivations(&meta)[0];
        assert!(r.is_compliant());
        assert!(r.offenders.is_empty());
    }

    /// An `'iscl'` on the grid item itself is explicitly permitted by
    /// §7 ("transformations are only permitted on the grid item
    /// itself"), so it must not surface as an offender.
    #[test]
    fn audit_grid_derivations_grid_level_iscl_permitted() {
        let meta = Meta {
            items: vec![make_infe(1, b"grid", 0), make_infe(2, b"av01", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 1,
                to_ids: vec![2],
            }],
            properties: vec![Property::Iscl(crate::meta::Iscl {
                target_width_numerator: 1,
                target_width_denominator: 2,
                target_height_numerator: 1,
                target_height_denominator: 2,
            })],
            // Iscl associated with the grid (id 1), tile 2 clean.
            associations: vec![assoc(1, &[0])],
            ..Meta::default()
        };
        let r = &audit_grid_derivations(&meta)[0];
        assert!(r.is_compliant());
        assert!(r.offenders.is_empty());
    }

    /// Two tiles offending in different ways. Both surface; the unique
    /// tile-id list collapses duplicates.
    #[test]
    fn audit_grid_derivations_multiple_offending_tiles() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"grid", 0),
                make_infe(2, b"av01", 0),
                make_infe(3, b"av01", 0),
                make_infe(4, b"av01", 0),
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 1,
                to_ids: vec![2, 3, 4],
            }],
            properties: vec![
                Property::Imir(Imir { axis: 1 }),
                Property::Clap(sample_clap()),
            ],
            associations: vec![
                // Tile 3 carries imir; tile 4 carries clap; tile 2 clean.
                assoc(3, &[0]),
                assoc(4, &[1]),
            ],
            ..Meta::default()
        };
        let r = &audit_grid_derivations(&meta)[0];
        assert!(!r.is_compliant());
        // clap-on-4 sorts before imir-on-3 in the per-tile loop:
        // we walk tiles in dimg order (2, 3, 4) and emit (clap, irot, imir)
        // per tile, so the result is [(3, imir), (4, clap)].
        assert_eq!(r.offenders, vec![(3, *b"imir"), (4, *b"clap")]);
        assert_eq!(r.offending_tile_ids(), vec![3, 4]);
    }

    /// File with no `grid` items returns an empty audit list — the
    /// constraint is vacuous and the strict-compliant predicate folds
    /// to true.
    #[test]
    fn audit_grid_derivations_empty_when_no_grid_items() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            ..Meta::default()
        };
        assert!(audit_grid_derivations(&meta).is_empty());
    }

    /// Multiple grid items each get their own audit record, in `iinf`
    /// declaration order.
    #[test]
    fn audit_grid_derivations_reports_each_grid_item() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"grid", 0), // first grid
                make_infe(2, b"av01", 0),
                make_infe(3, b"av01", 0),
                make_infe(4, b"grid", 0), // second grid
                make_infe(5, b"av01", 0),
            ],
            irefs: vec![
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 1,
                    to_ids: vec![2, 3],
                },
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 4,
                    to_ids: vec![5],
                },
            ],
            properties: vec![Property::Irot(Irot { angle: 3 })],
            // Tile 5 (second grid's tile) carries an irot.
            associations: vec![assoc(5, &[0])],
            ..Meta::default()
        };
        let r = audit_grid_derivations(&meta);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].grid_item_id, 1);
        assert!(r[0].is_compliant());
        assert_eq!(r[1].grid_item_id, 4);
        assert!(!r[1].is_compliant());
        assert_eq!(r[1].offenders, vec![(5, *b"irot")]);
    }

    /// A grid item with no `dimg` iref still emits an audit record with
    /// empty tile + offender vectors. The constraint is trivially
    /// satisfied (no chain → no offending tiles); a malformed grid is
    /// caught elsewhere.
    #[test]
    fn audit_grid_derivations_grid_without_dimg_is_compliant() {
        let meta = Meta {
            items: vec![make_infe(1, b"grid", 0)],
            ..Meta::default()
        };
        let r = &audit_grid_derivations(&meta)[0];
        assert_eq!(r.grid_item_id, 1);
        assert!(r.tile_item_ids.is_empty());
        assert!(r.is_compliant());
    }

    // -------------------------------------------------------------------
    // Identity (`iden`) compliance audit (HEIF §6.6.2.1 + §6.6.1)
    // -------------------------------------------------------------------

    use crate::meta::{IlocExtent, ItemLocation};

    /// Build a placeholder [`ItemLocation`] with a single non-empty
    /// extent — useful for the `has_item_body == true` (non-conformant)
    /// shape.
    fn make_iloc_with_body(id: u32) -> ItemLocation {
        ItemLocation {
            id,
            construction_method: 0,
            data_reference_index: 0,
            base_offset: 0,
            extents: vec![IlocExtent {
                offset: 1000,
                length: 16,
                extent_index: 0,
            }],
        }
    }

    /// `ItemLocation` with one extent whose `length == 0`. HEIF §6.6.2.1
    /// requires the iden to have no body; a zero-length extent is
    /// equivalent (no actual bytes) and the audit treats it as compliant.
    fn make_iloc_zero_length(id: u32) -> ItemLocation {
        ItemLocation {
            id,
            construction_method: 0,
            data_reference_index: 0,
            base_offset: 0,
            extents: vec![IlocExtent {
                offset: 0,
                length: 0,
                extent_index: 0,
            }],
        }
    }

    /// Happy path: one `'iden'` item with exactly one `'dimg'` input,
    /// exactly one `'dimg'` iref entry for the iden's `from_item_ID`,
    /// and no `'iloc'` entry at all (so trivially no body).
    #[test]
    fn audit_iden_compliant_no_iloc_entry() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0), // source
                make_infe(2, b"iden", 0), // identity derivation
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 2,
                to_ids: vec![1],
            }],
            ..Meta::default()
        };
        let r = audit_iden_derivations(&meta);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].iden_item_id, 2);
        assert_eq!(r[0].dimg_reference_count, 1);
        assert_eq!(r[0].dimg_iref_count, 1);
        assert!(!r[0].has_item_body);
        assert_eq!(r[0].source_item_id, Some(1));
        assert!(r[0].is_compliant());
        assert!(r[0].missing().is_empty());
    }

    /// An `'iden'` with an `'iloc'` entry that carries a zero-length
    /// extent passes the "no item body" check — the spec defines "body"
    /// in terms of actual byte content, not the presence of an iloc
    /// entry. Compliant.
    #[test]
    fn audit_iden_compliant_iloc_with_zero_length_extent() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"iden", 0)],
            locations: vec![make_iloc_zero_length(2)],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 2,
                to_ids: vec![1],
            }],
            ..Meta::default()
        };
        let r = &audit_iden_derivations(&meta)[0];
        assert!(!r.has_item_body);
        assert!(r.is_compliant());
    }

    /// HEIF §6.6.2.1: an `'iden'` item with a non-empty extent has a
    /// body, which is forbidden. The audit flags it.
    #[test]
    fn audit_iden_flags_non_empty_item_body() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"iden", 0)],
            locations: vec![make_iloc_with_body(2)],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 2,
                to_ids: vec![1],
            }],
            ..Meta::default()
        };
        let r = &audit_iden_derivations(&meta)[0];
        assert!(r.has_item_body);
        assert!(!r.is_compliant());
        assert!(r.missing().contains(&"no-item-body"));
    }

    /// HEIF §6.6.2.1: `reference_count` of an `'iden'`'s `'dimg'` iref
    /// shall be exactly 1. A zero-input iden fails.
    #[test]
    fn audit_iden_flags_zero_dimg_inputs() {
        let meta = Meta {
            items: vec![make_infe(2, b"iden", 0)],
            irefs: vec![],
            ..Meta::default()
        };
        let r = &audit_iden_derivations(&meta)[0];
        assert_eq!(r.dimg_reference_count, 0);
        assert_eq!(r.dimg_iref_count, 0);
        assert_eq!(r.source_item_id, None);
        assert!(!r.is_compliant());
        let m = r.missing();
        assert!(m.contains(&"dimg-reference-count-eq-1"));
        assert!(m.contains(&"dimg-iref-count-eq-1"));
    }

    /// HEIF §6.6.2.1: `reference_count` of an `'iden'`'s `'dimg'` iref
    /// shall be exactly 1. Two inputs fail. (HEIF §6.6.1 separately
    /// forbids multiple iref entries with the same `from_item_ID`; this
    /// case is one entry with reference_count = 2, so §6.6.1 is OK but
    /// §6.6.2.1 is not.)
    #[test]
    fn audit_iden_flags_two_dimg_inputs_in_single_iref() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0),
                make_infe(2, b"av01", 0),
                make_infe(3, b"iden", 0),
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 3,
                to_ids: vec![1, 2],
            }],
            ..Meta::default()
        };
        let r = &audit_iden_derivations(&meta)[0];
        assert_eq!(r.dimg_reference_count, 2);
        // Single iref entry → §6.6.1 still satisfied even though it
        // carries two to_ids.
        assert_eq!(r.dimg_iref_count, 1);
        assert_eq!(r.source_item_id, None);
        assert!(!r.is_compliant());
        assert!(r.missing().contains(&"dimg-reference-count-eq-1"));
        assert!(!r.missing().contains(&"dimg-iref-count-eq-1"));
    }

    /// HEIF §6.6.1: number of `'dimg'` SingleItemTypeReferenceBoxes with
    /// the same `from_item_ID` shall be at most 1. Two separate iref
    /// entries fail.
    #[test]
    fn audit_iden_flags_multiple_dimg_iref_entries() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0),
                make_infe(2, b"av01", 0),
                make_infe(3, b"iden", 0),
            ],
            irefs: vec![
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 3,
                    to_ids: vec![1],
                },
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 3,
                    to_ids: vec![2],
                },
            ],
            ..Meta::default()
        };
        let r = &audit_iden_derivations(&meta)[0];
        assert_eq!(r.dimg_iref_count, 2);
        // `iref_targets` returns the first matching entry only, so the
        // observed reference_count is 1 (from entry 0). §6.6.1 violation
        // is reported via `dimg_iref_count`.
        assert_eq!(r.dimg_reference_count, 1);
        assert!(!r.is_compliant());
        assert!(r.missing().contains(&"dimg-iref-count-eq-1"));
    }

    /// File with no `'iden'` items returns an empty audit list. Other
    /// derived items (`grid`, `tmap`, `sato`, `iovl`) are not picked up.
    #[test]
    fn audit_iden_empty_when_no_iden_items() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0),
                make_infe(2, b"grid", 0),
                make_infe(3, b"tmap", 0),
                make_infe(4, b"sato", 0),
                make_infe(5, b"iovl", 0),
            ],
            ..Meta::default()
        };
        assert!(audit_iden_derivations(&meta).is_empty());
    }

    /// Multiple `'iden'` items are audited in `iinf` declaration order.
    /// One compliant + one with a forbidden item body — the two records
    /// surface independently with the right ids.
    #[test]
    fn audit_iden_reports_each_iden_item_in_iinf_order() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0),
                make_infe(2, b"iden", 0), // compliant
                make_infe(3, b"av01", 0),
                make_infe(4, b"iden", 0), // has body → non-compliant
            ],
            locations: vec![make_iloc_with_body(4)],
            irefs: vec![
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 2,
                    to_ids: vec![1],
                },
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 4,
                    to_ids: vec![3],
                },
            ],
            ..Meta::default()
        };
        let r = audit_iden_derivations(&meta);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].iden_item_id, 2);
        assert!(r[0].is_compliant());
        assert_eq!(r[1].iden_item_id, 4);
        assert!(!r[1].is_compliant());
        assert!(r[1].has_item_body);
    }

    /// A non-`'dimg'` iref of the same shape (e.g. `'cdsc'`) must not
    /// be counted by the audit — only `'dimg'` references contribute to
    /// HEIF §6.6.2.1 / §6.6.1 input enumeration.
    #[test]
    fn audit_iden_ignores_non_dimg_irefs() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0),
                make_infe(2, b"iden", 0),
                make_infe(9, b"Exif", 0),
            ],
            irefs: vec![
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 2,
                    to_ids: vec![1],
                },
                // `cdsc` from a metadata item — irrelevant to iden input.
                IrefEntry {
                    reference_type: *b"cdsc",
                    from_id: 9,
                    to_ids: vec![2],
                },
            ],
            ..Meta::default()
        };
        let r = &audit_iden_derivations(&meta)[0];
        assert_eq!(r.dimg_iref_count, 1);
        assert_eq!(r.dimg_reference_count, 1);
        assert!(r.is_compliant());
    }

    // -------------------------------------------------------------------
    // AV1 Alpha bit-depth match audit (av1-avif v1.2.0 §4.1)
    // -------------------------------------------------------------------

    use crate::meta::{AuxC, ItemPropertyAssociation as Ipa, PropertyAssociation as Pa};

    /// Build an `av1C` payload whose first three bytes carry the
    /// `(high_bitdepth, twelve_bit)` flag pair for the given bit depth.
    /// Bit 7 is the `marker` (always 1), bits 6..0 of byte 0 are
    /// `version` (always 1) — but the audit only consults byte 2, so
    /// we just zero the first two bytes here.
    fn av1c_with_bit_depth(bit_depth: u8) -> Vec<u8> {
        let (high, twelve) = match bit_depth {
            8 => (0u8, 0u8),
            10 => (1, 0),
            12 => (1, 1),
            _ => panic!("test helper supports 8/10/12 only"),
        };
        // byte 0: marker(1) + version(7) — recreated nominally as 0x81.
        // byte 1: profile/seq_level — irrelevant to bit depth.
        // byte 2: high_bitdepth(1)<<6 | twelve_bit(1)<<5 | rest.
        let b2 = (high << 6) | (twelve << 5);
        vec![0x81, 0x00, b2]
    }

    fn ipa(item_id: u32, indices: &[u16]) -> Ipa {
        Ipa {
            item_id,
            entries: indices
                .iter()
                .map(|i| Pa {
                    index: *i,
                    essential: false,
                })
                .collect(),
        }
    }

    fn make_alpha_auxc() -> AuxC {
        AuxC {
            aux_type: crate::alpha::ALPHA_URN_PREFIX.to_string(),
            aux_subtype: Vec::new(),
        }
    }

    fn make_depth_auxc() -> AuxC {
        AuxC {
            aux_type: crate::meta::AUX_URN_DEPTH_MPEG.to_string(),
            aux_subtype: Vec::new(),
        }
    }

    /// `decode_av1c_bit_depth` covers the three legal AV1 depths.
    #[test]
    fn decode_av1c_bit_depth_recognises_8_10_12() {
        assert_eq!(decode_av1c_bit_depth(&av1c_with_bit_depth(8)), Some(8));
        assert_eq!(decode_av1c_bit_depth(&av1c_with_bit_depth(10)), Some(10));
        assert_eq!(decode_av1c_bit_depth(&av1c_with_bit_depth(12)), Some(12));
    }

    /// Truncated `av1C` (< 3 bytes) is rejected without panicking.
    #[test]
    fn decode_av1c_bit_depth_handles_truncation() {
        assert_eq!(decode_av1c_bit_depth(&[]), None);
        assert_eq!(decode_av1c_bit_depth(&[0x81]), None);
        assert_eq!(decode_av1c_bit_depth(&[0x81, 0x00]), None);
    }

    /// Happy path: 8-bit master + 8-bit alpha → compliant.
    #[test]
    fn audit_alpha_bit_depth_match_compliant() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0), // master
                make_infe(2, b"av01", 0), // alpha
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"auxl",
                from_id: 2,
                to_ids: vec![1],
            }],
            properties: vec![
                Property::Av1C(av1c_with_bit_depth(8)), // index 0 — master av1C
                Property::Av1C(av1c_with_bit_depth(8)), // index 1 — alpha av1C
                Property::AuxC(make_alpha_auxc()),      // index 2
            ],
            associations: vec![ipa(1, &[0]), ipa(2, &[1, 2])],
            ..Meta::default()
        };
        let r = audit_alpha_bit_depth(&meta);
        assert_eq!(r.len(), 1);
        let rec = &r[0];
        assert_eq!(rec.alpha_item_id, 2);
        assert_eq!(rec.master_item_id, 1);
        assert_eq!(rec.alpha_bit_depth, Some(8));
        assert_eq!(rec.master_bit_depth, Some(8));
        assert!(!rec.alpha_missing_av1c);
        assert!(!rec.master_missing_av1c);
        assert!(rec.is_compliant());
        assert!(rec.missing().is_empty());
    }

    /// 10-bit master + 8-bit alpha → §4.1 mismatch flagged.
    #[test]
    fn audit_alpha_bit_depth_mismatch_flagged() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"av01", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"auxl",
                from_id: 2,
                to_ids: vec![1],
            }],
            properties: vec![
                Property::Av1C(av1c_with_bit_depth(10)), // master 10-bit
                Property::Av1C(av1c_with_bit_depth(8)),  // alpha 8-bit
                Property::AuxC(make_alpha_auxc()),
            ],
            associations: vec![ipa(1, &[0]), ipa(2, &[1, 2])],
            ..Meta::default()
        };
        let rec = &audit_alpha_bit_depth(&meta)[0];
        assert_eq!(rec.alpha_bit_depth, Some(8));
        assert_eq!(rec.master_bit_depth, Some(10));
        assert!(!rec.is_compliant());
        assert_eq!(rec.missing(), vec!["alpha-master-bit-depth-mismatch"]);
    }

    /// 12-bit on both sides also matches.
    #[test]
    fn audit_alpha_bit_depth_12_bit_compliant() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"av01", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"auxl",
                from_id: 2,
                to_ids: vec![1],
            }],
            properties: vec![
                Property::Av1C(av1c_with_bit_depth(12)),
                Property::Av1C(av1c_with_bit_depth(12)),
                Property::AuxC(make_alpha_auxc()),
            ],
            associations: vec![ipa(1, &[0]), ipa(2, &[1, 2])],
            ..Meta::default()
        };
        let rec = &audit_alpha_bit_depth(&meta)[0];
        assert!(rec.is_compliant());
    }

    /// Alpha item missing `av1C` surfaces both the missing-av1c flag
    /// and the corresponding `missing()` entry. The §2.1 violation
    /// (every AV1 Image Item shall carry `av1C`) is reported alongside
    /// the §4.1 check.
    #[test]
    fn audit_alpha_bit_depth_alpha_missing_av1c() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"av01", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"auxl",
                from_id: 2,
                to_ids: vec![1],
            }],
            properties: vec![
                Property::Av1C(av1c_with_bit_depth(10)),
                Property::AuxC(make_alpha_auxc()),
            ],
            // Alpha item 2 has only the auxC, no av1C.
            associations: vec![ipa(1, &[0]), ipa(2, &[1])],
            ..Meta::default()
        };
        let rec = &audit_alpha_bit_depth(&meta)[0];
        assert!(rec.alpha_missing_av1c);
        assert_eq!(rec.alpha_bit_depth, None);
        assert!(!rec.is_compliant());
        assert_eq!(rec.missing(), vec!["alpha-item-missing-av1C"]);
    }

    /// Master item missing `av1C` surfaces the corresponding flag +
    /// `missing()` entry.
    #[test]
    fn audit_alpha_bit_depth_master_missing_av1c() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"av01", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"auxl",
                from_id: 2,
                to_ids: vec![1],
            }],
            properties: vec![
                Property::Av1C(av1c_with_bit_depth(10)),
                Property::AuxC(make_alpha_auxc()),
            ],
            // Master item 1 has no av1C; alpha carries both.
            associations: vec![ipa(2, &[0, 1])],
            ..Meta::default()
        };
        let rec = &audit_alpha_bit_depth(&meta)[0];
        assert!(rec.master_missing_av1c);
        assert_eq!(rec.master_bit_depth, None);
        assert!(!rec.is_compliant());
        assert_eq!(rec.missing(), vec!["master-item-missing-av1C"]);
    }

    /// Truncated `av1C` payload on the alpha side surfaces as
    /// `alpha-item-av1C-truncated` (distinct from the missing case).
    #[test]
    fn audit_alpha_bit_depth_truncated_av1c() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"av01", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"auxl",
                from_id: 2,
                to_ids: vec![1],
            }],
            properties: vec![
                Property::Av1C(av1c_with_bit_depth(10)),
                // Alpha av1C is truncated to 2 bytes — present-but-unusable.
                Property::Av1C(vec![0x81, 0x00]),
                Property::AuxC(make_alpha_auxc()),
            ],
            associations: vec![ipa(1, &[0]), ipa(2, &[1, 2])],
            ..Meta::default()
        };
        let rec = &audit_alpha_bit_depth(&meta)[0];
        assert!(!rec.alpha_missing_av1c);
        assert_eq!(rec.alpha_bit_depth, None);
        assert!(!rec.is_compliant());
        assert_eq!(rec.missing(), vec!["alpha-item-av1C-truncated"]);
    }

    /// Non-alpha auxiliaries (depth maps, gain maps) are out of §4.1's
    /// scope and must not surface in the audit. A depth-map auxC is the
    /// canonical confound — the same `auxl` iref + a non-alpha URN
    /// means the audit walks past.
    #[test]
    fn audit_alpha_bit_depth_skips_depth_map_auxiliary() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"av01", 0)],
            irefs: vec![IrefEntry {
                reference_type: *b"auxl",
                from_id: 2,
                to_ids: vec![1],
            }],
            properties: vec![
                Property::Av1C(av1c_with_bit_depth(10)),
                Property::Av1C(av1c_with_bit_depth(8)),
                Property::AuxC(make_depth_auxc()), // depth, not alpha
            ],
            associations: vec![ipa(1, &[0]), ipa(2, &[1, 2])],
            ..Meta::default()
        };
        assert!(audit_alpha_bit_depth(&meta).is_empty());
    }

    /// A single alpha item declared against multiple masters in one
    /// `auxl` `to_ids` list emits one record per master pairing, in
    /// `to_ids` order. The §4.1 `shall` applies per pairing.
    #[test]
    fn audit_alpha_bit_depth_one_record_per_master() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0), // master A, 8-bit
                make_infe(2, b"av01", 0), // master B, 10-bit
                make_infe(3, b"av01", 0), // alpha, 10-bit
            ],
            irefs: vec![IrefEntry {
                reference_type: *b"auxl",
                from_id: 3,
                to_ids: vec![1, 2],
            }],
            properties: vec![
                Property::Av1C(av1c_with_bit_depth(8)),  // master A
                Property::Av1C(av1c_with_bit_depth(10)), // master B
                Property::Av1C(av1c_with_bit_depth(10)), // alpha
                Property::AuxC(make_alpha_auxc()),
            ],
            associations: vec![ipa(1, &[0]), ipa(2, &[1]), ipa(3, &[2, 3])],
            ..Meta::default()
        };
        let r = audit_alpha_bit_depth(&meta);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].master_item_id, 1);
        assert!(!r[0].is_compliant()); // 10 vs 8 → mismatch
        assert_eq!(r[1].master_item_id, 2);
        assert!(r[1].is_compliant()); // 10 vs 10 → ok
    }

    /// Files with no alpha auxiliaries emit no records.
    #[test]
    fn audit_alpha_bit_depth_empty_when_no_alpha() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            irefs: Vec::new(),
            properties: vec![Property::Av1C(av1c_with_bit_depth(10))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        assert!(audit_alpha_bit_depth(&meta).is_empty());
    }

    /// Multiple alpha auxiliaries (one per master in a multi-image
    /// collection) emit one record per `(alpha, master)` pair in iref
    /// declaration order.
    #[test]
    fn audit_alpha_bit_depth_multiple_alpha_items() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"av01", 0),
                make_infe(2, b"av01", 0),
                make_infe(3, b"av01", 0),
                make_infe(4, b"av01", 0),
            ],
            irefs: vec![
                IrefEntry {
                    reference_type: *b"auxl",
                    from_id: 3,
                    to_ids: vec![1],
                },
                IrefEntry {
                    reference_type: *b"auxl",
                    from_id: 4,
                    to_ids: vec![2],
                },
            ],
            properties: vec![
                Property::Av1C(av1c_with_bit_depth(8)),
                Property::Av1C(av1c_with_bit_depth(10)),
                Property::Av1C(av1c_with_bit_depth(8)),
                Property::Av1C(av1c_with_bit_depth(10)),
                Property::AuxC(make_alpha_auxc()),
            ],
            associations: vec![ipa(1, &[0]), ipa(2, &[1]), ipa(3, &[2, 4]), ipa(4, &[3, 4])],
            ..Meta::default()
        };
        let r = audit_alpha_bit_depth(&meta);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].alpha_item_id, 3);
        assert_eq!(r[0].master_item_id, 1);
        assert!(r[0].is_compliant());
        assert_eq!(r[1].alpha_item_id, 4);
        assert_eq!(r[1].master_item_id, 2);
        assert!(r[1].is_compliant());
    }

    // -------------------------------------------------------------------
    // §2.1 Sequence Header OBU count audit
    // -------------------------------------------------------------------

    /// Build one OBU framed per AV1 §5.3.1/§5.3.2 with `obu_has_size_field
    /// == 1` and no extension header. `obu_type` goes in bits 6..3.
    /// `payload` is the OBU body bytes (their content doesn't matter for
    /// the audit — only the type and the framing are inspected).
    fn obu_with_size(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        // header byte: 0|type(4)|ext(0)|has_size(1)|reserved(0)
        let hdr = ((obu_type & 0x0f) << 3) | 0b10;
        let mut out = vec![hdr];
        // leb128 encode payload length.
        let mut v = payload.len() as u32;
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
                out.push(b);
            } else {
                out.push(b);
                break;
            }
        }
        out.extend_from_slice(payload);
        out
    }

    /// Build a one-byte OBU with `obu_has_size_field == 0` — illegal in
    /// AVIF Image Item Data per av1-avif §2.1 + AV1 §5.3.1's container
    /// chaining requirement.
    fn obu_no_size(obu_type: u8) -> Vec<u8> {
        // header byte: 0|type(4)|ext(0)|has_size(0)|reserved(0)
        vec![(obu_type & 0x0f) << 3]
    }

    /// Build a synthetic file whose iloc resolves item `1`'s payload to
    /// the given byte slice. Returns `(file_bytes, meta)`. The file
    /// bytes are just a flat buffer with the payload at offset 0 (so
    /// `iloc.base_offset = 0`, `extent.offset = 0`, `extent.length =
    /// payload.len()`).
    fn synth_av01_item(payload: &[u8]) -> (Vec<u8>, Meta) {
        use crate::meta::{IlocExtent, ItemLocation};
        let file = payload.to_vec();
        let length = file.len() as u64;
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            locations: vec![ItemLocation {
                id: 1,
                construction_method: 0,
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![IlocExtent {
                    offset: 0,
                    length,
                    extent_index: 0,
                }],
            }],
            ..Meta::default()
        };
        (file, meta)
    }

    /// leb128 round-trip on small + large values (within 32-bit range).
    #[test]
    fn read_leb128_decodes_single_and_multi_byte_values() {
        // single-byte: 0x05 → 5 in 1 byte
        assert_eq!(read_leb128(&[0x05, 0xff]), Ok((5, 1)));
        // two-byte: 0x80 0x01 → 128 in 2 bytes
        assert_eq!(read_leb128(&[0x80, 0x01, 0xff]), Ok((128, 2)));
        // five-byte: largest u32: 0xff ff ff ff 0f
        assert_eq!(
            read_leb128(&[0xff, 0xff, 0xff, 0xff, 0x0f]),
            Ok((0xffff_ffff, 5))
        );
    }

    /// Truncation (continuation bit set on last byte) errors out.
    #[test]
    fn read_leb128_rejects_truncated_continuation() {
        assert!(read_leb128(&[0x80]).is_err());
        assert!(read_leb128(&[0x80, 0x80, 0x80]).is_err());
    }

    /// A leb128 sequence longer than 8 bytes errors out (the §4.10.5
    /// loop bound).
    #[test]
    fn read_leb128_rejects_overlong_sequence() {
        assert!(read_leb128(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80]).is_err());
    }

    /// Happy path: one Sequence Header + one Temporal Delimiter + one
    /// Frame OBU → `sequence_header_count == 1`, compliant.
    #[test]
    fn audit_sequence_header_obu_single_count_compliant() {
        let mut stream = Vec::new();
        stream.extend(obu_with_size(2, &[])); // OBU_TEMPORAL_DELIMITER
        stream.extend(obu_with_size(1, &[0x00, 0x01, 0x02])); // OBU_SEQUENCE_HEADER
        stream.extend(obu_with_size(6, &[0xab; 7])); // OBU_FRAME
        let (file, meta) = synth_av01_item(&stream);
        let r = audit_sequence_header_obu(&meta, &file);
        assert_eq!(r.len(), 1);
        let rec = &r[0];
        assert_eq!(rec.item_id, 1);
        assert_eq!(rec.sequence_header_count, 1);
        assert_eq!(rec.total_obu_count, 3);
        assert!(rec.is_compliant());
        assert!(rec.missing().is_empty());
    }

    /// Two Sequence Headers in one item → §2.1 violation, flagged as
    /// `av01-item-multiple-sequence-header-obus`.
    #[test]
    fn audit_sequence_header_obu_two_count_flagged() {
        let mut stream = Vec::new();
        stream.extend(obu_with_size(1, &[0x00, 0x01])); // SH #1
        stream.extend(obu_with_size(1, &[0x00, 0x01])); // SH #2
        let (file, meta) = synth_av01_item(&stream);
        let rec = &audit_sequence_header_obu(&meta, &file)[0];
        assert_eq!(rec.sequence_header_count, 2);
        assert!(!rec.is_compliant());
        assert_eq!(
            rec.missing(),
            vec!["av01-item-multiple-sequence-header-obus"]
        );
    }

    /// Zero Sequence Headers → §2.1 violation, flagged as
    /// `av01-item-missing-sequence-header-obu`.
    #[test]
    fn audit_sequence_header_obu_zero_count_flagged() {
        let mut stream = Vec::new();
        stream.extend(obu_with_size(2, &[])); // TD only
        stream.extend(obu_with_size(6, &[0xab; 4])); // FRAME
        let (file, meta) = synth_av01_item(&stream);
        let rec = &audit_sequence_header_obu(&meta, &file)[0];
        assert_eq!(rec.sequence_header_count, 0);
        assert_eq!(rec.total_obu_count, 2);
        assert!(!rec.is_compliant());
        assert_eq!(rec.missing(), vec!["av01-item-missing-sequence-header-obu"]);
    }

    /// `obu_has_size_field == 0` in a chained stream → walker stops and
    /// reports `has_size_field_zero`. Per AV1 §5.3.1, an OBU without a
    /// size field cannot be chained with subsequent OBUs.
    #[test]
    fn audit_sequence_header_obu_size_field_zero_flagged() {
        let mut stream = Vec::new();
        stream.extend(obu_no_size(1)); // SH with no size
                                       // Following bytes can't be framed — anything after.
        stream.extend([0xff, 0xff, 0xff]);
        let (file, meta) = synth_av01_item(&stream);
        let rec = &audit_sequence_header_obu(&meta, &file)[0];
        assert!(rec.has_size_field_zero);
        assert!(!rec.is_compliant());
        // The SH header byte itself was decoded before we noticed the
        // missing size — count surfaces but compliance still false.
        assert_eq!(rec.sequence_header_count, 1);
        assert!(rec.missing().contains(&"av01-item-obu-has-size-field-zero"));
    }

    /// A declared `obu_size` that runs past the item payload → walker
    /// reports `truncated_obu`.
    #[test]
    fn audit_sequence_header_obu_truncated_payload_flagged() {
        // Header for SH, claim 100-byte payload, but only give it 3.
        let mut stream = vec![(1u8 << 3) | 0b10]; // SH + has_size
        stream.push(100); // leb128(100), single byte
        stream.extend([0xde, 0xad, 0xbe]); // only 3 bytes of payload
        let (file, meta) = synth_av01_item(&stream);
        let rec = &audit_sequence_header_obu(&meta, &file)[0];
        assert!(rec.truncated_obu);
        assert!(!rec.is_compliant());
        assert!(rec.missing().contains(&"av01-item-obu-stream-truncated"));
    }

    /// Truncated leb128 mid-OBU (continuation bit set on last byte) →
    /// `truncated_obu` flagged; no OBUs walked past the bad leb.
    #[test]
    fn audit_sequence_header_obu_truncated_leb128_flagged() {
        // Header byte SH + has_size, then a single byte 0x80 (continuation
        // bit set, no follow-on) — leb128 walker errors → truncated.
        let stream = vec![(1u8 << 3) | 0b10, 0x80];
        let (file, meta) = synth_av01_item(&stream);
        let rec = &audit_sequence_header_obu(&meta, &file)[0];
        assert!(rec.truncated_obu);
        assert!(!rec.is_compliant());
        // Header byte was readable, but the leb128 framing failure
        // bailed before the type-credit step — SH counter does not
        // fire because we cannot confirm we successfully framed an OBU.
        assert_eq!(rec.sequence_header_count, 0);
        assert_eq!(rec.total_obu_count, 0);
    }

    /// `iinf` lists an av01 item but no `iloc` resolves it → missing_iloc.
    #[test]
    fn audit_sequence_header_obu_missing_iloc_flagged() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            // no locations
            ..Meta::default()
        };
        let r = audit_sequence_header_obu(&meta, &[]);
        assert_eq!(r.len(), 1);
        let rec = &r[0];
        assert!(rec.missing_iloc);
        assert!(!rec.is_compliant());
        assert_eq!(rec.missing(), vec!["av01-item-missing-iloc"]);
    }

    /// Non-av01 items (e.g. `Exif`, `iden`, `grid`) are ignored.
    #[test]
    fn audit_sequence_header_obu_ignores_non_av01_items() {
        let meta = Meta {
            items: vec![
                make_infe(1, b"Exif", 0),
                make_infe(2, b"iden", 0),
                make_infe(3, b"grid", 0),
            ],
            ..Meta::default()
        };
        assert!(audit_sequence_header_obu(&meta, &[]).is_empty());
    }

    /// An `obu_extension_flag == 1` OBU correctly skips the extension
    /// header byte before reading `obu_size`.
    #[test]
    fn audit_sequence_header_obu_handles_extension_header() {
        // header byte: SH(1)|ext(1)|has_size(1)|reserved(0)
        let hdr = (1u8 << 3) | 0b110;
        let mut stream = vec![hdr];
        stream.push(0x00); // extension header byte (temporal_id=0, spatial_id=0)
        stream.push(2); // leb128(2)
        stream.extend([0x11, 0x22]); // payload
        let (file, meta) = synth_av01_item(&stream);
        let rec = &audit_sequence_header_obu(&meta, &file)[0];
        assert_eq!(rec.sequence_header_count, 1);
        assert_eq!(rec.total_obu_count, 1);
        assert!(rec.is_compliant());
    }

    /// Empty `av01` payload: 0 OBUs walked, 0 SH OBUs → flagged as
    /// missing SH.
    #[test]
    fn audit_sequence_header_obu_empty_payload_flagged_missing() {
        let (file, meta) = synth_av01_item(&[]);
        let rec = &audit_sequence_header_obu(&meta, &file)[0];
        assert_eq!(rec.sequence_header_count, 0);
        assert_eq!(rec.total_obu_count, 0);
        assert!(!rec.missing_iloc);
        assert!(!rec.truncated_obu);
        assert!(!rec.is_compliant());
        assert_eq!(rec.missing(), vec!["av01-item-missing-sequence-header-obu"]);
    }

    /// Multiple AV1 Image Items in the same file each get their own
    /// audit record, in `item_ids_of_type` (declaration) order.
    #[test]
    fn audit_sequence_header_obu_one_record_per_av01_item() {
        use crate::meta::{IlocExtent, ItemLocation};
        // Build two items back-to-back in the file: item 1 compliant
        // (one SH), item 2 non-compliant (zero SH).
        let mut s1 = Vec::new();
        s1.extend(obu_with_size(2, &[])); // TD
        s1.extend(obu_with_size(1, &[0x00; 3])); // SH
        let mut s2 = Vec::new();
        s2.extend(obu_with_size(2, &[])); // TD only — no SH
        let mut file = Vec::new();
        let off1 = file.len() as u64;
        file.extend(&s1);
        let off2 = file.len() as u64;
        file.extend(&s2);
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"av01", 0)],
            locations: vec![
                ItemLocation {
                    id: 1,
                    construction_method: 0,
                    data_reference_index: 0,
                    base_offset: 0,
                    extents: vec![IlocExtent {
                        offset: off1,
                        length: s1.len() as u64,
                        extent_index: 0,
                    }],
                },
                ItemLocation {
                    id: 2,
                    construction_method: 0,
                    data_reference_index: 0,
                    base_offset: 0,
                    extents: vec![IlocExtent {
                        offset: off2,
                        length: s2.len() as u64,
                        extent_index: 0,
                    }],
                },
            ],
            ..Meta::default()
        };
        let r = audit_sequence_header_obu(&meta, &file);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].item_id, 1);
        assert!(r[0].is_compliant());
        assert_eq!(r[1].item_id, 2);
        assert!(!r[1].is_compliant());
        assert_eq!(r[1].sequence_header_count, 0);
    }

    // -- ISO 21496-1 Annex C.2 gain map metadata --------------------------

    /// Push a `numerator(int32) / denominator(uint32)` rational pair onto
    /// a big-endian payload buffer.
    fn push_rational(buf: &mut Vec<u8>, num: i32, den: u32) {
        buf.extend_from_slice(&(num as u32).to_be_bytes());
        buf.extend_from_slice(&den.to_be_bytes());
    }

    /// Build a single-channel (`is_multichannel == 0`) gain map metadata
    /// payload with deterministic, distinguishable rational values.
    fn build_singlechannel_gain_map() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u16.to_be_bytes()); // minimum_version
        buf.extend_from_slice(&0u16.to_be_bytes()); // writer_version
        buf.push(0x40); // is_multichannel=0, use_base_colour_space=1
        push_rational(&mut buf, 5, 2); // base_hdr_headroom
        push_rational(&mut buf, 8, 1); // alternate_hdr_headroom
                                       // one GainMapChannel
        push_rational(&mut buf, -1, 4); // gain_map_min
        push_rational(&mut buf, 3, 4); // gain_map_max
        push_rational(&mut buf, 1, 1); // gamma
        push_rational(&mut buf, -2, 16); // base_offset
        push_rational(&mut buf, 7, 16); // alternate_offset
        buf
    }

    /// Single-channel payload parses with the right flags, headroom, and
    /// one channel record; the rationals round-trip exactly.
    #[test]
    fn gain_map_metadata_parses_single_channel() {
        let buf = build_singlechannel_gain_map();
        let m = GainMapMetadata::parse(&buf).unwrap();
        assert_eq!(m.minimum_version, 0);
        assert_eq!(m.writer_version, 0);
        assert!(!m.is_multichannel);
        assert!(m.use_base_colour_space);
        assert_eq!(m.channel_count(), 1);
        assert_eq!(
            m.base_hdr_headroom,
            GainMapRational {
                numerator: 5,
                denominator: 2
            }
        );
        assert_eq!(
            m.alternate_hdr_headroom,
            GainMapRational {
                numerator: 8,
                denominator: 1
            }
        );
        let c = m.channels[0];
        assert_eq!(
            c.gain_map_min,
            GainMapRational {
                numerator: -1,
                denominator: 4
            }
        );
        assert_eq!(c.gain_map_max.as_f64(), 0.75);
        assert_eq!(
            c.gamma,
            GainMapRational {
                numerator: 1,
                denominator: 1
            }
        );
        assert_eq!(c.base_offset.as_f64(), -0.125);
        assert_eq!(c.alternate_offset.numerator, 7);
    }

    /// `is_multichannel == 1` yields three channel records (R, G, B
    /// order) read from 3 × 40 trailing bytes.
    #[test]
    fn gain_map_metadata_parses_three_channels_when_multichannel() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u16.to_be_bytes()); // minimum_version
        buf.extend_from_slice(&1u16.to_be_bytes()); // writer_version
        buf.push(0x80); // is_multichannel=1, use_base_colour_space=0
        push_rational(&mut buf, 1, 1); // base_hdr_headroom
                                       // alternate_hdr_headroom must differ from base_hdr_headroom
                                       // (§5.2.7); 4/1 ≠ 1/1.
        push_rational(&mut buf, 4, 1); // alternate_hdr_headroom
        for ch in 0..3i32 {
            push_rational(&mut buf, ch, 1); // gain_map_min (R=0,G=1,B=2)
            push_rational(&mut buf, 10, 1); // gain_map_max
            push_rational(&mut buf, 2, 1); // gamma
            push_rational(&mut buf, 0, 1); // base_offset
            push_rational(&mut buf, 0, 1); // alternate_offset
        }
        let m = GainMapMetadata::parse(&buf).unwrap();
        assert!(m.is_multichannel);
        assert!(!m.use_base_colour_space);
        assert_eq!(m.channel_count(), 3);
        assert_eq!(m.channels[0].gain_map_min.numerator, 0);
        assert_eq!(m.channels[1].gain_map_min.numerator, 1);
        assert_eq!(m.channels[2].gain_map_min.numerator, 2);
    }

    /// Annex C.2.1: trailing padding / future-optional metadata after the
    /// recognised fields is ignored, not an error.
    #[test]
    fn gain_map_metadata_ignores_trailing_bytes() {
        let mut buf = build_singlechannel_gain_map();
        buf.extend_from_slice(&[0xAA; 32]); // padding + hypothetical v2 fields
        let m = GainMapMetadata::parse(&buf).unwrap();
        assert_eq!(m.channel_count(), 1);
    }

    /// Annex C.2.3: a `minimum_version` the reader doesn't understand is
    /// an `Unsupported` (display the base image), not malformed data.
    #[test]
    fn gain_map_metadata_unknown_min_version_is_unsupported() {
        let mut buf = build_singlechannel_gain_map();
        buf[0] = 0;
        buf[1] = 1; // minimum_version = 1
        let err = GainMapMetadata::parse(&buf).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
    }

    /// A zero rational denominator is rejected (C.2.3 "shall not be 0").
    #[test]
    fn gain_map_metadata_rejects_zero_denominator() {
        let mut buf = build_singlechannel_gain_map();
        // base_hdr_headroom denominator sits at offset 5+4 = 9.
        buf[9] = 0;
        buf[10] = 0;
        buf[11] = 0;
        buf[12] = 0;
        let err = GainMapMetadata::parse(&buf).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    /// `gamma_numerator == 0` is rejected (C.2.3 "gamma_numerator shall
    /// not be 0"), distinct from the denominator constraint.
    #[test]
    fn gain_map_metadata_rejects_zero_gamma_numerator() {
        let mut buf = build_singlechannel_gain_map();
        // Channel record starts at offset 21; gamma is field 3 →
        // 21 + 16 = 37, the gamma numerator (4 bytes).
        for b in buf.iter_mut().skip(37).take(4) {
            *b = 0;
        }
        let err = GainMapMetadata::parse(&buf).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    /// A payload truncated before all channel bytes arrive is rejected.
    #[test]
    fn gain_map_metadata_rejects_truncated_channel() {
        let mut buf = build_singlechannel_gain_map();
        buf.truncate(buf.len() - 1); // drop last byte of the channel
        let err = GainMapMetadata::parse(&buf).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    /// §5.2.5.3: per-component `max(G)` "shall be greater than or
    /// equal to the `min(G)` value". The default fixture has min=-1/4
    /// (-0.25) and max=3/4 (0.75), so swapping the two numerators
    /// flips the predicate and the parse must reject the payload.
    #[test]
    fn gain_map_metadata_rejects_max_below_min() {
        let mut buf = build_singlechannel_gain_map();
        // Channel record starts at offset 21; the gain_map_min and
        // gain_map_max sint32 numerators sit at +0 and +8 within it.
        // Swap them so max < min.
        let orig_min = i32::from_be_bytes(buf[21..25].try_into().unwrap());
        let orig_max = i32::from_be_bytes(buf[29..33].try_into().unwrap());
        buf[21..25].copy_from_slice(&orig_max.to_be_bytes());
        buf[29..33].copy_from_slice(&orig_min.to_be_bytes());
        let err = GainMapMetadata::parse(&buf).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    /// §5.2.5.3 boundary: `max(G) == min(G)` is permitted ("greater
    /// than or equal to"). Build a channel where both equal 0/1 and
    /// confirm the parse succeeds.
    #[test]
    fn gain_map_metadata_accepts_max_equal_to_min() {
        let mut buf = build_singlechannel_gain_map();
        // Force gain_map_min and gain_map_max to the same rational
        // (0/1, the simplest legal value).
        buf[21..25].copy_from_slice(&0i32.to_be_bytes()); // min numerator
        buf[25..29].copy_from_slice(&1u32.to_be_bytes()); // min denominator
        buf[29..33].copy_from_slice(&0i32.to_be_bytes()); // max numerator
        buf[33..37].copy_from_slice(&1u32.to_be_bytes()); // max denominator
        let m = GainMapMetadata::parse(&buf).expect("max==min is permitted");
        assert_eq!(m.channels[0].gain_map_min.numerator, 0);
        assert_eq!(m.channels[0].gain_map_max.numerator, 0);
    }

    /// §5.2.7: `alternate_hdr_headroom` "shall not be equal to" the
    /// `base_hdr_headroom`. Overwrite the alternate headroom with the
    /// base headroom's exact bytes and confirm the parse rejects it.
    #[test]
    fn gain_map_metadata_rejects_equal_hdr_headrooms() {
        let mut buf = build_singlechannel_gain_map();
        // base_hdr_headroom sits at offset 5..13; alternate at 13..21.
        let base = buf[5..13].to_vec();
        buf[13..21].copy_from_slice(&base);
        let err = GainMapMetadata::parse(&buf).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    /// §5.2.7 uses rational *value* equality, not byte equality:
    /// 1/1 and 2/2 are the same value and so must trip the check
    /// even though the encoded bytes differ.
    #[test]
    fn gain_map_metadata_rejects_value_equal_hdr_headrooms() {
        let mut buf = build_singlechannel_gain_map();
        // base = 1/1, alternate = 2/2 → both = 1.0, must reject.
        buf[5..9].copy_from_slice(&1u32.to_be_bytes()); // base numerator
        buf[9..13].copy_from_slice(&1u32.to_be_bytes()); // base denominator
        buf[13..17].copy_from_slice(&2u32.to_be_bytes()); // alt numerator
        buf[17..21].copy_from_slice(&2u32.to_be_bytes()); // alt denominator
        let err = GainMapMetadata::parse(&buf).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    // -- ISO 21496-1 §6 gain map application ------------------------------

    /// Helper to compare two floats within a tight absolute tolerance.
    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-12, "expected {b}, got {a}");
    }

    /// §6.3 Formula (3): `W == 0` at the baseline headroom, `W == 1` at
    /// the alternate headroom (positive span), and a linear interpolation
    /// in between. The single-channel fixture has H_baseline = 2.5 and
    /// H_alternate = 8.0 (span 5.5).
    #[test]
    fn weight_factor_spans_baseline_to_alternate() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        approx(m.weight_factor(2.5), 0.0); // at H_baseline → not applied
        approx(m.weight_factor(8.0), 1.0); // at H_alternate → fully applied
        approx(m.weight_factor(5.25), 0.5); // midpoint → half
                                            // Targets outside [H_baseline, H_alternate] clamp into [0, 1].
        approx(m.weight_factor(-100.0), 0.0);
        approx(m.weight_factor(100.0), 1.0);
    }

    /// §6.3 Formula (3) with a negative span (H_alternate < H_baseline):
    /// `sign(H_alternate − H_baseline)` flips W negative, and full
    /// application yields `W == -1` (§6.3 NOTE 4).
    #[test]
    fn weight_factor_is_negative_when_alternate_below_baseline() {
        // base_hdr = 5/1 = 5.0, alternate_hdr = 1/1 = 1.0 → span = -4.
        let mut buf = build_singlechannel_gain_map();
        buf[5..9].copy_from_slice(&5u32.to_be_bytes()); // base num
        buf[9..13].copy_from_slice(&1u32.to_be_bytes()); // base den
        buf[13..17].copy_from_slice(&1u32.to_be_bytes()); // alt num
        buf[17..21].copy_from_slice(&1u32.to_be_bytes()); // alt den
        let m = GainMapMetadata::parse(&buf).unwrap();
        approx(m.weight_factor(5.0), 0.0); // at H_baseline
        approx(m.weight_factor(1.0), -1.0); // at H_alternate → W = -1
        approx(m.weight_factor(3.0), -0.5); // midpoint
    }

    /// §6.2.1 Formula (1) inverse-normalization with γ = 1: a normalized
    /// sample of 1.0 maps to max(G), 0.0 to min(G), 0.5 to the midpoint.
    /// The fixture channel has min(G) = −0.25, max(G) = 0.75.
    #[test]
    fn unnormalize_log2_gain_inverts_normalization() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        let ch = m.channels[0];
        approx(ch.unnormalize_log2_gain(1.0), 0.75); // → max(G)
        approx(ch.unnormalize_log2_gain(0.0), -0.25); // → min(G)
        approx(ch.unnormalize_log2_gain(0.5), 0.25); // span 1.0 × 0.5 − 0.25
                                                     // Out-of-range stored sample saturates rather than producing NaN.
        approx(ch.unnormalize_log2_gain(2.0), 0.75);
        approx(ch.unnormalize_log2_gain(-1.0), -0.25);
    }

    /// §6.2.1 Formula (1) with γ ≠ 1: the gamma inverse `x^(1/γ)` is
    /// applied before the [min, max] range scaling. With γ = 2 and a
    /// normalized sample of 0.25, `0.25^(1/2) = 0.5`, so the result is
    /// the same midpoint as a γ = 1 sample of 0.5.
    #[test]
    fn unnormalize_log2_gain_applies_gamma_inverse() {
        let mut buf = build_singlechannel_gain_map();
        // channel gamma lives at offset 21 + 16 = 37 (num) / 41 (den).
        buf[37..41].copy_from_slice(&2u32.to_be_bytes()); // gamma num = 2
        buf[41..45].copy_from_slice(&1u32.to_be_bytes()); // gamma den = 1
        let m = GainMapMetadata::parse(&buf).unwrap();
        let ch = m.channels[0];
        approx(ch.unnormalize_log2_gain(0.25), 0.25);
        approx(ch.unnormalize_log2_gain(1.0), 0.75); // 1^anything = 1 → max
        approx(ch.unnormalize_log2_gain(0.0), -0.25); // 0 → min
    }

    /// §6.3 Formula (2) end to end: at the baseline headroom W = 0 so
    /// `2^(W·G) = 1` and the alternate reduces to
    /// `(Baseline + k_baseline) − k_alternate`. Fixture offsets are
    /// k_baseline = −0.125, k_alternate = 0.4375.
    #[test]
    fn apply_component_at_baseline_headroom_is_offset_only() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        // Baseline 1.0, target == H_baseline (2.5) → W = 0.
        let out = m.apply_component(1.0, 1.0, 2.5, 0).unwrap();
        approx(out, 1.0 + (-0.125) - 0.4375);
    }

    /// §6.3 Formula (2) at full application (target == H_alternate, W = 1)
    /// with a maximal gain sample (G = max(G) = 0.75): the multiplicative
    /// term is `2^0.75`.
    #[test]
    fn apply_component_at_full_application() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        // baseline 1.0, normalized 1.0 → G = 0.75, target 8.0 → W = 1.
        let out = m.apply_component(1.0, 1.0, 8.0, 0).unwrap();
        let expected = (1.0 + (-0.125)) * 0.75f64.exp2() - 0.4375;
        approx(out, expected);
    }

    /// §5.2.5.1 broadcast: a single-channel metadata record applies to
    /// all three RGB colour components, so each component uses the same
    /// channel values, and `apply_rgb` matches `apply_component` per slot.
    #[test]
    fn apply_rgb_broadcasts_single_channel_metadata() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        let baseline = [0.5, 1.0, 0.25];
        let gain = [0.0, 0.5, 1.0];
        let rgb = m.apply_rgb(baseline, gain, 8.0).unwrap();
        for c in 0..3 {
            let want = m.apply_component(baseline[c], gain[c], 8.0, c).unwrap();
            approx(rgb[c], want);
        }
    }

    /// §5.2.5.1 per-component metadata: a three-channel record uses a
    /// distinct value per colour component. The R/G/B channels carry
    /// different min(G) values, so `channel_for` must index them rather
    /// than broadcast channel 0.
    #[test]
    fn apply_component_indexes_per_component_metadata() {
        // Three-channel payload: R/G/B min(G) = 0/0.5/1 (×1 den), max = 2,
        // gamma = 1, offsets 0; base_hdr = 1, alt_hdr = 2.
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u16.to_be_bytes()); // minimum_version
        buf.extend_from_slice(&0u16.to_be_bytes()); // writer_version
        buf.push(0x80); // is_multichannel = 1
        push_rational(&mut buf, 1, 1); // base_hdr_headroom = 1.0
        push_rational(&mut buf, 2, 1); // alternate_hdr_headroom = 2.0
        let mins = [(0i32, 1u32), (1, 2), (1, 1)]; // 0.0, 0.5, 1.0
        for (mn, md) in mins {
            push_rational(&mut buf, mn, md); // gain_map_min
            push_rational(&mut buf, 2, 1); // gain_map_max = 2.0
            push_rational(&mut buf, 1, 1); // gamma = 1
            push_rational(&mut buf, 0, 1); // base_offset = 0
            push_rational(&mut buf, 0, 1); // alternate_offset = 0
        }
        let m = GainMapMetadata::parse(&buf).unwrap();
        assert_eq!(m.channel_count(), 3);
        // normalized = 0 selects each component's min(G); with W = 1 at
        // target 2.0 the result is baseline · 2^min(G).
        let baseline = 1.0;
        approx(
            m.apply_component(baseline, 0.0, 2.0, 0).unwrap(),
            baseline * 0.0f64.exp2(),
        );
        approx(
            m.apply_component(baseline, 0.0, 2.0, 1).unwrap(),
            baseline * 0.5f64.exp2(),
        );
        approx(
            m.apply_component(baseline, 0.0, 2.0, 2).unwrap(),
            baseline * 1.0f64.exp2(),
        );
    }

    /// `apply_component` rejects an out-of-range colour-component index.
    #[test]
    fn apply_component_rejects_component_index_above_two() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        assert!(m.apply_component(1.0, 1.0, 8.0, 3).is_none());
    }

    /// §6.2 NOTE 3 / §6.3 round trip: a gain map computed from a known
    /// baseline/alternate pair (Annex A.2 Formula (A.1)) reconstructs the
    /// alternate when applied with W = 1. With zero offsets and γ = 1, the
    /// log2-ratio stored as the gain and re-applied returns the alternate.
    #[test]
    fn apply_component_round_trips_a2_gain() {
        // Construct metadata whose single channel spans the exact gain
        // G = log2(alternate / baseline) for baseline = 2, alternate = 8
        // → G = log2(4) = 2.0. Store min = max = 2.0 so any normalized
        // sample unnormalizes to 2.0; offsets 0, gamma 1.
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.push(0x00); // single channel
        push_rational(&mut buf, 1, 1); // base_hdr = 1.0
        push_rational(&mut buf, 2, 1); // alt_hdr  = 2.0 (W = 1 at target 2)
        push_rational(&mut buf, 2, 1); // gain_map_min = 2.0
        push_rational(&mut buf, 2, 1); // gain_map_max = 2.0
        push_rational(&mut buf, 1, 1); // gamma
        push_rational(&mut buf, 0, 1); // base_offset
        push_rational(&mut buf, 0, 1); // alternate_offset
        let m = GainMapMetadata::parse(&buf).unwrap();
        // Alternate = (2 + 0) · 2^(1 · 2) − 0 = 2 · 4 = 8.
        approx(m.apply_component(2.0, 0.5, 2.0, 0).unwrap(), 8.0);
    }

    /// `apply_plane_rgb` over an achromatic gain plane (one sample per
    /// pixel) matches per-pixel `apply_rgb` with the sample broadcast to
    /// all three colour components (§6.3 NOTE 2).
    #[test]
    fn apply_plane_rgb_achromatic_matches_per_pixel() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        // 2×1 image, interleaved RGB baseline.
        let baseline = [0.25, 0.5, 0.75, 1.0, 0.125, 0.6];
        let gain = [0.3, 0.9]; // one sample per pixel
        let out = m.apply_plane_rgb(&baseline, &gain, 2, 1, 6.0).unwrap();
        assert_eq!(out.len(), 6);
        for p in 0..2 {
            let base = [baseline[p * 3], baseline[p * 3 + 1], baseline[p * 3 + 2]];
            let want = m.apply_rgb(base, [gain[p]; 3], 6.0).unwrap();
            for c in 0..3 {
                approx(out[p * 3 + c], want[c]);
            }
        }
    }

    /// `apply_plane_rgb` over an interleaved RGB gain plane consumes one
    /// gain sample per colour component.
    #[test]
    fn apply_plane_rgb_interleaved_uses_per_component_gain() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        let baseline = [0.25, 0.5, 0.75]; // 1×1
        let gain = [0.0, 0.5, 1.0]; // RGB gain plane, 1×1×3
        let out = m.apply_plane_rgb(&baseline, &gain, 1, 1, 8.0).unwrap();
        for c in 0..3 {
            let want = m.apply_component(baseline[c], gain[c], 8.0, c).unwrap();
            approx(out[c], want);
        }
    }

    /// `apply_plane_rgb` rejects a baseline length that does not match
    /// `width × height × 3` and a gain length that is neither the
    /// achromatic nor the RGB size.
    #[test]
    fn apply_plane_rgb_rejects_mismatched_lengths() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        // baseline too short for 2×1×3 = 6.
        let err = m
            .apply_plane_rgb(&[0.0, 0.0, 0.0], &[0.0, 0.0], 2, 1, 6.0)
            .unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
        // gain length 5 is neither 2 (achromatic) nor 6 (RGB).
        let baseline = [0.0; 6];
        let err = m
            .apply_plane_rgb(&baseline, &[0.0; 5], 2, 1, 6.0)
            .unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    /// `apply_plane_rgb` on a zero-area plane is a valid empty result.
    #[test]
    fn apply_plane_rgb_zero_area_is_empty() {
        let m = GainMapMetadata::parse(&build_singlechannel_gain_map()).unwrap();
        let out = m.apply_plane_rgb(&[], &[], 0, 4, 8.0).unwrap();
        assert!(out.is_empty());
    }

    // -------------------------------------------------------------------
    // AVIF Profile compliance audit (av1-avif v1.2.0 §8.2 / §8.3)
    // -------------------------------------------------------------------

    /// Build an `av1C` payload whose byte 1 packs
    /// `seq_profile (3) | seq_level_idx_0 (5)` per av1-isobmff §2.3.
    /// Other bytes are filled with conventional values (marker=1,
    /// version=1, chroma 4:2:0 mono-cleared).
    fn av1c_with_profile_level(seq_profile: u8, seq_level_idx_0: u8) -> Vec<u8> {
        assert!(seq_profile <= 7);
        assert!(seq_level_idx_0 <= 31);
        let b1 = (seq_profile << 5) | (seq_level_idx_0 & 0x1F);
        vec![0x81, b1, 0x00, 0x00]
    }

    fn brands_with(baseline: bool, advanced: bool) -> crate::parser::BrandClass {
        crate::parser::BrandClass {
            is_image: true,
            is_miaf: true,
            is_baseline_profile: baseline,
            is_advanced_profile: advanced,
            ..crate::parser::BrandClass::default()
        }
    }

    /// Decoded `seq_profile` / `seq_level_idx_0` round-trip through
    /// the byte-1 helpers across the boundary values that the spec's
    /// §8.2 / §8.3 audit branches on.
    #[test]
    fn decode_av1c_byte1_round_trips_profile_and_level() {
        // (seq_profile, seq_level_idx_0) coverage: 0/13 (Baseline edge),
        // 0/14 (over Baseline level), 1/16 (Advanced edge), 1/17 (over
        // Advanced level), 2/0 (Professional disallowed), 7/31 (max bits).
        let cases = [(0, 13), (0, 14), (1, 16), (1, 17), (2, 0), (7, 31)];
        for (p, l) in cases {
            let bytes = av1c_with_profile_level(p, l);
            assert_eq!(decode_av1c_seq_profile(&bytes), Some(p), "p={p} l={l}");
            assert_eq!(decode_av1c_seq_level_idx_0(&bytes), Some(l), "p={p} l={l}");
        }
    }

    /// Both helpers tolerate truncation without panicking.
    #[test]
    fn decode_av1c_byte1_handles_truncation() {
        assert_eq!(decode_av1c_seq_profile(&[]), None);
        assert_eq!(decode_av1c_seq_profile(&[0x81]), None);
        assert_eq!(decode_av1c_seq_level_idx_0(&[]), None);
        assert_eq!(decode_av1c_seq_level_idx_0(&[0x81]), None);
    }

    /// MA1B + Main Profile + level 5.1 (`seq_profile=0, idx=13`) is the
    /// spec-canonical Baseline shape. Compliant.
    #[test]
    fn audit_profile_baseline_main_level_5_1_compliant() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(av1c_with_profile_level(0, 13))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].profile, AvifProfile::Baseline);
        assert_eq!(r[0].seq_profile, Some(0));
        assert_eq!(r[0].seq_level_idx_0, Some(13));
        assert!(r[0].is_compliant());
        assert!(r[0].missing().is_empty());
    }

    /// MA1B + Main + level 5.2 (`idx=14`) tips the level over the
    /// §8.2 bound. Flagged with `seq-level-idx-out-of-range`.
    #[test]
    fn audit_profile_baseline_level_above_5_1_flagged() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(av1c_with_profile_level(0, 14))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert!(!r[0].is_compliant());
        assert_eq!(r[0].missing(), vec!["seq-level-idx-out-of-range"]);
    }

    /// MA1B + High Profile (`seq_profile=1`) violates §8.2's "Main
    /// Profile" requirement. Flagged with `seq-profile-out-of-range`.
    #[test]
    fn audit_profile_baseline_high_profile_flagged() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(av1c_with_profile_level(1, 13))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert!(!r[0].is_compliant());
        assert_eq!(r[0].missing(), vec!["seq-profile-out-of-range"]);
    }

    /// MA1A + High Profile + level 6.0 (`idx=16`) is the spec-canonical
    /// Advanced shape. Compliant.
    #[test]
    fn audit_profile_advanced_high_level_6_0_compliant() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(av1c_with_profile_level(1, 16))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(false, true));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].profile, AvifProfile::Advanced);
        assert!(r[0].is_compliant());
    }

    /// MA1A + Main Profile (`seq_profile=0`) is also compliant — Main
    /// is a subset of High per AV1 §A.2.
    #[test]
    fn audit_profile_advanced_main_profile_compliant() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(av1c_with_profile_level(0, 13))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(false, true));
        assert!(r[0].is_compliant());
    }

    /// MA1A + Professional (`seq_profile=2`) breaches §8.3's "≤ High"
    /// requirement. Flagged.
    #[test]
    fn audit_profile_advanced_professional_flagged() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(av1c_with_profile_level(2, 16))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(false, true));
        assert!(!r[0].is_compliant());
        assert_eq!(r[0].missing(), vec!["seq-profile-out-of-range"]);
    }

    /// MA1A + level 6.1 (`idx=17`) breaches §8.3's "≤ 6.0".
    #[test]
    fn audit_profile_advanced_level_above_6_0_flagged() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(av1c_with_profile_level(1, 17))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(false, true));
        assert!(!r[0].is_compliant());
        assert_eq!(r[0].missing(), vec!["seq-level-idx-out-of-range"]);
    }

    /// AV1 §A.3's `seq_level_idx_0 == 31` "Maximum parameters" carve-out
    /// signals unconstrained sizing, which is outside either profile's
    /// reach — even though Main Profile (`seq_profile=0`) is compliant
    /// with §8.2's profile clause, the level clause is unmet.
    #[test]
    fn audit_profile_level_31_max_parameters_flagged_for_baseline() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(av1c_with_profile_level(0, 31))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(true, false));
        assert!(!r[0].is_compliant());
        assert_eq!(r[0].missing(), vec!["seq-level-idx-out-of-range"]);
    }

    /// Missing `av1C` surfaces the missing-av1c flag distinctly from
    /// a truncated `av1C`.
    #[test]
    fn audit_profile_missing_av1c_flagged() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![],
            associations: vec![],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert!(r[0].missing_av1c);
        assert_eq!(r[0].seq_profile, None);
        assert_eq!(r[0].seq_level_idx_0, None);
        assert!(!r[0].is_compliant());
        assert_eq!(r[0].missing(), vec!["item-missing-av1C"]);
    }

    /// Truncated `av1C` (`< 2` bytes) surfaces as item-av1C-truncated,
    /// distinct from missing-av1c.
    #[test]
    fn audit_profile_truncated_av1c_flagged() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(vec![0x81])], // only 1 byte
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(true, false));
        assert_eq!(r.len(), 1);
        assert!(!r[0].missing_av1c);
        assert_eq!(r[0].seq_profile, None);
        assert_eq!(r[0].seq_level_idx_0, None);
        assert!(!r[0].is_compliant());
        assert_eq!(r[0].missing(), vec!["item-av1C-truncated"]);
    }

    /// File declaring both `MA1B` and `MA1A` emits one record per
    /// brand, Baseline before Advanced.
    #[test]
    fn audit_profile_both_brands_emit_two_records_per_item() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            // Main profile + level 5.1 — compliant with both brands.
            properties: vec![Property::Av1C(av1c_with_profile_level(0, 13))],
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(true, true));
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].profile, AvifProfile::Baseline);
        assert_eq!(r[1].profile, AvifProfile::Advanced);
        assert!(r[0].is_compliant());
        assert!(r[1].is_compliant());
    }

    /// File declaring neither `MA1B` nor `MA1A` skips the audit
    /// entirely. The §8 constraints only apply to files claiming the
    /// brand; an unbranded AVIF has nothing to fail.
    #[test]
    fn audit_profile_no_brand_claim_returns_empty() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0)],
            properties: vec![Property::Av1C(av1c_with_profile_level(2, 31))], // would fail both
            associations: vec![ipa(1, &[0])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(false, false));
        assert!(r.is_empty());
    }

    /// File with no AV1 Image Items returns an empty audit even when
    /// the brand is declared (degenerate case — `meta.item_ids_of_type`
    /// returns nothing).
    #[test]
    fn audit_profile_no_av01_items_returns_empty() {
        let meta = Meta {
            items: vec![make_infe(1, b"mime", 0)],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(true, false));
        assert!(r.is_empty());
    }

    /// Two AV1 Image Items in a file declaring both brands → 4
    /// records, paired (Baseline, Advanced) per item in iinf order.
    #[test]
    fn audit_profile_two_items_two_brands_four_records() {
        let meta = Meta {
            items: vec![make_infe(1, b"av01", 0), make_infe(2, b"av01", 0)],
            properties: vec![
                Property::Av1C(av1c_with_profile_level(0, 13)), // item 1: compliant both
                Property::Av1C(av1c_with_profile_level(2, 16)), // item 2: Advanced-fails profile
            ],
            associations: vec![ipa(1, &[0]), ipa(2, &[1])],
            ..Meta::default()
        };
        let r = audit_avif_profile_compliance(&meta, &brands_with(true, true));
        assert_eq!(r.len(), 4);
        // Item 1 first (iinf order), then item 2.
        assert_eq!(r[0].item_id, 1);
        assert_eq!(r[0].profile, AvifProfile::Baseline);
        assert!(r[0].is_compliant());
        assert_eq!(r[1].item_id, 1);
        assert_eq!(r[1].profile, AvifProfile::Advanced);
        assert!(r[1].is_compliant());
        assert_eq!(r[2].item_id, 2);
        assert_eq!(r[2].profile, AvifProfile::Baseline);
        assert!(!r[2].is_compliant()); // seq_profile=2 fails Baseline
        assert_eq!(r[3].item_id, 2);
        assert_eq!(r[3].profile, AvifProfile::Advanced);
        assert!(!r[3].is_compliant()); // seq_profile=2 fails Advanced too
    }

    // -----------------------------------------------------------------------
    // Derived-image geometry resolution (HEIF §6.3 / §6.6.2)
    // -----------------------------------------------------------------------

    use crate::meta::{
        Clap as MClap, Imir as MImir, Irot as MIrot, Iscl as MIscl, ItemInfo as MII,
        ItemPropertyAssociation as MIpa, Meta as MMeta, Property as MProp,
        PropertyAssociation as MPa,
    };

    fn geo_ii(id: u32, item_type: &[u8; 4]) -> MII {
        MII {
            id,
            item_type: *item_type,
            name: String::new(),
            content_type: None,
            content_encoding: None,
            item_uri_type: None,
            flags: 0,
        }
    }

    fn geo_assoc(item_id: u32, indices: &[u16]) -> MIpa {
        MIpa {
            item_id,
            entries: indices
                .iter()
                .map(|&index| MPa {
                    index,
                    essential: true,
                })
                .collect(),
        }
    }

    fn ispe_prop(w: u32, h: u32) -> MProp {
        MProp::Ispe(crate::meta::Ispe {
            width: w,
            height: h,
        })
    }

    /// `DimTransform::apply_dims` — rotation by 90°/270° swaps W/H; 0°/180°
    /// preserve.
    #[test]
    fn dim_transform_rotate_swaps_at_odd_quarter_turns() {
        assert_eq!(
            DimTransform::Rotate { angle: 0 }.apply_dims(40, 30),
            (40, 30)
        );
        assert_eq!(
            DimTransform::Rotate { angle: 1 }.apply_dims(40, 30),
            (30, 40)
        );
        assert_eq!(
            DimTransform::Rotate { angle: 2 }.apply_dims(40, 30),
            (40, 30)
        );
        assert_eq!(
            DimTransform::Rotate { angle: 3 }.apply_dims(40, 30),
            (30, 40)
        );
    }

    /// Mirror never changes dimensions (§6.5.12).
    #[test]
    fn dim_transform_mirror_preserves_dims() {
        assert_eq!(
            DimTransform::Mirror { axis: 0 }.apply_dims(40, 30),
            (40, 30)
        );
        assert_eq!(
            DimTransform::Mirror { axis: 1 }.apply_dims(40, 30),
            (40, 30)
        );
    }

    /// Crop reduces dimensions to the clean-aperture width/height; a crop
    /// larger than the input is a no-op (defensive, matches `apply_clap`).
    #[test]
    fn dim_transform_crop_to_clean_aperture() {
        let c = DimTransform::Crop {
            width_n: 20,
            width_d: 1,
            height_n: 10,
            height_d: 1,
        };
        assert_eq!(c.apply_dims(40, 30), (20, 10));
        // Crop wider than input → unchanged.
        let big = DimTransform::Crop {
            width_n: 100,
            width_d: 1,
            height_n: 10,
            height_d: 1,
        };
        assert_eq!(big.apply_dims(40, 30), (40, 30));
        // Zero denominator → unchanged.
        let zero = DimTransform::Crop {
            width_n: 20,
            width_d: 0,
            height_n: 10,
            height_d: 1,
        };
        assert_eq!(zero.apply_dims(40, 30), (40, 30));
    }

    /// Scale applies `ceil(input * num / den)` per §6.5.13.
    #[test]
    fn dim_transform_scale_ceil() {
        let s = DimTransform::Scale {
            target_width_numerator: 3,
            target_width_denominator: 2,
            target_height_numerator: 1,
            target_height_denominator: 2,
        };
        // 40 * 3 / 2 = 60; 30 * 1 / 2 = 15.
        assert_eq!(s.apply_dims(40, 30), (60, 15));
        // Ceiling: 41 * 1 / 2 = 20.5 → 21.
        assert_eq!(
            DimTransform::Scale {
                target_width_numerator: 1,
                target_width_denominator: 2,
                target_height_numerator: 1,
                target_height_denominator: 1,
            }
            .apply_dims(41, 9),
            (21, 9)
        );
    }

    /// `transform_chain` collects only the dimension-affecting transformative
    /// properties, in `ipma` order, skipping descriptive properties.
    #[test]
    fn transform_chain_preserves_ipma_order_skips_descriptive() {
        let meta = MMeta {
            items: vec![geo_ii(1, b"av01")],
            properties: vec![
                ispe_prop(100, 50),              // 0: descriptive, skipped
                MProp::Irot(MIrot { angle: 1 }), // 1: rotate
                MProp::Clap(MClap {
                    // 2: crop
                    clean_aperture_width_n: 40,
                    clean_aperture_width_d: 1,
                    clean_aperture_height_n: 30,
                    clean_aperture_height_d: 1,
                    horiz_off_n: 0,
                    horiz_off_d: 1,
                    vert_off_n: 0,
                    vert_off_d: 1,
                }),
                MProp::Imir(MImir { axis: 1 }), // 3: mirror
            ],
            // ipma order: ispe, irot, clap, imir.
            associations: vec![geo_assoc(1, &[0, 1, 2, 3])],
            ..MMeta::default()
        };
        let chain = transform_chain(&meta, 1);
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0], DimTransform::Rotate { angle: 1 });
        assert!(matches!(chain[1], DimTransform::Crop { .. }));
        assert_eq!(chain[2], DimTransform::Mirror { axis: 1 });
    }

    /// `output_dims_from_reconstructed` folds the chain in order: a coded
    /// 100×50 image rotated 90° → 50×100, then clap-cropped to 40×30.
    #[test]
    fn output_dims_folds_chain_in_order() {
        let meta = MMeta {
            items: vec![geo_ii(1, b"av01")],
            properties: vec![
                MProp::Irot(MIrot { angle: 1 }),
                MProp::Clap(MClap {
                    clean_aperture_width_n: 40,
                    clean_aperture_width_d: 1,
                    clean_aperture_height_n: 30,
                    clean_aperture_height_d: 1,
                    horiz_off_n: 0,
                    horiz_off_d: 1,
                    vert_off_n: 0,
                    vert_off_d: 1,
                }),
            ],
            associations: vec![geo_assoc(1, &[0, 1])],
            ..MMeta::default()
        };
        // Reconstructed 100×50, rotate → 50×100, crop → 40×30.
        assert_eq!(output_dims_from_reconstructed(&meta, 1, 100, 50), (40, 30));
        // Empty chain → identity.
        let bare = MMeta {
            items: vec![geo_ii(9, b"av01")],
            ..MMeta::default()
        };
        assert_eq!(output_dims_from_reconstructed(&bare, 9, 100, 50), (100, 50));
    }

    /// `reconstructed_dims` for a coded item reads `ispe`.
    #[test]
    fn reconstructed_dims_coded_uses_ispe() {
        let meta = MMeta {
            items: vec![geo_ii(1, b"av01")],
            properties: vec![ispe_prop(640, 480)],
            associations: vec![geo_assoc(1, &[0])],
            ..MMeta::default()
        };
        assert_eq!(reconstructed_dims(&meta, 1, &[], None), Some((640, 480)));
        // No ispe → None.
        let bare = MMeta {
            items: vec![geo_ii(2, b"av01")],
            ..MMeta::default()
        };
        assert_eq!(reconstructed_dims(&bare, 2, &[], None), None);
    }

    /// Build a 16-bit `iovl` descriptor with the given canvas + offsets.
    fn iovl_desc(out_w: u16, out_h: u16, offsets: &[(i16, i16)]) -> Vec<u8> {
        let mut buf = vec![0u8, 0u8]; // version 0, flags 0 (16-bit fields)
        for _ in 0..4 {
            buf.extend_from_slice(&0u16.to_be_bytes()); // canvas fill
        }
        buf.extend_from_slice(&out_w.to_be_bytes());
        buf.extend_from_slice(&out_h.to_be_bytes());
        for &(x, y) in offsets {
            buf.extend_from_slice(&x.to_be_bytes());
            buf.extend_from_slice(&y.to_be_bytes());
        }
        buf
    }

    /// End-to-end `iovl` resolution against an `idat`-backed descriptor:
    /// two inputs placed on a 200×200 canvas, the second partially clipped
    /// at the right/bottom edge.
    #[test]
    fn resolve_overlays_idat_with_clipping() {
        // idat holds the iovl descriptor at offset 0.
        let desc = iovl_desc(200, 200, &[(10, 20), (160, 170)]);
        let idat = desc.clone();
        let meta = MMeta {
            items: vec![geo_ii(1, b"iovl"), geo_ii(2, b"av01"), geo_ii(3, b"av01")],
            properties: vec![ispe_prop(80, 60), ispe_prop(80, 60)],
            associations: vec![geo_assoc(2, &[0]), geo_assoc(3, &[1])],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 1,
                to_ids: vec![2, 3],
            }],
            locations: vec![ItemLocation {
                id: 1,
                construction_method: 1, // idat
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![IlocExtent {
                    offset: 0,
                    length: desc.len() as u64,
                    extent_index: 0,
                }],
            }],
            ..MMeta::default()
        };
        let res = resolve_overlays(&meta, &[], Some(&idat));
        assert_eq!(res.len(), 1);
        let r = &res[0];
        assert_eq!(r.iovl_item_id, 1);
        assert_eq!(r.canvas(), (200, 200));
        assert_eq!(r.placements.len(), 2);

        // First input: fully visible at (10, 20), 80×60.
        let p0 = &r.placements[0];
        assert_eq!(p0.source_item_id, 2);
        assert_eq!((p0.input_width, p0.input_height), (80, 60));
        assert!(p0.fully_visible(200, 200));
        assert_eq!(p0.visible(200, 200), Some((10, 20, 80, 60)));

        // Second input: at (160, 170), 80×60 → clipped to (160,170,40,30).
        let p1 = &r.placements[1];
        assert_eq!(p1.source_item_id, 3);
        assert!(!p1.fully_visible(200, 200));
        assert_eq!(p1.visible(200, 200), Some((160, 170, 40, 30)));
        // Not fully covered → fill colour shows through.
        assert!(r.canvas_partially_filled());
    }

    /// An overlay input placed entirely off-canvas (negative beyond its own
    /// extent) is reported as off-canvas with no visible rectangle.
    #[test]
    fn overlay_placement_off_canvas() {
        let p = OverlayPlacement {
            source_item_id: 5,
            offset_x: -100,
            offset_y: 0,
            input_width: 50,
            input_height: 50,
        };
        assert!(p.off_canvas(200, 200));
        assert_eq!(p.visible(200, 200), None);
        // A negative offset that still overlaps → partial visible rect.
        let q = OverlayPlacement { offset_x: -10, ..p };
        assert_eq!(q.visible(200, 200), Some((0, 0, 40, 50)));
    }

    /// End-to-end `iden` resolution: an identity derivation that crops its
    /// 100×80 source via a `clap` carried on the iden item itself
    /// (§6.6.2.1 NOTE 2).
    #[test]
    fn resolve_iden_applies_iden_transforms() {
        let meta = MMeta {
            items: vec![geo_ii(1, b"iden"), geo_ii(2, b"av01")],
            properties: vec![
                ispe_prop(100, 80), // 0: source ispe
                MProp::Clap(MClap {
                    // 1: clap on the iden item
                    clean_aperture_width_n: 50,
                    clean_aperture_width_d: 1,
                    clean_aperture_height_n: 40,
                    clean_aperture_height_d: 1,
                    horiz_off_n: 0,
                    horiz_off_d: 1,
                    vert_off_n: 0,
                    vert_off_d: 1,
                }),
            ],
            associations: vec![geo_assoc(2, &[0]), geo_assoc(1, &[1])],
            irefs: vec![IrefEntry {
                reference_type: *b"dimg",
                from_id: 1,
                to_ids: vec![2],
            }],
            ..MMeta::default()
        };
        let res = resolve_iden_derivations(&meta, &[], None);
        assert_eq!(res.len(), 1);
        let r = &res[0];
        assert_eq!(r.iden_item_id, 1);
        assert_eq!(r.source_item_id, Some(2));
        assert_eq!(r.source_dims, Some((100, 80)));
        assert_eq!(r.transforms.len(), 1);
        assert_eq!(r.output_dims, Some((50, 40)));
    }

    /// An `iden` whose source is itself a grid resolves transitively: the
    /// grid's `output_width/height` become the iden's source dims, then the
    /// iden's `irot` swaps them.
    #[test]
    fn reconstructed_dims_iden_over_grid_with_rotation() {
        // grid descriptor: version 0, flags 0, rows=1, cols=1, 256×128.
        let mut grid = vec![0u8, 0u8, 0u8, 0u8];
        grid.extend_from_slice(&256u16.to_be_bytes());
        grid.extend_from_slice(&128u16.to_be_bytes());
        let idat = grid.clone();
        let meta = MMeta {
            items: vec![geo_ii(1, b"iden"), geo_ii(2, b"grid"), geo_ii(3, b"av01")],
            properties: vec![
                ispe_prop(256, 128),             // 0: tile ispe (unused for grid dims)
                MProp::Irot(MIrot { angle: 1 }), // 1: rotate on the iden
            ],
            associations: vec![geo_assoc(3, &[0]), geo_assoc(1, &[1])],
            irefs: vec![
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 1,
                    to_ids: vec![2],
                },
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 2,
                    to_ids: vec![3],
                },
            ],
            locations: vec![ItemLocation {
                id: 2,
                construction_method: 1,
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![IlocExtent {
                    offset: 0,
                    length: grid.len() as u64,
                    extent_index: 0,
                }],
            }],
            ..MMeta::default()
        };
        // iden reconstructed dims = output image of its grid source = 256×128
        // (grid has no transforms), then the iden's irot swaps → 128×256.
        let res = resolve_iden_derivations(&meta, &[], Some(&idat));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].source_dims, Some((256, 128)));
        assert_eq!(res[0].output_dims, Some((128, 256)));
    }

    /// A `dimg` cycle (iden → iden → …) is broken by the depth guard rather
    /// than recursing forever.
    #[test]
    fn reconstructed_dims_cycle_guard() {
        let meta = MMeta {
            items: vec![geo_ii(1, b"iden"), geo_ii(2, b"iden")],
            irefs: vec![
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 1,
                    to_ids: vec![2],
                },
                IrefEntry {
                    reference_type: *b"dimg",
                    from_id: 2,
                    to_ids: vec![1],
                },
            ],
            ..MMeta::default()
        };
        // Must terminate and return None rather than overflow the stack.
        assert_eq!(reconstructed_dims(&meta, 1, &[], None), None);
    }

    /// `iscl` participates in the chain — verify a coded item scaled 2×.
    #[test]
    fn output_dims_with_iscl() {
        let meta = MMeta {
            items: vec![geo_ii(1, b"av01")],
            properties: vec![MProp::Iscl(MIscl {
                target_width_numerator: 2,
                target_width_denominator: 1,
                target_height_numerator: 2,
                target_height_denominator: 1,
            })],
            associations: vec![geo_assoc(1, &[0])],
            ..MMeta::default()
        };
        assert_eq!(output_dims_from_reconstructed(&meta, 1, 64, 48), (128, 96));
    }

    // -----------------------------------------------------------------------
    // Property / fuzz tests for the box-parsing surface
    // -----------------------------------------------------------------------
    //
    // These hammer the descriptor parsers and the geometry resolver with
    // pseudo-random and adversarially-shaped byte inputs. The contract under
    // test is **total**: a parser must return `Ok(_)` or `Err(_)` and never
    // panic (no slice-index OOB, no integer overflow in debug, no
    // unbounded recursion) for *any* input. The PRNG is a deterministic
    // splitmix64 so a failure reproduces from the printed seed.

    /// Deterministic splitmix64 — no external crate, reproducible from seed.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// A pseudo-random byte vector of length `0..=max_len`.
        fn bytes(&mut self, max_len: usize) -> Vec<u8> {
            let len = (self.next() as usize) % (max_len + 1);
            (0..len).map(|_| (self.next() & 0xff) as u8).collect()
        }
    }

    /// `ImageOverlay::parse` is total over arbitrary bytes × arbitrary
    /// reference counts — it never panics, only returns `Ok`/`Err`.
    #[test]
    fn fuzz_image_overlay_parse_is_total() {
        let mut rng = SplitMix64(0x0AF1_2026_0341_0001);
        for _ in 0..20_000 {
            let buf = rng.bytes(48);
            let refs = (rng.next() as usize) % 8;
            // Must not panic regardless of input.
            let _ = ImageOverlay::parse(&buf, refs);
        }
    }

    /// `GainMapMetadata::parse` (ISO 21496-1 Annex C.2) is total — random
    /// payloads must not panic on the flag byte, version bounds, or the
    /// per-channel rational reads.
    #[test]
    fn fuzz_gain_map_metadata_parse_is_total() {
        let mut rng = SplitMix64(0x0AF1_2026_0341_0002);
        for _ in 0..20_000 {
            let buf = rng.bytes(160);
            let _ = GainMapMetadata::parse(&buf);
        }
    }

    /// `SampleTransform::parse` (av1-avif §4.2.3) is total over random
    /// descriptor bytes × reference counts — the postfix token stream
    /// validation must reject malformed input without panicking.
    #[test]
    fn fuzz_sample_transform_parse_is_total() {
        let mut rng = SplitMix64(0x0AF1_2026_0341_0003);
        for _ in 0..20_000 {
            let buf = rng.bytes(64);
            let refs = (rng.next() % 8) as u32;
            let _ = SampleTransform::parse(&buf, refs);
        }
    }

    /// `parse_grpl` (HEIF §9.4 EntityToGroupBox container) is total over
    /// random `grpl` payloads.
    #[test]
    fn fuzz_parse_grpl_is_total() {
        let mut rng = SplitMix64(0x0AF1_2026_0341_0004);
        for _ in 0..20_000 {
            let buf = rng.bytes(80);
            let _ = parse_grpl(&buf);
        }
    }

    /// The geometry resolver is total over a randomly-shaped item graph:
    /// `resolve_overlays`, `resolve_iden_derivations`, and
    /// `reconstructed_dims` must terminate (cycle guard) and never panic
    /// for any meta layout × backing buffer.
    #[test]
    fn fuzz_geometry_resolver_is_total() {
        let mut rng = SplitMix64(0x0AF1_2026_0341_0005);
        let types: [&[u8; 4]; 5] = [b"av01", b"grid", b"iovl", b"iden", b"sato"];
        for _ in 0..5_000 {
            let item_count = 1 + (rng.next() as usize % 6);
            let mut items = Vec::new();
            let mut properties = Vec::new();
            let mut associations = Vec::new();
            let mut irefs = Vec::new();
            let mut locations = Vec::new();
            for i in 0..item_count {
                let id = (i + 1) as u32;
                let t = types[(rng.next() as usize) % types.len()];
                items.push(geo_ii(id, t));
                // Random ispe property + association.
                let w = (rng.next() % 600) as u32;
                let h = (rng.next() % 600) as u32;
                properties.push(ispe_prop(w, h));
                associations.push(geo_assoc(id, &[(properties.len() - 1) as u16]));
                // A random dimg edge to another (possibly self → cycle).
                if rng.next() & 1 == 0 {
                    let to = 1 + (rng.next() as u32 % item_count as u32);
                    irefs.push(IrefEntry {
                        reference_type: *b"dimg",
                        from_id: id,
                        to_ids: vec![to],
                    });
                }
                // A random idat-backed location.
                if rng.next() & 1 == 0 {
                    locations.push(ItemLocation {
                        id,
                        construction_method: (rng.next() % 3) as u8,
                        data_reference_index: 0,
                        base_offset: rng.next() % 64,
                        extents: vec![IlocExtent {
                            offset: rng.next() % 64,
                            length: rng.next() % 64,
                            extent_index: 0,
                        }],
                    });
                }
            }
            let meta = MMeta {
                items,
                properties,
                associations,
                irefs,
                locations,
                ..MMeta::default()
            };
            let file = rng.bytes(128);
            let idat = rng.bytes(128);
            // None of these may panic or fail to terminate.
            let _ = resolve_overlays(&meta, &file, Some(&idat));
            let _ = resolve_iden_derivations(&meta, &file, Some(&idat));
            for id in 1..=item_count as u32 {
                let _ = reconstructed_dims(&meta, id, &file, Some(&idat));
            }
        }
    }
}
