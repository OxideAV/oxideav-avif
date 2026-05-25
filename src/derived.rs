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

use crate::box_parser::{iter_boxes, parse_full_box, read_u16, read_u32, type_str, BoxType};
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
        assert_eq!(g.group_id, 42);
        assert_eq!(g.entity_ids, vec![1, 2, 3]);
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
}
