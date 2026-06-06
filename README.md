# oxideav-avif

Pure-Rust **AVIF** (AV1 Image File Format) decoder. Walks the HEIF /
ISOBMFF box hierarchy, resolves the primary item via `pitm` + `iloc`,
surfaces the `av1C` configuration record + `ispe` / `colr` / `pixi` /
`pasp` item properties, then hands the AV1 OBU bitstream to
[`oxideav-av1`](https://crates.io/crates/oxideav-av1) and composites
the result (grid tiles, alpha auxiliary, `irot` / `imir` / `clap`
post-transforms) into the frame returned to the caller. Zero C
dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Status

End-to-end AVIF decode works: `AvifDecoder::send_packet` +
`receive_frame` yield a `VideoFrame` whose dimensions match the
primary item's `ispe`. Pixel fidelity tracks the current state of
[`oxideav-av1`](https://crates.io/crates/oxideav-av1) — on simple
flat / synthetic content the decoded samples are tight against the
source; on rich content (natural photos) the intra-prediction path
still loses significant signal.

| Stage                                  | Coverage                                                                                                                                                   |
|----------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `ftyp` brand check                     | accepts `avif` / `avis` / `mif1` / `msf1` / `miaf`                                                                                                         |
| `meta` sub-boxes                       | `hdlr`, `pitm` (v0/v1), `iinf` (v0/v1) + `infe` (v2/v3), `iloc` (v0/v1/v2), `iref`, `iprp` / `ipco` / `ipma` (v0/v1, small + large property indices)       |
| Item properties                        | `av1C`, `ispe`, `colr` (nclx + ICC), `pixi`, `pasp`, `irot`, `imir`, `clap`, `auxC`, `mdcv`, `clli`, `cclv`, `rloc`, `lsel`, `a1op`, `a1lx`, `iscl` (HEIF §6.5.13 image scaling — four u16 ratio fields + `Iscl::is_well_formed` + `Iscl::scaled_dims`), `rref` (HEIF §6.5.17 required reference types — typed `Vec<BoxType>` + `Rref::{count, requires}`), `crtt` (HEIF §6.5.18 creation time — u64 microseconds since 1904-01-01 UTC + `Crtt::{seconds_since_unix_epoch, subsecond_micros}`), `mdft` (HEIF §6.5.19 modification time — same 1904-epoch microsecond unit as `crtt`; `Mdft::{seconds_since_unix_epoch, subsecond_micros}` mirror the `crtt` helpers, and `mdft` may legally co-occur with `crtt` on a single item to surface a creation/modification pair), `udes` (HEIF §6.5.20 user description — four UTF-8 strings `lang` / `name` / `description` / `tags` with the §6.5.20.3 empty-string "absent" sentinels projected via `Udes::{lang_opt, name_opt, description_opt, tags_opt}` and a `Udes::tag_list` view that splits on `','` and trims each segment; §6.5.20.1 quantity is zero-or-more so multiple language variants legally co-occur on a single item); unknown boxes retained as `Property::Other` so indices stay valid |
| Sample Transform (`sato`)              | descriptor parser + per-sample evaluator for av1-avif §4.2.3 — full operator table (negation/abs/not/bsr unary + sum/difference/product/quotient/and/or/xor/pow/min/max binary), all 4 bit-depth widths (8/16/32/64-bit intermediate), every spec assertion enforced (`token_count >= 1`, sample index ≤ `reference_count`, postfix order, stack discipline, single-element terminal stack, reserved-token rejection); composition into a reconstructed image deferred until oxideav-av1 ships a decoder |
| Tone Map (`tmap`)                      | item-type four-CC detection + `AvifInfo::tmap_item_ids` enumeration + av1-avif §4.2.2 `should`-level compliance audit (`audit_tone_map` / `ToneMapCompliance`): `altr` group pairs the tmap with its base item; gain-map inputs (`dimg to_ids[1..]`) flagged hidden via `infe` flags low bit; aggregate via `AvifInfo::tone_map_compliance` / `tone_map_strict_compliant()`. **`tmap` descriptor body parse** lands via `GainMapMetadata::parse` (ISO 21496-1:2025 Annex C.2): `GainMapVersion` + flags (`is_multichannel` → 1 or 3 R/G/B channels, `use_base_colour_space`), base/alternate HDR headroom, and per-channel `GainMapChannel` rationals (min/max/gamma/base+alternate offset). Enforces every §5.2 / Annex C.2.3 `shall` (non-zero denominators, non-zero `gamma_numerator`, `writer_version ≥ minimum_version`, per-channel `gain_map_max ≥ gain_map_min` per §5.2.5.3 — value-comparison via cross-multiplied i64 so `max == min` is permitted, `alternate_hdr_headroom ≠ base_hdr_headroom` per §5.2.7 — also value-comparison so e.g. `1/1` and `2/2` trip the check), returns `Unsupported` for an unknown `minimum_version`, and ignores trailing padding / future-optional bytes. One-call extractor `gain_map_metadata(file, tmap_item_id)` resolves a tmap item's `iloc` payload and runs the parse, mirroring the existing `item_payload_bytes` accessor pattern |
| AV1 layered properties (`a1op`/`a1lx`) | `a1op` operating-point selector (u8 `op_index`) + `a1lx` layered-image index (`layer_size[3]`, 16/32-bit fields, `documented_layers()`) parsed per av1-avif §2.3.2; surfaced via `AvifInfo::{operating_point, layered_index}` |
| Essential-property enforcement         | `Meta::{unsupported_essential_properties, has_unsupported_essential_property}` flag any `ipma`-essential property that lands in `Property::Other`; a reader must not process such an item (av1-avif §2.3.2.1.2 + MIAF §7.3.5) |
| Auxiliary classification (`AuxKind`)   | `auxC` URN routed to `Alpha` / `DepthMap` / `HdrGainMap` / `Other` covering MPEG, HEVC-HEIF, and Apple gain-map spellings; `AvifInfo` exposes `aux_items` + per-kind item-id helpers |
| Derived images (`iovl`, `iden`)        | `iovl` ImageOverlay descriptor parsed (16-bit + 32-bit field widths, signed offsets); `iden` item-type constant exported. Composition pending an AV1 decoder for the sources |
| Entity grouping (`grpl`)               | `GroupsListBox` walk emits typed `EntityGroup` per `EntityToGroupBox`; `altr` / `ster` / `eqiv` recognised via `is_alternates()` / `is_stereo_pair()` / `is_equivalence()` (HEIF §9.4) |
| Brand compliance audit                 | `audit_mif1` (HEIF §10.2.1.1): reports per-box presence + the `mif1` brand claim, returning a `Mif1Compliance { is_compliant(), missing() }`. Pinned against the Microsoft monochrome fixture |
| Metadata items (`Exif`, XMP)           | `cdsc` iref walker resolves Exif (`item_type == 'Exif'` and `mime`-wrapped `application/octet-stream` / `image/tiff` / `image/x-exif`) + XMP (`mime` + `application/rdf+xml`) attached to the primary; surfaced as `AvifInfo::{exif_item_id, xmp_item_id, has_descriptive_metadata()}`. Raw bytes are extracted on demand via `item_payload_bytes` |
| Thumbnails                             | `thmb` iref enumeration: `AvifInfo::thumbnail_item_ids` lists every thumbnail item attached to the primary; `has_thumbnails()` shorthand |
| Premultiplied-alpha signalling         | HEIF `prem` iref (`from_id` = alpha auxiliary, `to_ids` includes the colour image) is detected and surfaced as `AvifInfo::premultiplied_alpha` |
| `infe` v2/v3 tail fields               | `mime` items: `content_type` + optional `content_encoding` (empty string normalised to `None`); `uri ` items: `item_uri_type`. All exposed on `ItemInfo` so callers can route generic carriers without re-parsing the box |
| CICP color signalling                  | `colr` nclx → `CicpTriple` (primaries / transfer / matrix / full_range) with H.273 defaults (`Unspecified` = `2/2/2/false`); ICC + Unknown fall back to Unspecified; alpha auxiliary CICP constant carries `full_range = true` per av1-avif §4.1 |
| HDR metadata                           | `mdcv` (SMPTE ST 2086 mastering display primaries + luminance), `clli` (MaxCLL / MaxFALL cd/m²), `cclv` (draft av1-avif extension, same layout as `clli`); surfaced via `AvifInfo::{mdcv, clli, cclv, has_hdr_metadata(), max_cll(), max_fall()}` |
| AV1 wrap pass-through                  | `av1C`-derived bit depth (8/10/12-bit), monochrome flag, and chroma subsampling `(x, y)` decoded and surfaced via `AvifInfo::{bit_depth, monochrome, chroma_subsampling}`; callers no longer need to re-parse `av1C` |
| Sequence Header OBU audit (§2.1)       | av1-avif §2.1 `shall` "The AV1 Image Item Data shall have exactly one Sequence Header OBU" audited at the container layer via `audit_sequence_header_obu(meta, file)` / `SequenceHeaderObuAudit`. One record per `'av01'` item walks the OBU framing (AV1 §5.3.1 header byte + §4.10.5 leb128 `obu_size` + §6.2.1 `obu_type` table) and reports `sequence_header_count`, `total_obu_count`, plus structural failure flags (`missing_iloc` / `truncated_obu` / `has_size_field_zero`). Surfaced via `AvifInfo::sequence_header_obu_compliance` / `sequence_header_obu_strict_compliant()` |
| Primary item data                      | resolved via `iloc` construction_method 0 (file offset); single-extent items return a zero-copy slice; multi-extent items are concatenated via `item_bytes_owned()` (HEIF §8.11.3.3) |
| Grid primary items (HEIF §6.6.2)       | grid descriptor parse + per-tile decode via `dimg` iref + composite into the declared output rectangle; plus av1-avif §7 derivation-chain audit (`audit_grid_derivations` / `GridDerivationAudit`) flagging any `clap` / `irot` / `imir` / `iscl` attached to a tile in violation of the "transformative properties only on the grid item itself" `shall` — `rref` is descriptive and is not flagged. Surfaced via `AvifInfo::grid_derivation_compliance` / `grid_derivations_strict_compliant()` |
| Alpha auxiliary                        | `auxl` + `auxC` URN detection, AV1-coded monochrome item decoded, composited onto the color frame (`Gray8 → YA8`, `Yuv → YuvA`); plus av1-avif §4.1 alpha-vs-master bit-depth `shall` audit (`audit_alpha_bit_depth` / `AlphaBitDepthAudit`) surfaced via `AvifInfo::alpha_bit_depth_compliance` / `alpha_bit_depth_strict_compliant()` |
| Post-transforms                        | `clap` (centre crop) → `irot` (90/180/270°) → `imir` (horizontal/vertical), applied in that order per §6.5.10                                              |
| AV1 hand-off                           | `av1C` plumbed through `CodecParameters::extradata`; primary-item OBU payload fed to `oxideav_av1::Av1Decoder`; frame returned through `AvifDecoder`       |
| MIAF profile dispatch                  | `BrandClass` flags `is_baseline_profile` (MA1B) + `is_advanced_profile` (MA1A) + `is_miaf`; surfaced through `AvifInfo::brands`. Plus av1-avif §8.2 / §8.3 `shall`-level audit (`audit_avif_profile_compliance` / `AvifProfileCompliance`) that walks each AV1 Image Item's `av1C[1]` for the `(seq_profile, seq_level_idx_0)` pair and reports whether it satisfies the declared brand's bounds (Baseline: Main + level ≤ 5.1; Advanced: ≤ High + level ≤ 6.0); surfaced via `AvifInfo::avif_profile_compliance` / `avif_profile_strict_compliant()` |
| AVIS image sequences                   | sample-table walk (`parse_avis` / `sample_table`) emits a flat frame-offset list; caller feeds each sample to `oxideav_av1` for sequential decode. Plus av1-avif §3 `shall`-level audit (`audit_avis_sequence` / `AvisSequenceCompliance`): track `mdia/hdlr/handler_type == 'pict'`, `stsd` carries exactly one `'av01'` SampleEntry, and Sequence Header OBUs across samples are byte-identical. `AvisMeta` surfaces `handler` + `sample_description_types`. Plus av1-avif §8.2 / §8.3 sequence-track profile compliance audit (`audit_avis_profile_compliance` / `AvisProfileCompliance`): when the file declares `MA1B` and/or `MA1A`, the track's `stsd → av01 → av1C` byte 1 (`(seq_profile, seq_level_idx_0)`) is checked against the per-profile bounds (Baseline: Main + level ≤ 5.1; Advanced: ≤ High + level ≤ 6.0), one record per declared brand. Plus ISO/IEC 14496-12 §8.6.6 `edts/elst` edit list parsed into `AvisMeta::edit_list` (v0 + v1 entry shapes widened; per-entry `is_empty_edit()` / `is_dwell()` helpers) + §8.6.6.3 `shall`-level audit (`audit_edit_list` / `EditListCompliance`): no trailing empty edit (`media_time == -1`) and every `media_rate_integer ∈ {0, 1}` (0 = dwell, 1 = normal-rate). Vacuous-pass for tracks without `edts` (the §8.6.5 implicit-identity case). Plus the [`AvisInfo`] aggregator surfaced via `inspect_avis(file)`: one call folds `parse_avis` + `classify_brands` + every AVIS-side audit into one record (with summary fields `timescale`, `media_timescale`, `display_dims`, `sample_count`, `total_sample_duration`, `has_av1_codec_config`, `has_edit_list`, `brands`) and exposes `is_compliant_all()` + `missing_all()` + `duration_seconds()` + `media_duration_seconds()` + `is_avis_brand()`. Plus ISO/IEC 14496-12 §8.4.2.2 `mdhd` media-timescale plumb: `AvisMeta::media_timescale` + `AvisInfo::media_timescale` populated from the first track's `mdia/mdhd` (v0 + v1), forward-compatible silence for missing / truncated / `version > 1` shapes. `EditListEntry::media_time_seconds(media_timescale)` + `segment_duration_seconds(movie_timescale)` consume the field for spec-correct timeline conversions |
| Encoder                                | **not implemented**: no AV1 encoder exists in oxideav                                                                                                      |

### What decodes

- Tiny flat-content AVIFs (reference-encoder-produced 16x16..64x64 mono
  or lossless 4:4:4) — sample means land within 1-2 units of the target
  value. See `tests/fixtures/{gray32,midgray,white16,red,black420}.avif`
  and the `decodes_flat_gray_to_mid_value` integration test.
- The 1280×720 `monochrome.avif` conformance fixture —
  `send_packet`/`receive_frame` succeed and return a full 1280×720
  Gray8 plane with a plausible brightness histogram.

### What fails / lossy

- Rich / natural-image AVIFs — the decoded YUV planes collapse toward
  mid-gray (intra edge filter + chroma intra still imperfect in the
  av1 crate). For the `testsrc` intra baseline in `oxideav-av1` PSNR
  hovers around 11 dB.
- `bbb_alpha.avif` (3840×2160 4:2:0 + alpha) — the AV1 layer rejects
  the bottom-edge `TX 64×56` shape (§5.11.27). The AVIF container
  handoff is verified end-to-end (alpha auxiliary item is correctly
  located and its OBU stream is well-formed) — the failure is in
  the AV1 crate's TX-set coverage, not the AVIF wrapper. A previous
  panic at `symbol.rs:105` is no longer reproducible — the av1 crate
  now surfaces a clean `Unsupported`.
- `kimono_rotate90.avif` (1024×722 4:2:0) — rejected by av1 as
  "TX 32×41 not in the AV1 set"; the irregular bottom edge
  (722 mod 64 = 18) lands on a TX size oxideav-av1 doesn't yet
  emit. The AVIF container code surfaces the error verbatim, and the
  `irot` property is exposed via `transforms_for` for callers that
  want to apply it themselves.

See `examples/diag_decode.rs` for a drop-in report of exactly which
stage each input reaches.

### Round 241 — HEIF §6.5.20 `udes` user-description item property

The descriptive item-property rollout continues with §6.5.20
UserDescriptionProperty, which pairs the associated item or entity
group with a human-readable name, description, and a comma-separated
tag list, all carried in a single language. Per §6.5.20.1 the
property is descriptive with `Quantity (per associated item_ID):
Zero or more`, and multiple instances on the same item shall carry
**different** language codes — they represent the same content
translated for different audiences, from which a reader picks the
most appropriate.

The wire layout is taken verbatim from §6.5.20.2 — a
FullBox(`udes`, version=0, flags=0) followed by four sequential
NUL-terminated UTF-8 strings:

```text
utf8string lang;
utf8string name;
utf8string description;
utf8string tags;
```

Each field's empty-string form (a single NUL byte) is the documented
§6.5.20.3 "absent" sentinel: empty `lang` = unknown/undefined
language, empty `name` = no name provided, empty `description` = no
description provided, empty `tags` = no tags provided.

The parser preserves every string verbatim — including the
empty-string sentinel — and surfaces a strongly typed projection
via four `*_opt` helpers and a derived tag view:

```text
Property::Udes(Udes {
    lang: String,        // RFC 5646 language tag, empty = unknown
    name: String,        // empty = absent
    description: String, // empty = absent
    tags: String,        // empty = absent
})

Udes::lang_opt()         -> Option<&str>     // None when empty
Udes::name_opt()         -> Option<&str>     // None when empty
Udes::description_opt()  -> Option<&str>     // None when empty
Udes::tags_opt()         -> Option<&str>     // None when empty (raw form)
Udes::tag_list()         -> Vec<&str>        // split on ',' + trim
```

`tag_list` realises the §6.5.20.3 "comma-separated user-defined
tags" shape: it splits the `tags` field on `','`, trims whitespace
per segment, and drops empty / whitespace-only segments so a caller
iterating the result gets a clean tag list. The raw `tags` field is
preserved untouched for callers that want the on-wire form.

Forward-compatibility behaviour matches §8.11.6 `infe`: trailing
bytes past the fourth NUL terminator are ignored at parse time, so a
v0 producer that pads the box with reserved bytes for a future spec
revision is read cleanly. An unknown `version` value is rejected so
a future-version layout (which might re-shape the field order or
widths) cannot be misread as v0, and a body that runs out before all
four NUL terminators have been observed is rejected by `read_cstr`
rather than producing a partially-populated `Udes`.

A recognised `udes` property — even when unusually flagged essential
in the `ipma` association — does not trip
[`Meta::unsupported_essential_properties`], joining the previously
recognised `clap` / `irot` / `imir` / `lsel` / `a1op` / `a1lx` /
`iscl` / `rref` / `crtt` / `mdft` properties on the always-honoured
list. (`udes` is descriptive per §6.5.20.1, so the §7
grid-derivation audit is untouched — transformative-property scope
only.)

Test delta: +11 unit (`udes_round_trip_reads_all_four_fields`,
`udes_empty_strings_are_preserved_and_projectable_to_none`,
`udes_opt_helpers_round_trip_non_empty`,
`udes_tag_list_splits_and_trims`, `udes_preserves_utf8_multibyte`,
`udes_rejects_unknown_version`, `udes_rejects_truncated_body`,
`udes_tolerates_trailing_bytes`,
`udes_dispatched_through_parse_ipco`,
`udes_essential_association_is_recognised`,
`udes_multiple_languages_coexist_on_same_item`). Default lib 364
(was 353); standalone lib 349 (was 338); integration 61 + 1 ignored
unchanged. Re-exports: `oxideav_avif::Udes`.

Followups: §6.5.21 AccessibilityTextProperty (`altt`) is the next
property in the §6.5 sequence — a `Quantity: Zero or more`
descriptive item property whose body is two sequential `utf8string`
fields (`alt_text` + `alt_lang`), structurally a strict subset of
`udes`. §6.5.22 / §6.5.23 / §6.5.24 (`aebr` / `wbbr` / `fobr`) are
the capture-side numeric descriptive properties (exposure
numerator/step, blue-amber / green-magenta white balance,
focus-distance rational) — each a single fixed-width payload, so
the parsers would be three sibling commits in the same shape as
`crtt` / `mdft`.

### Round 238 — HEIF §6.5.19 `mdft` modification-time item property

The HEIF item-properties rollout continues with §6.5.19
ModificationTimeProperty, the descriptive sibling of §6.5.18 `crtt`
that documents the most recent modification time of the associated
item or entity group. The body shape is taken verbatim from
ISO/IEC 23008-12:2025 §6.5.19.2 — a FullBox(`mdft`, version=0,
flags=0) carrying a single `unsigned int(64) modification_time` field
— and the time unit is microseconds since midnight, Jan. 1, 1904 UTC
per §6.5.19.3, identical to `crtt`'s epoch / unit.

```text
parse_ipco(...) -> Vec<Property>
    ... + Property::Mdft(Mdft { modification_time: u64 })

Mdft::seconds_since_unix_epoch() -> Option<u64>
Mdft::subsecond_micros()         -> u32
```

The wire layout mirrors `crtt` exactly (same FullBox header, same u64
field width, same 1904-epoch microsecond unit), so the parser is
structurally identical — only the box four-CC and the surfaced struct
differ. The two properties may legally co-occur on the same item;
when both are associated, `Meta::property_for(id, &b"crtt")` and
`Meta::property_for(id, &b"mdft")` resolve each independently to yield
a creation/modification pair. The spec does not require
`modification_time >= creation_time`, but a well-formed writer would
honour that ordering.

`Mdft::seconds_since_unix_epoch` returns `None` for a pre-1970
timestamp (the `u64` field cannot represent a signed offset, so the
helper avoids underflow by returning the option rather than wrapping
into a nonsense future date), reusing the same `2_082_844_800`-second
1904→1970 offset constant introduced for `crtt`.

The parser rejects unknown `version` values so a future-version layout
cannot be misread as v0, and a body shorter than the 8-byte
`modification_time` field is rejected at parse time rather than
silently zero-extended.

A recognised `mdft` property — even when unusually flagged essential
in the `ipma` association — does not trip
[`Meta::unsupported_essential_properties`], joining the previously
recognised `clap` / `irot` / `imir` / `lsel` / `a1op` / `a1lx` /
`iscl` / `rref` / `crtt` properties on the always-honoured list.
(`mdft` is descriptive per §6.5.19.1, so the §7 grid-derivation audit
is untouched — transformative-property scope only.)

Test delta: +9 unit (`mdft_round_trip_reads_modification_time`,
`mdft_rejects_truncated_body`, `mdft_rejects_unknown_version`,
`mdft_rejects_missing_payload`, `mdft_dispatched_through_parse_ipco`,
`mdft_seconds_since_unix_epoch_matches_documented_offset`,
`mdft_subsecond_micros_isolates_remainder`,
`mdft_essential_association_is_recognised`,
`mdft_and_crtt_coexist_on_same_item`). Default lib 353 (was 344);
standalone lib 338 (was 329); integration 61 + 1 ignored unchanged.
Re-exports: `oxideav_avif::Mdft`.

Followups: §6.5.20 UserDescriptionProperty (`udes`) is the next
property in the §6.5 sequence — a `Quantity: Zero or more`
descriptive item property whose body opens with a 16-bit ISO 639-2/T
language packed code followed by four NUL-terminated `utf8string`
fields (name / description / tags / lang); it introduces the
multi-`utf8string` body shape to the item-property plane. §6.5.21
AccessibilityText (`altt`) follows the same packed-language plus
string-pair scaffolding.

### Round 233 — HEIF §6.5.18 `crtt` creation-time item property

The HEIF item-properties rollout continues with §6.5.18
CreationTimeProperty, a descriptive item property documenting the
creation time of the associated item or entity group. The body shape
is taken verbatim from ISO/IEC 23008-12:2025 §6.5.18.2 — a
FullBox(`crtt`, version=0, flags=0) carrying a single
`unsigned int(64) creation_time` field — and the time unit is
microseconds since midnight, Jan. 1, 1904 UTC per §6.5.18.3.

```text
parse_ipco(...) -> Vec<Property>
    ... + Property::Crtt(Crtt { creation_time: u64 })

Crtt::seconds_since_unix_epoch() -> Option<u64>
Crtt::subsecond_micros()         -> u32
```

The 1904 epoch matches the legacy QuickTime / ISOBMFF movie-header
epoch (ISO/IEC 14496-12 §8.2.2), but the §6.5.18 unit is
*microseconds* rather than the *seconds* used by `mvhd` / `tkhd` /
`mdhd`, so a reader comparing this property against an AVIS track
timestamp must scale by `10^6` in the appropriate direction. The
1904→1970 offset is `2_082_844_800` seconds (66 calendar years × 365
days + 17 leap-year days × 86 400 s/day), captured as a single
module-level constant for direct inspection. `seconds_since_unix_epoch`
returns `None` for a pre-1970 timestamp (the `u64` field cannot
represent a signed offset, so the helper avoids underflow by
returning the option rather than wrapping into a nonsense future
date).

The parser rejects unknown `version` values so a future-version
layout cannot be misread as v0, and a body shorter than the 8-byte
`creation_time` field is rejected at parse time rather than silently
zero-extended.

A recognised `crtt` property — even when unusually flagged essential
in the `ipma` association — does not trip
[`Meta::unsupported_essential_properties`], joining the previously
recognised `clap` / `irot` / `imir` / `lsel` / `a1op` / `a1lx` /
`iscl` / `rref` properties on the always-honoured list. (`crtt` is
descriptive per §6.5.18.1, so the §7 grid-derivation audit is
untouched — transformative-property scope only.)

Test delta: +8 unit (`crtt_round_trip_reads_creation_time`,
`crtt_rejects_truncated_body`, `crtt_rejects_unknown_version`,
`crtt_rejects_missing_payload`, `crtt_dispatched_through_parse_ipco`,
`crtt_seconds_since_unix_epoch_matches_documented_offset`,
`crtt_subsecond_micros_isolates_remainder`,
`crtt_essential_association_is_recognised`). Default lib 344 (was
336); standalone lib 329 (was 321); integration 61 + 1 ignored
unchanged. Re-exports: `oxideav_avif::Crtt`.

Followups: §6.5.19 ModificationTimeProperty (`mdft`) is the obvious
sibling — identical FullBox + u64 µs layout, so the same shape can
be lifted from the `crtt` parser; §6.5.20 UserDescriptionProperty
(`udes`) adds the first multi-`utf8string` body in the item-property
plane (lang / name / description / tags). §6.5.21 AccessibilityText
(`altt`) follows the same string-pair shape.

### Round 230 — HEIF §6.5.13 `iscl` + §6.5.17 `rref` item properties

The r172 follow-up — "HEIF defines additional transformative properties
(`'iscl'` image scaling, `'rref'` required reference) the audit doesn't
yet flag" — lands as a two-property parse + a §7 audit extension. Both
property bodies are taken straight from ISO/IEC 23008-12:2025 §6.5.13
(ImageScaling) and §6.5.17 (RequiredReferenceTypesProperty); both are
exposed as typed [`Property`] variants and dispatched through
[`parse_ipco`] alongside the other recognised properties.

```text
parse_ipco(...) -> Vec<Property>
    ... + Property::Iscl(Iscl) | Property::Rref(Rref)

audit_grid_derivations(meta) -> Vec<GridDerivationAudit>
    ... offenders now include (tile_id, 'iscl') as well as
        (tile_id, 'clap' | 'irot' | 'imir')
```

`Iscl` carries the four `unsigned int(16)` fields verbatim
(`target_width_numerator`, `target_width_denominator`,
`target_height_numerator`, `target_height_denominator`). The §6.5.13.3
non-zero-numerator-and-denominator `shall` is exposed as
[`Iscl::is_well_formed`] (the parser surfaces the bytes as written so a
file that ships a zero field still decodes — the semantic check sits on
the type, matching the pattern used by the existing rational-bearing
property parsers in this module). [`Iscl::scaled_dims`] folds the
§6.5.13.1 formula — `ceil((input * numerator) / denominator)` — using
u64 intermediate arithmetic so no `u16 × u32` product overflows, with
the result saturating to `u32::MAX` if it would otherwise wrap.

`Rref` carries the §6.5.17.2 list as a typed `Vec<BoxType>` (each
`reference_type[i]` is a `u32` four-CC), plus convenience helpers
[`Rref::count`] and [`Rref::requires`]. The `reference_type_count`
field is captured implicitly as `reference_types.len()`. A declared
count that exceeds the available body bytes is rejected at parse time
(per §6.5.17 a reader that fails to honour every listed type `shall`
refuse to process the associated item, so a partial read defeats the
property's purpose).

Both parsers reject unknown `version` values (per the spec's
`version = 0` declaration in the syntax block); a future-version
layout cannot be misread as v0.

The av1-avif §7 grid-derivation audit was extended to flag `iscl` as a
transformative property on tiles (HEIF §6.5.13 explicitly classifies
it as a transformative item property). `rref` is descriptive and is
therefore **not** flagged — the §7 `shall` is scoped to transformative
properties only.

Recognised `iscl` and `rref` essential associations no longer trip
[`Meta::unsupported_essential_properties`] (the doc enumerates them
alongside the previously-recognised `clap` / `irot` / `imir` / `lsel`
/ `a1op` / `a1lx`).

Test delta: +18 unit (`iscl_round_trip_reads_all_four_fields`,
`iscl_rejects_truncated_body`, `iscl_rejects_unknown_version`,
`iscl_is_well_formed_rejects_zero_field` covering each of the four
fields, `iscl_scaled_dims_applies_ceil_division` with three ratio
shapes including the identity 1:1 case,
`iscl_scaled_dims_short_circuits_zero_denominator`,
`iscl_scaled_dims_saturates_on_u32_overflow`,
`iscl_dispatched_through_parse_ipco`,
`rref_round_trip_reads_typed_four_ccs`, `rref_empty_list_parses`,
`rref_rejects_truncated_table`, `rref_rejects_unknown_version`,
`rref_rejects_missing_count`, `rref_dispatched_through_parse_ipco`,
`iscl_and_rref_essential_associations_are_recognised`,
`audit_grid_derivations_tile_iscl_flagged`,
`audit_grid_derivations_tile_rref_not_flagged`,
`audit_grid_derivations_grid_level_iscl_permitted`); the
pre-existing `tile_with_all_three_kinds` test widened to
`tile_with_all_four_kinds` to cover the new `iscl` kind without
losing the original three-kind shape. Default lib 336 (was 318);
standalone lib 321 (was 303); integration 61 + 1 ignored unchanged.

Followup: the §4.1 alpha-bit-depth sequence-track audit (covering
AVIS files with an alpha auxiliary stream, noted as a r206 / r224
follow-up) is unchanged — it still needs [`AvisMeta`] to grow
multi-track support before [`inspect_avis`] can surface an
`alpha_bit_depth_compliance` field analogous to the still-image one.
The §6.5.13 `iscl` transformative composition path (actually applying
the scaling to the reconstructed image) is deferred — the property is
now surfaced at the container layer, but the pixel-side scaler still
needs to land in [`transform`].

### Round 224 — `mdhd` media-timescale plumb (ISO/IEC 14496-12 §8.4.2.2)

The repeated r212 / r218 follow-up — "plumbing `mdhd` (the media
timescale) is still on the table; today `media_time` is in raw
media-timescale units" — lands as one field on `AvisMeta` and one
field on [`AvisInfo`] plus a small set of conversion helpers:

```text
parse_avis(file).media_timescale: Option<u32>
inspect_avis(file).media_timescale: Option<u32>
EditListEntry::media_time_seconds(media_timescale) -> Option<f64>
EditListEntry::segment_duration_seconds(movie_timescale) -> Option<f64>
AvisInfo::media_duration_seconds() -> Option<f64>
```

`parse_avis` walks the first track's `trak/mdia/mdhd` and pulls the
32-bit `timescale` field per ISO/IEC 14496-12 §8.4.2.2:

- **v0:** `creation_time(32) + modification_time(32) + timescale(32) +
  duration(32) + language(16) + pre_defined(16)` — timescale at body
  offset 8.
- **v1:** `creation_time(64) + modification_time(64) + timescale(32) +
  duration(64) + language(16) + pre_defined(16)` — timescale at body
  offset 16.

A missing `mdhd`, a truncated body, or a future `version > 1` value
silently surfaces as `None` (forward-compatible — no error returned
to callers; the AVIS still parses).

`EditListEntry::media_time_seconds` divides `media_time` (a media-
timescale signed-integer) by the supplied `media_timescale`. Returns
`None` for the §8.6.6.3 empty-edit sentinel (`media_time == -1`,
which isn't a position on the media timeline) and for the
degenerate `media_timescale == 0` case. The parallel
`segment_duration_seconds` divides `segment_duration` (a
movie-timescale unsigned) by `mvhd::timescale` — they're separate
helpers because the two fields live on different timelines per the
spec.

`AvisInfo::media_duration_seconds` computes
`total_sample_duration / media_timescale` — the spec-correct
conversion for the accumulated `stts` per-sample deltas. Per
ISO/IEC 14496-12 §8.6.1.2 (`stts`), per-sample `delta` values are
in media-timescale units, so dividing by `mvhd::timescale`
(`AvisInfo::duration_seconds`) is only correct when the encoder
sets `mvhd::timescale == mdhd::timescale`, a common but not
universal default. When the two differ this helper is the
spec-correct one; when they agree both report the same number.

Coverage details:

- A truncated v0 `mdhd` body (shorter than the 12-byte cursor
  needed to reach `timescale + 4`) returns `None` rather than
  panicking; same for a v1 body shorter than 20 bytes.
- A future-version (`version > 1`) `mdhd` returns `None` — the
  layout past v1 isn't defined here, so we don't guess.
- `AvisInfo::media_duration_seconds` short-circuits to `None` when
  either `media_timescale` is `None` (no `mdhd`) or `Some(0)`
  (degenerate timescale). Callers wanting a fallback gate should
  `media_duration_seconds().or_else(|| duration_seconds())`.

Test delta: +14 unit (`find_first_track_media_timescale_*`
covering v0 / v1 reads, absent / truncated / future-version cases;
`edit_list_entry_media_time_seconds_*` covering normal entry, the
empty-edit `None`, and the zero-timescale `None`;
`edit_list_entry_segment_duration_seconds_*` covering the normal
case and the zero-timescale `None`;
`build_avis_info_media_timescale_*` covering carry-through, the
absent-`mdhd` case, the differing-timescale case where
`media_duration_seconds` and `duration_seconds` diverge, and the
zero `media_timescale` short-circuit). +1 integration
(`inspect_avis_resolves_media_timescale_for_alpha_video_fixture`)
pins the resolved field on the real Netflix `alpha_video.avif`
fixture. Default lib 318 (was 304); standalone lib 303 (was 289);
integration 61 + 1 ignored (was 60 + 1).

Followup: this round resolves the second half of the r212 follow-up
statement. The r206 §4.1 alpha-bit-depth sequence-track audit
(covering AVIS files with an alpha auxiliary stream) is unchanged
— it still needs `AvisMeta` to grow multi-track support before
`inspect_avis` can surface an `alpha_bit_depth_compliance` field
analogous to the still-image one.

### Round 218 — AVIS aggregator (`inspect_avis` / `AvisInfo`)

The repeated r201 / r206 / r212 follow-up — "the AVIS path's `AvifInfo`
does not yet surface the audit the way `AvifInfo::avif_profile_compliance`
does for items; today the caller pairs `parse_avis` + `classify_brands` +
the new audit directly" — lands as a single-record builder paralleling
the still-image [`inspect`] entry point:

```text
inspect_avis(file: &[u8]) -> Result<AvisInfo>
```

`AvisInfo` carries summary fields populated from the underlying parse
(`timescale`, `display_dims`, `sample_count`, `total_sample_duration`,
`has_av1_codec_config`, `handler`, `sample_description_types`, `brands`,
`has_edit_list`) plus the three AVIS-side compliance records folded in
the same call:

```rust,ignore
pub struct AvisInfo {
    pub timescale: u32,
    pub display_dims: Option<(u32, u32)>,
    pub sample_count: u32,
    pub total_sample_duration: u64,
    pub has_av1_codec_config: bool,
    pub handler: Option<BoxType>,
    pub sample_description_types: Vec<BoxType>,
    pub brands: BrandClass,
    pub has_edit_list: bool,
    pub sequence_compliance: AvisSequenceCompliance,    // §3
    pub profile_compliance: Vec<AvisProfileCompliance>, // §8.2 / §8.3
    pub edit_list_compliance: EditListCompliance,       // §8.6.6.3
}
```

Helpers: `is_compliant_all()` ANDs every audited `shall` across the
three records (vacuously `true` when `profile_compliance` is empty —
i.e. the file claims neither `MA1B` nor `MA1A`); `missing_all()`
concatenates the three `missing()` lists in deterministic audit order
(§3 → §8.2/§8.3 → §8.6.6.3); `duration_seconds()` folds
`total_sample_duration / timescale` (and returns `None` when
`timescale == 0`); `is_avis_brand()` reflects `BrandClass::is_sequence`
(`ftyp` actually claimed `avis`, not just "had a `moov`"); `frame_count()`
mirrors `sample_count`.

The aggregator introduces no new `shall`-level normative material:
every audited rule is forwarded verbatim from the existing per-audit
walkers. The value is one-call ergonomics + a record shape that
matches the still-image `AvifInfo` pattern (single record carrying
every compliance flag + summary signal needed to make presence-
versus-correctness decisions without re-walking the file).

Coverage details:

- `inspect_avis` errors on a missing `ftyp` / `moov` / `stbl` (every
  failure mode surfaces verbatim from `parse_header` /
  `classify_brands` / `parse_avis` — the aggregator does not change
  the error surface).
- An AVIS file that ships no `edts` produces
  `edit_list_compliance.is_compliant() == true` (the §8.6.5
  implicit-identity case) and `has_edit_list == false`. Callers
  wanting "explicit edit list ∧ compliant" should AND both.
- A file declaring neither `MA1B` nor `MA1A` produces an empty
  `profile_compliance` vector; `is_compliant_all()` is then trivially
  `true` for the profile dimension. Callers wanting "profile claim ∧
  compliant" should combine `is_compliant_all()` with `brands.is_baseline_profile
  || brands.is_advanced_profile`.

Test delta: +9 unit (`avis::tests::build_avis_info_*` covering the
clean-aggregate happy path, `duration_seconds` undefined for
`timescale == 0`, no-brand-claim empty profile vector, dual-brand
record ordering, `is_avis_brand` reflecting the sequence flag,
`missing_all` token ordering across audits, `total_sample_duration`
u64 widening, `has_edit_list` presence flag, and `has_av1_codec_config`
tracking the `av1C` presence). +1 integration
(`inspect_avis_aggregates_alpha_video_fixture_to_compliant`) pins
the aggregator on the real Netflix `alpha_video.avif` fixture
(640×480, declares `MA1B`, all three audits clear). Default lib 304
(was 295); standalone lib 289 (was 280); integration 60 + 1 ignored
(was 59 + 1).

Followup: plumbing `mdhd` (the media timescale, as noted in the
r212 section) is still on the table — today `media_time` is in raw
media-timescale units and `total_sample_duration` is in
movie-timescale units, so reconstructing a single presentation
duration that respects both still requires reading `tkhd::duration`
plus the edit list's `segment_duration` outside the aggregator.
The §4.1 alpha-bit-depth sequence-track follow-up (covering AVIS
files with an alpha auxiliary stream) is unchanged from r206 — it
needs `AvisMeta` to grow multi-track support before `inspect_avis`
can surface an `alpha_bit_depth_compliance` field analogous to the
still-image one.

### Round 212 — ISO/IEC 14496-12 §8.6.6 AVIS edit list (`edts/elst`)

An AVIS track's presentation timeline is reshaped by the optional
`Edit Box` (`edts`), which contains a single `EditListBox` (`elst`)
mapping the movie timeline onto the media timeline (§8.6.5 + §8.6.6).
Round 212 lifts the `elst` into `AvisMeta` and adds the §8.6.6.3
`shall`-level audit.

```text
parse_avis(file).edit_list: Vec<EditListEntry>
audit_edit_list(&AvisMeta) -> EditListCompliance
```

`AvisMeta` grows one field: `edit_list: Vec<EditListEntry>`,
populated by `parse_avis` from the first track's
`trak/edts/elst`. v0 (32-bit `segment_duration` / signed-32
`media_time`) and v1 (64-bit / signed-64) entries are widened to the
same `EditListEntry` shape so callers stay version-agnostic:

```rust,ignore
pub struct EditListEntry {
    pub segment_duration: u64,
    pub media_time: i64,
    pub media_rate_integer: i16,
    pub media_rate_fraction: i16,
}
```

`EditListEntry::is_empty_edit()` flags the §8.6.6.3 sentinel
`media_time == -1` (the presentation advances while no media is
presented — used to offset a track's start by an initial delay).
`EditListEntry::is_dwell()` flags `media_rate_integer == 0` (the
single media frame at `media_time` is held for `segment_duration`).

The audit reports both §8.6.6.3 normative `shall`s:

1. **Last entry not empty.** "The last edit in a track shall never be
   an empty edit." Vacuously satisfied when the edit list is empty
   (a file without an `edts` has no "last edit" to constrain).
2. **`media_rate_integer` in `{0, 1}`.** "Otherwise this field shall
   contain the value 1." Dwell (`0`) and normal-rate (`1`) are
   accepted; every other integer value (including negatives) trips
   the check.

`EditListCompliance` carries one bool per `shall`, four diagnostic
fields (`entry_count`, `empty_edit_count`, `dwell_entry_count`,
`out_of_range_rate_count`), and `is_compliant()` / `missing()`
helpers shaped exactly like the other AVIS audits. Diagnostic
tokens: `avis-edit-list-last-entry-empty`,
`avis-edit-list-media-rate-out-of-range`.

Coverage details:

- Future-version (v2+) `elst` payloads silently produce an empty
  entry list — a forward-compatible reader can fall back to identity
  mapping, and the audit then trivially passes. v0 / v1 both parse
  end-to-end.
- A truncated entry table stops the walk cleanly: every well-formed
  entry up to the truncation point is returned (no error), matching
  the project's permissive-parse + strict-audit split used by the
  other AVIS audits.
- The §8.6.6.3 audit treats an empty edit list as vacuously
  compliant — exposing the implicit-identity case as a positive
  signal rather than an "unknown" tri-state.

Test delta: +14 unit
(`avis::tests::parse_edit_list_*` covering v0 single normal entry,
v0 empty-edit sign-extension to `i64`, v1 large `media_time` round
trip past `i32::MAX`, truncated entry table stop, future-version
silent skip; `avis::tests::find_first_track_edit_list_*` covering
the moov-payload plumb + absent-`edts` empty case;
`avis::tests::audit_edit_list_*` covering the empty-list vacuous
pass, leading-empty + normal compliant shape, trailing-empty
flagged, dwell-entry counted, out-of-range positive rate flagged,
negative rate flagged, and both-`shall`s-fail aggregation).
Default lib 295 (was 281); standalone lib 295 (was 281); integration
59 + 1 ignored (unchanged).

Followup: the `inspect_avis` aggregator noted as a r206 follow-up
remains the natural next step — it would surface the new
`EditListCompliance` alongside `AvisSequenceCompliance` and
`AvisProfileCompliance` in a single record. Plumbing `mdhd` (the
media timescale) is also still on the table; today `media_time`
is reported in raw media-timescale units, and `tkhd::duration` plus
the edit list's `segment_duration` together reconstruct the
presentation duration — but no helper consolidates them yet.

### Round 206 — av1-avif §8.2 / §8.3 AVIS profile compliance audit

The §8.2 `MA1B` Baseline / §8.3 `MA1A` Advanced profile audit (landed
round 195 for still-image AV1 Image Items via
[`audit_avif_profile_compliance`]) now has a sequence-track companion
that inspects the AVIS track's `AV1CodecConfigurationRecord` instead of
the per-item `iprp.ipco` association:

```text
audit_avis_profile_compliance(&AvisMeta, &BrandClass)
    -> Vec<AvisProfileCompliance>
```

The audit reads only the track-level `av1C` byte 1 (already surfaced as
`AvisMeta::av1_codec_config` and decoded with the same byte-1 helpers
the still-image audit uses — `seq_profile` from the high 3 bits,
`seq_level_idx_0` from the low 5, per av1-isobmff §2.3). One
`AvisProfileCompliance { profile, seq_profile, seq_level_idx_0,
missing_av1c, is_compliant(), missing() }` record is emitted per
declared brand: a file claiming both `MA1B` and `MA1A` produces two
records (Baseline before Advanced); a file claiming neither
short-circuits to an empty vector.

The single-track shape of AVIS (one image-sequence track per file)
means there is no per-item fan-out — the still-image audit's
`(item_id, profile)` Cartesian collapses to `(track, profile)` here.
The same level-31 carve-out applies: AV1 §A.3 "Maximum parameters" is
treated as out-of-range for either profile since both §8.2 and §8.3
bound the level (5.1 / 6.0).

The Baseline check is `seq_profile == 0 && seq_level_idx_0 <= 13`; the
Advanced check is `seq_profile <= 1 && seq_level_idx_0 <= 16` (per AV1
§A.2 a Main-Profile stream is also a valid High-Profile stream, so
`seq_profile == 0` passes the Advanced check too). Diagnostic tokens
are prefixed `avis-` (`avis-track-missing-av1C`,
`avis-track-av1C-truncated`, `avis-seq-profile-out-of-range`,
`avis-seq-level-idx-out-of-range`) so callers folding both audits into
one `missing()` list can tell the carriers apart.

Test delta: +11 unit (`avis::tests::audit_avis_profile_*` covering the
no-brand-claim short-circuit, the Baseline + Advanced boundary values
(5.1 / 6.0), the High-Profile rejection under MA1B, the level-above-5.1
rejection under MA1B, the Main-Profile acceptance under MA1A, the
Professional-Profile rejection under MA1A, the level-31 rejection
under both profiles, the `missing_av1c` versus `av1C-truncated`
distinction, and the dual-brand record-count / ordering shape). +1
integration
(`alpha_video_avis_satisfies_section_8_2_baseline_profile_audit`)
pins the §8.2 audit on the real Netflix `alpha_video.avif` fixture
(which declares `MA1B` in its `compatible-brands` list). Default lib
281 (was 270); standalone lib 266 (was 255); integration 59 + 1
ignored (was 58 + 1).

Followup: the AVIS path's `AvifInfo` (a single-record builder
parallel to the still-image `AvifInfo`) does not yet surface the audit
the way `AvifInfo::avif_profile_compliance` does for items; today the
caller pairs `parse_avis` + `classify_brands` + the new audit
directly. An `inspect_avis` shape that aggregates these signals into
one record is the natural next step alongside the §4.1
alpha-bit-depth sequence-track follow-up still on the table.

### Round 201 — av1-avif §3 AV1 Image Sequence compliance audit

av1-avif v1.2.0 §3 layers three `shall`-level constraints on top of a
MIAF image-sequence track:

1. The track handler `shall` be `'pict'` (`mdia/hdlr/handler_type`).
2. The track `shall` have only one AV1 Sample description entry
   (`stbl/stsd` `entry_count == 1` with `SampleEntry` type `'av01'`).
3. If multiple Sequence Header OBUs are present across the track
   payload, they `shall` be byte-identical.

Round 201 audits these `shall`s at the container layer via
`audit_avis_sequence(meta, file)`, mirroring the existing per-item
audits in [`derived`] (§2.1, §4.1, §6.6.2.1, §7, §8.2/§8.3) but
scoped to AVIS — one record per file (each AVIS carries a single
image-sequence track).

`AvisMeta` grows two fields populated by `parse_avis`:
`handler: Option<BoxType>` (the four-CC at
`mdia/hdlr/handler_type`, per ISO/IEC 14496-12 §8.4.3) and
`sample_description_types: Vec<BoxType>` (the four-CC of every
SampleEntry declared by `stsd`, in declaration order). The audit
walks every sample's payload exactly once, framing OBUs per AV1
§5.3.1 / §5.3.2 (header byte + optional extension byte) plus
§4.10.5 (`leb128()` for `obu_size`), and collects the byte-slice
of every OBU whose `obu_type` decodes to `OBU_SEQUENCE_HEADER`
(value `1`, per AV1 §6.2.1). The SH-identity check then compares
each collected slice against the first one.

The resulting `AvisSequenceCompliance` record carries one bool per
`shall`, four diagnostic fields (`observed_handler`,
`sample_description_count`, `sequence_header_obu_count`,
`samples_out_of_range`), and `is_compliant()` / `missing()`
helpers shaped exactly like the existing audits. The
`samples_out_of_range` counter tracks samples whose declared
`(offset, size)` falls outside `file`; such samples are skipped
from the SH-identity walk rather than flipping a `shall` to
non-compliant (the audit reports cleanly on partial files).

Files declaring an `'av01'`-typed SampleEntry that's the only
entry, with a `'pict'` handler and per-sample SH OBUs that match
byte-for-byte, satisfy every audited `shall`. The
`sample_description_is_av01` field is reported independently of
`single_sample_description` so the audit distinguishes
"right count, wrong type" (e.g. an `hvc1` track masquerading as
AVIS) from "wrong count" (e.g. dual av01/hevc tracks).

Test delta: +18 unit (`avis::tests::audit_avis_sequence_*` covering
the four `shall` failure modes individually + the diverging-SH
case + the vacuous one-SH / zero-SH compliance paths +
out-of-range sample handling + the real `alpha_video.avif`
fixture's pass; plus `walk_sequence_header_obus_*` covering
SH-only extraction, truncated leb / truncated body / has_size=0
edges; plus `sample_description_types_*` round-trip + the missing-
`stsd` empty-vector shape). +1 integration
(`alpha_video_avis_passes_section_3_compliance_audit` pins the
audit on the real Netflix fixture through the
`oxideav_avif::audit_avis_sequence` re-export). Default lib 270
(was 252); standalone lib 255 (was 237); integration 58 + 1
ignored (was 57 + 1).

Followup: this round audits the §3 sample-description shape
permissively — a non-`'av01'` first SampleEntry passes the
single-description check (since the count is 1) but fails the
type check; encoders that ship dual SampleEntries also trip the
count check first. The descriptive-property side of an AVIS track
(`tkhd` colr / pixi / pasp at the SampleEntry level, parallel to
the still-image `iprp` audits) is still container-side TODO — the
existing per-item audits in [`derived`] do not yet have an AVIS
counterpart. An `avis` companion to `audit_alpha_bit_depth` (the
§4.1 sequence shall on matching alpha + master sequence bit
depths) lands naturally once `AvisMeta` grows multi-track support.

### Round 195 — av1-avif §8.2 / §8.3 AVIF profile compliance audit

av1-avif v1.2.0 §8.2 (`MA1B` Baseline) requires every AV1 Image Item to
satisfy AV1 Main Profile (`seq_profile == 0`) at level 5.1 or lower
(`seq_level_idx_0 <= 13`); §8.3 (`MA1A` Advanced) requires AV1 High
Profile (`seq_profile <= 1`) at level 6.0 or lower
(`seq_level_idx_0 <= 16`). Round 195 audits these `shall`s at the
container layer via `audit_avif_profile_compliance(meta, brands)`,
parallel to the existing §2.1 / §4.1 / §6.6.2.1 / §7 audits.

One `AvifProfileCompliance { profile, item_id, seq_profile,
seq_level_idx_0, missing_av1c, is_compliant(), missing() }` record is
emitted per `(AV1 Image Item, declared profile)` pairing — when a file
declares both `MA1B` and `MA1A`, each item emits one record per brand
(Baseline before Advanced). The audit operates entirely on `av1C[1]`,
which packs `seq_profile (3) | seq_level_idx_0 (5)` per av1-isobmff
§2.3 (AV1 §A.3 maps `seq_level_idx_0 == 13` → level 5.1, `16` → level
6.0, `31` → unconstrained "Maximum parameters", deliberately treated
here as out-of-range for both profiles).

Files declaring neither `MA1B` nor `MA1A` skip the audit entirely (the
returned vector is empty) — a file that doesn't claim a profile has
nothing to fail. `AvifInfo::avif_profile_compliance` is populated by
both the single-item and grid `build_info` paths;
`AvifInfo::avif_profile_strict_compliant()` folds every record to a
single boolean (trivially `true` for the empty-vector case, so combine
with `brands.is_baseline_profile || brands.is_advanced_profile` for a
presence + compliance gate).

This round also completes the scrub of pre-existing decorative
attributions to a specific reference AVIF encoder/decoder family across
`cicp.rs`, `meta.rs`, `inspect.rs`, `tests/integration.rs`, and
`tests/fuzz_regressions.rs` — text has been re-anchored to spec-relative
terminology (BT.709/sRGB/BT.601 SDR triple, "reference encoder",
"black-box oracle") so the in-tree wording follows the project's
clean-room conventions for paraphrased docs. The one public API rename
that fell out — `CicpTriple::is_libavif_srgb_default` →
`CicpTriple::is_sdr_srgb_bt601_default` — has no external consumers (no
other crate referenced the helper).

Test delta: +21 unit (`derived::tests::audit_profile_*` +
`decode_av1c_byte1_*` covering the Baseline + Advanced edges, the
Professional / level-31 rejection cases, the missing / truncated
`av1C` cases, the no-brand-claim short-circuit, and the
multi-item / multi-brand record-count semantics). +3 integration
(`monochrome_fixture_satisfies_avif_baseline_profile_audit` and
`bbb_alpha_fixture_avif_profile_audit_per_av01_item` both pin §8.2
compliance on the real Microsoft fixtures; the `red64` fixture pins
§8.3 Advanced compliance). Default lib 252 (was 210); standalone lib
237 (was 195); integration 57 + 1 ignored (was 54 + 1).

Followup: av1-avif §8 also bounds AV1 Image *Sequence* tracks
(`avis`): the per-frame `av1C` lives in `stsd.av01.av1C` rather than
`iprp.ipco`, so a sample-table-level extension to the audit would
cover those when needed. The current walker only inspects single-item
items via `iprp.ipco` — image-sequence coverage is the natural next
step alongside the parallel sample-table follow-up already noted on
the §4.1 alpha-bit-depth audit.

### Round 182 — av1-avif §2.1 Sequence Header OBU count audit

av1-avif v1.2.0 §2.1 mandates that "The AV1 Image Item Data shall have
exactly one Sequence Header OBU." Round 182 audits this `shall` at the
container layer via `audit_sequence_header_obu(meta, file)`, parallel
to the existing §4.1 alpha-bit-depth (round 176), §6.6.2.1 iden, and
§7 grid-derivation audits.

The walker resolves each `'av01'` item's payload via its `iloc` and
parses the OBU framing per AV1 §5.3.1: header byte (with
`obu_type` in bits 6..3 and `obu_has_size_field` in bit 1, per
§5.3.2), the optional one-byte extension header when
`obu_extension_flag == 1` (§5.3.3), and the leb128 `obu_size`
(§4.10.5) when the size field is present. The OBU payload bodies
themselves are skipped — only the type field is consulted, and a
running count of OBUs whose `obu_type` equals
`OBU_SEQUENCE_HEADER == 1` (per AV1 §6.2.1's enumeration) is
returned per item.

The `SequenceHeaderObuAudit` record distinguishes three structural
failure modes from a plain count mismatch: `missing_iloc` when no
location resolves the item's bytes; `truncated_obu` when the framing
walker hits end-of-stream mid-OBU (truncated leb128, or a declared
`obu_size` that runs past the item payload); and `has_size_field_zero`
when an OBU in the chain carries `obu_has_size_field == 0` (AV1 §5.3.1
requires the size field be set when chaining OBUs inside a
container-framed unit, which is exactly the AVIF Image Item Data
case). Two integration tests pin the audit on real fixtures: the
Microsoft `monochrome.avif` (one `'av01'`, exactly one Sequence
Header OBU) and `bbb_alpha_inverted.avif` (two `'av01'` items —
colour primary + alpha auxiliary — each with exactly one SH OBU).

`AvifInfo` exposes `sequence_header_obu_compliance` (the per-item
record vector) plus `sequence_header_obu_strict_compliant()` (folds
every record into a single boolean — trivially `true` for files
without any `'av01'` items, so combine with the file's brand check
for a presence + compliance gate).

### Round 176 — av1-avif §4.1 alpha bit-depth match audit

The §4.1 Auxiliary-Image `shall` "An AV1 Alpha Image Item (respectively
an AV1 Alpha Image Sequence) shall be encoded with the same bit depth
as the associated master AV1 Image Item (respectively AV1 Image
Sequence)" is now audited at the container layer, parallel to the
existing §7 grid-derivation and §6.6.2.1 iden audits. The new
`oxideav_avif::derived::audit_alpha_bit_depth(&Meta)` walker enumerates
every `'auxl'` iref entry whose source carries an `'auxC'` URN starting
with the alpha prefix (`urn:mpeg:mpegB:cicp:systems:auxiliary:alpha`),
then emits one
`AlphaBitDepthAudit { alpha_item_id, master_item_id, alpha_bit_depth,
master_bit_depth, alpha_missing_av1c, master_missing_av1c,
is_compliant(), missing() }` record per `(alpha, master)` pairing
declared in the iref's `to_ids`, in iref declaration order. A single
alpha attached to multiple masters emits one record per master.

Bit depth is decoded directly from each item's `av1C` flag byte (`8`,
`10`, or `12`) without re-parsing the full configuration record. The
audit also surfaces two §2.1 violations that would defeat the §4.1
check itself: missing `av1C` (`{alpha,master}_missing_av1c`) and
present-but-truncated `av1C` (decoded depth is `None` with the missing
flag still false). `missing()` enumerates failed `shall`s as static
strings (`alpha-master-bit-depth-mismatch`,
`alpha-item-missing-av1C`, `alpha-item-av1C-truncated`,
`master-item-missing-av1C`, `master-item-av1C-truncated`).

`AvifInfo::alpha_bit_depth_compliance: Vec<AlphaBitDepthAudit>` is
populated by both the single-item and grid `build_info` paths (an
alpha-free file trivially returns an empty vector).
`AvifInfo::alpha_bit_depth_strict_compliant()` folds every record to
a single boolean (vacuously `true` when no alpha auxiliaries exist;
combine with `has_alpha` for a presence + compliance gate).

Test delta: +10 unit (`derived::tests::audit_alpha_bit_depth_*` +
`decode_av1c_bit_depth_*`) covering: matching 8-bit pairing compliant;
10/8 bit mismatch flagged with the right `missing()` token; 12/12 also
compliant; alpha missing `av1C`; master missing `av1C`; truncated
`av1C` distinguishes from missing; depth-map auxiliary (non-alpha URN)
ignored; one alpha pointing at multiple masters emits one record per
pairing; empty-when-no-alpha vacuum; multiple distinct alpha
auxiliaries in one file each emit their own record. +2 integration
(`monochrome_fixture_has_no_alpha_bit_depth_audit_records` pins the
no-alpha vacuum on the Microsoft monochrome fixture;
`bbb_alpha_fixture_alpha_master_bit_depth_match` pins the §4.1
compliant shape end-to-end on the real `bbb_alpha_inverted.avif`
fixture, confirming both alpha and master carry an `av1C` and agree
on bit depth). Default lib 210 (was 198); standalone lib 195 (was
183); integration 52 + 1 ignored (was 50 + 1).

Followup: §4.1's parallel `shall` on AV1 Alpha Image *Sequences*
(matching bit depth between alpha and master sequences) needs a
sample-table-level audit on `avis` — the per-frame `av1C` lives in
`stsd.av01.av1C` rather than `iprp.ipco`. The current walker only
covers single-item files; an `avis` extension would consume
`avis::AvisMeta::av1_codec_config` for the alpha track once the AVIS
walker grows multi-track support.

### Round 172 — av1-avif §7 grid-derivation transformative-property audit

The §7 General-constraints `shall` "Transformative properties shall not be
associated with items in a derivation chain that serves as an input to a
grid derived image item" is now audited at the container layer. The new
`oxideav_avif::derived::audit_grid_derivations(&Meta)` walker returns one
`GridDerivationAudit { grid_item_id, tile_item_ids, offenders,
is_compliant(), offending_tile_ids() }` record per `'grid'` item in
`iinf` declaration order. Each record lists the offending
`(tile_item_id, property_kind)` pairs found by walking the grid's
`'dimg'` iref and inspecting every tile for an associated `'clap'` /
`'irot'` / `'imir'` property. Transformative properties on the grid
item *itself* are explicitly permitted by §7 and don't surface as
offenders.

`AvifInfo::grid_derivation_compliance: Vec<GridDerivationAudit>` is
populated by both the single-item and grid `build_info` paths (the
audit fires on every file — single-item files trivially return an empty
vector since they have no grid items). `AvifInfo::
grid_derivations_strict_compliant()` returns the AND of every record
(trivially `true` when no grid items exist; combine with `is_grid` for
a presence + compliance gate).

Test delta: +7 unit (`derived::tests::audit_grid_derivations_*`
covering: clean chain with grid-level `irot` permitted; single-tile
`irot` flagged; tile with all three kinds emits three offender entries
in stable `(clap, irot, imir)` order; multiple offending tiles across a
chain; empty-when-no-grid-items vacuum; multi-grid file produces one
record per grid; grid without a `dimg` iref returns a vacuously
compliant record). +2 integration (`synthetic_4x1_strip_passes_grid_
derivation_audit` pins the 4-tile-clean shape end-to-end through
`inspect`; `monochrome_fixture_has_no_grid_derivation_audit_records`
pins the no-grid-item shape on the real Microsoft fixture). Default
lib 189 (was 182); standalone lib 174 (was 167); integration 48 + 1
ignored (was 46 + 1).

Followup: HEIF defines additional transformative properties (`'iscl'`
image scaling, `'rref'` required reference) the audit doesn't yet
flag — these would land in `Property::Other` today and a tile carrying
one essential would already trip
`Meta::unsupported_essential_properties`, but a focused §7 audit
extension would surface them by kind once those property types are
parsed here.

### Round 130 — Tone Map (`tmap`) av1-avif §4.2.2 compliance audit

The HEIF-defined `'tmap'` descriptor body parse stays deferred (the
only HEIF edition currently shipped in `docs/image/heif/` is the 2017
first edition, which predates `tmap`). What av1-avif §4.2.2 *does*
normatively require independently of the descriptor body is two
file-shape `should` constraints, and this round audits both:

1. The `tmap` item and its base image item (input `0` of the tmap's
   `'dimg'` iref) should be grouped together by an `'altr'` entity
   group, so legacy readers that don't understand `tmap` still pick a
   valid alternate.
2. Each gain-map input image item (`to_ids[1..]` of the same iref) of
   the tmap should be a HEIF [hidden image item][HEIF §6.4.2]: the
   `infe` FullBox `flags` low bit (`(flags & 0x01) == 1`) is set so a
   legacy reader never surfaces the gain map as a primary picture.

The new `oxideav_avif::derived::audit_tone_map(&Meta)` walker returns
one `ToneMapCompliance { tmap_item_id, base_item_id,
gain_map_item_ids, paired_in_altr, gain_maps_hidden,
is_compliant(), missing() }` record per `'tmap'` item in `iinf`
order. `AvifInfo::tone_map_compliance: Vec<ToneMapCompliance>` is
populated by both the single-item and grid `build_info` paths;
`AvifInfo::tone_map_strict_compliant()` returns the AND of every
audit record (trivially `true` for files without any tmap items, so
combine with `has_tone_map()` for a presence + compliance gate).

`ItemInfo` now retains the 24-bit `infe` FullBox `flags` field
(previously dropped on the floor) so the hidden-image-item check
above can run without re-walking the box. `ItemInfo::is_hidden()`
exposes the low-bit semantics (`(flags & 0x01) == 0x01`) and ignores
the upper reserved bits.

Test delta: +8 unit (`derived::tests::audit_tone_map_*` covering the
two-`should` happy path with and without a gain map, both-fail surface
with no `grpl` plus a visible gain map, `altr` group missing the tmap
id, tmap with no `dimg` iref, empty audit list when no tmap items
present, multiple tmap items returned in `iinf` order, plus an
`ItemInfo::is_hidden` low-bit-semantics sweep across the 24-bit flag
space). Default lib 182 (was 174); standalone lib 167 (was 159);
integration 46 + 1 ignored unchanged.

Followup: an HEIF edition (2022 / 3rd edition or later) shipped in
`docs/image/heif/` would unblock the `tmap` descriptor body parse +
the full tone-map composition path.

### Round 127 — Sample Transform Derived Image Item (`sato`) parser + evaluator

The av1-avif v1.2.0 §4.2.3 Sample Transform derived-image carrier is
now fully decoded at the container layer. `oxideav_avif::derived::
SampleTransform::parse(payload, reference_count)` decodes a `sato`
descriptor's postfix-notation expression into a typed
`Vec<Token>` (Token = `Constant(i64) | Sample(u8) | Unary(u8) |
Binary(u8) | Reserved(u8)`), checking every spec assertion before
returning:

* Header layout: `version:2 | reserved:4 | bit_depth:2` then a
  `token_count: u8` then `token_count` tokens. `version` must be 0;
  `token_count` must be ≥ 1; `bit_depth` selects an 8/16/32/64-bit
  intermediate precision per Table 1 and, for `Constant` tokens, the
  byte width of the inline literal.
* Operator table: unary `negation` / `abs` / `not` / `bsr` (Table 2
  rows 64..=67); binary `sum` / `difference` / `product` / `quotient` /
  `and` / `or` / `xor` / `pow` / `min` / `max` (rows 128..=137).
  `Sample(n)` tokens (`1..=32`) are 1-based input-image indices into
  the parallel `dimg` iref's `to_ids`; the parser rejects any value
  exceeding the `reference_count` argument per §4.2.3.4.
* Reserved-token rejection: values in `33..=63`, `68..=127`, and
  `138..=255` cause `parse` to error. A `parse_relaxed` counterpart
  surfaces them as `Token::Reserved(raw)` for diagnostic dumps.
* Stack discipline: parse-time validation enforces every constraint
  from §4.2.3.4 — unary tokens require ≥ 1 element on the stack,
  binary tokens require ≥ 2, the final expression must leave exactly
  one element on the stack. Underflowing expressions are rejected
  before the descriptor is handed back.

`SampleTransform::evaluate(&inputs)` walks the parsed expression to
produce one output sample value. Intermediate arithmetic saturates at
`i64` then clamps to the `num_bits` precision per the §4.2.3.3
underflow / overflow rule (replaced by `-2^(num_bits-1)` /
`2^(num_bits-1)-1`); the caller is responsible for the final clamp
into the reconstructed item's `PixelInformationProperty` bit depth.
Composition of an actual `sato` reconstructed image is deferred
until `oxideav-av1` ships a decoder again — for now the descriptor
parse path is enough to validate files and reason about their layout.

Item-type enumeration: `meta::ITEM_TYPE_SATO` + `meta::ITEM_TYPE_TMAP`
constants land alongside `IOVL` / `IDEN`, and the new
`Meta::item_ids_of_type(&four_cc)` walker enumerates derived-image
carriers by type. `AvifInfo::sato_item_ids` / `tmap_item_ids` are
populated by both the single-item and grid `build_info` paths;
`AvifInfo::has_sample_transform()` / `has_tone_map()` predicates give
callers a one-call presence gate. The Tone Map carrier currently
parses only the item-type four-CC; the HEIF-defined `tmap` descriptor
body is a follow-up.

Test delta: +21 unit (`derived::tests::sato_*` — bit-depth coverage,
operator semantics, evaluation of the Appendix A MSB/residual
recombination example, reserved-token rejection per range, stack
discipline, truncated payloads, classification helpers) and +2
integration (synthetic AVIF with `av01` primary + `sato` derived
item linked by `dimg`, plus a negative on the Microsoft monochrome
fixture). Standalone lib 159 (was 138); default lib 174 (was 153);
integration 46 + 1 ignored (was 44 + 1).

### Round 123 — AV1 layered-image properties + essential-property enforcement

Two AV1-specific descriptive item properties that previously fell into
`Property::Other` are now parsed and typed (av1-avif §2.3.2). Pure
container box work — no AV1 decode dependency.

* **`a1op` — OperatingPointSelectorProperty** (av1-avif §2.3.2.1). A bare
  `ItemProperty` carrying a single `unsigned int(8) op_index` that
  selects which AV1 operating point a scalable item should decode. New
  `meta::A1op { op_index }`; the spec mandates the property be marked
  essential.
* **`a1lx` — AV1LayeredImageIndexingProperty** (av1-avif §2.3.2.3). A
  `reserved(7) + large_size(1)` byte then three
  `(large_size+1)*16`-bit `layer_size` values documenting the byte size
  of every layer except the last. New
  `meta::A1lx { large_size, layer_size: [u32; 3] }` with
  `documented_layers()` (leading non-zero run = layer count − 1). Both
  16-bit and 32-bit field widths handled; the spec forbids marking this
  property essential.
* Both surfaced on `AvifInfo` as `operating_point: Option<A1op>` and
  `layered_index: Option<A1lx>`, resolved on the primary item for the
  single-item and grid paths alike.
* **Essential-property enforcement** (av1-avif §2.3.2.1.2 + MIAF §7.3.5).
  The `ipma` essential bit was parsed but inert. `Meta::{
  unsupported_essential_properties, has_unsupported_essential_property}`
  now report any property flagged essential that this crate cannot
  interpret (lands in `Property::Other`, or whose association index
  dangles). A reader must not process such an item. Recognised
  properties (typed, even if only ignored) and non-essential unknown
  properties do not block.

Test delta: +8 unit (`a1op`/`a1lx` round-trips at both field widths,
`ipco` dispatch, three essential-enforcement cases) + 3 integration
(synthetic `a1op`/`a1lx`-bearing AVIF through `inspect`, the negative
no-props path, and an essential-but-recognised `a1op` not blocking).
Standalone lib 138 (was 131); default lib 153; integration 44 + 1
ignored (was 41 + 1).

### Round 81 — derived-image + entity-grouping + MIAF compliance

Container side gains a coordinated batch of HEIF surface that doesn't
need the AV1 decoder (which after the 2026-05-20 `oxideav-av1`
clean-room rebuild is a `NotImplemented` scaffold). The decoder path
in this crate keeps its public API and now reports a clean
`Unsupported` at the AV1 hand-off, the same shape callers would have
seen from `oxideav-av1::NotImplemented`. Parse, `inspect`, and the
new validators below all work end-to-end.

* **`auxC` URN classification** — HEIF §6.5.8. `AuxKind` enum maps
  the URN string to `Alpha` / `DepthMap` / `HdrGainMap` / `Other`,
  covering both the MPEG spelling (`urn:mpeg:mpegB:cicp:systems:auxiliary:*`)
  and the HEVC-HEIF spelling (`urn:mpeg:hevc:2015:auxid:1` / `:2`)
  plus Apple's HDR gain-map URN
  (`urn:com:apple:photo:2020:aux:hdrgainmap`). `Meta::aux_items_for`
  enumerates every auxiliary linked to a primary via `auxl`, paired
  with its kind; `AvifInfo` surfaces `aux_items`, `alpha_aux_kind`,
  `depth_map_item_id`, `hdr_gain_map_item_id` for one-call routing.
* **`rloc` relative-location property** (HEIF §6.5.7) —
  `Rloc { horizontal_offset, vertical_offset }` parsed alongside the
  other descriptive item properties and surfaced through the
  existing `property_for` dispatch.
* **`lsel` layer-selector property** (HEIF §6.5.11) —
  `Lsel { layer_id }` for multi-layer image items. ItemProperty
  (no FullBox), one u16.
* **`iovl` image-overlay descriptor** (HEIF §6.6.2.2) — the new
  `derived::ImageOverlay::parse(payload, reference_count)` decodes
  the per-derivation header plus `(horizontal_offset, vertical_offset)`
  per source image. Handles both the 16-bit and 32-bit
  (`flags & 1 == 1`) field-length variants and signed offsets per
  spec. `dimg` iref enumeration on the caller side supplies the
  source IDs in layering order (bottom-most first). This is HEIF
  surface that an AVIF reader may encounter on `mif1`/MIAF inputs.
* **Entity groups (`grpl`)** (HEIF §9.4) — `derived::parse_grpl` walks
  a GroupsListBox payload into typed `EntityGroup { grouping_type,
  group_id, entity_ids }`. Helpers `is_alternates()` /
  `is_stereo_pair()` / `is_equivalence()` cover the common `altr` /
  `ster` / `eqiv` groupings. `Meta` captures the raw `grpl` slice
  during the meta walk and `Meta::groups()` lazy-parses on demand.
* **`mif1` compliance audit** (HEIF §10.2.1.1) —
  `parser::audit_mif1` walks `ftyp` + `meta` once and reports per-box
  presence (`hdlr` / `pitm` / `iinf` + at least one `infe` / `iloc` /
  `iprp`) plus the `mif1` brand claim, returning a
  `Mif1Compliance { is_compliant(), missing() }`. Pinned against the
  Microsoft `monochrome.avif` fixture (fully compliant) plus a
  ftyp-only synthetic that exercises the missing-box path.
  `AvifInfo` carries the audit result alongside `is_strict_mif1()`.
* **`Meta` exposes raw `grpl` + `idat` slices** so callers can route
  entity-grouping and item-data-bearing derived items without
  rewalking the meta box.
Test delta: +13 lib unit tests on the standalone (no-default)
surface (131 lib, was 118; +10 more under the default `registry`
feature for the existing `validate_av1_config_*` tests, totalling
141 against `oxideav-av1` 0.1.8 from crates.io). Integration suite
unchanged (41 + 1 ignored).

Workspace caveat: the AV1 calls in this commit target the published
`oxideav-av1` 0.1.8 API (the one this crate's CI pulls from
crates.io). The umbrella workspace's `[patch.crates-io]` table
currently redirects to the orphan-rebuilt `oxideav-av1` master,
which is a `NotImplemented` scaffold without `Av1CodecConfig` /
`Av1Decoder` — so workspace-level builds will fail the registry
feature until the av1 clean-room ships its decoder. The integration
drive helper already graceful-skips both the old `coded_lossless` /
`§7.7.4` shape and any future "decoder unavailable" string so a
real decoder publishing in either direction doesn't tip tests red.

### Round 75 — HEIF item-properties + iref typed-relationships

Container side reaches further into the descriptive surface that an AVIF
file carries around its primary AV1 OBU stream. None of this requires
the AV1 decoder.

* **`infe` v2/v3 tail fields** (`ItemInfo.content_type` /
  `content_encoding` / `item_uri_type`). ISO/IEC 14496-12 §8.11.6.2:
  for `item_type == 'mime'` the entry ships `content_type` (MIME)
  then an optional `content_encoding`; for `item_type == 'uri '` it
  ships an absolute URI. Previously the parser stopped at
  `item_name` and discarded the tail, leaving callers unable to tell
  an XMP item from an Exif `octet-stream` item. The three new
  optional fields are populated only when relevant; every other
  item_type leaves them `None` so the common path stays compact.
* **`thmb` / `cdsc` / `prem` iref enumeration** (`Meta`
  `iref_sources_of` plus `is_alpha_premultiplied_for`). ISO/IEC
  14496-12 §8.11.12 / HEIF §6.10.1.1. The existing
  `iref_source_of` only returned the first match; `iref_sources_of`
  walks every entry, which a primary item with multiple thumbnails
  needs.
* **`AvifInfo` carries descriptive-metadata pointers**:
  `thumbnail_item_ids: Vec<u32>`, `exif_item_id: Option<u32>`,
  `xmp_item_id: Option<u32>`, `premultiplied_alpha: bool`.
  Helpers: `has_thumbnails()`, `has_descriptive_metadata()`. The
  Exif detector accepts the native `'Exif'` item type AND the
  generic `mime` carrier with
  `application/octet-stream` / `image/tiff` / `image/x-exif`
  content_type. XMP is detected via
  `mime` + `application/rdf+xml` (case-insensitive — encoders
  disagree on capitalisation).
* **`item_payload_bytes(file, item_id) -> Result<Vec<u8>>`**: a
  thin public wrapper around the existing `item_bytes_owned`
  resolver so callers with a populated `AvifInfo` can extract the
  Exif TIFF or XMP RDF/XML payload in one call.
* **Real-fixture validation**: the Microsoft `monochrome.avif`
  conformance fixture ships a native Exif item (id 2) linked via
  `cdsc`. The new `inspect_fixture_resolves_native_exif_metadata_item`
  test pins the end-to-end resolution path on real bytes plus
  verifies HEIF §A.2.1's 4-byte `exif_tiff_header_offset` prefix
  → TIFF byte-order marker (`II` / `MM`).

Test delta: +14 unit tests in `meta` / `inspect` (118 lib, was 104),
integration suite unchanged (41 + 1 ignored).

### Round 47 — fuzz-driven AVIF→AV1 boundary hardening

Daily cargo-fuzz workflow surfaced an arithmetic-overflow class of
crashes in `oxideav-av1`'s coefficient decoder when fed adversarial
AV1 OBU streams wrapped in AVIF containers. The AV1 fix itself is a
sibling-crate concern; this round adds the AVIF-side defensive
layer so the host stops handing garbage to the AV1 entropy stage.

* **`validate_av1_config`** at the AVIF→AV1 handoff refuses `av1C`
  records whose fields violate AV1 spec invariants:
  `seq_profile > 2` (§A.4), reserved `seq_level_idx_0` in 24..=30
  (§A.3), `monochrome = 1` without both chroma-subsampling bits
  (§5.5.2), 4:2:2 outside `seq_profile = 2` (§5.5.2), 4:4:4 in
  `seq_profile = 0` (§5.5.2). Both the still-image path
  (`decode_av01_item`) and the AVIS sequence path
  (`decode_avis_file`) call into the validator before forwarding
  any OBU bytes.
* **32 MiB soft cap** on the AV1 OBU payload size at the AVIF→AV1
  handoff. Real-world AVIF items stay well under this; the cap
  protects against pathological inputs that dominate the fuzz wall
  clock without surfacing useful crashes.
* **AVIS `samples_per_chunk` DoS guard**: `sample_table` enforces
  a 16 Mi total-samples soft cap. Without it, an `stsc` entry
  with `samples_per_chunk = 0xFFFF_FFFF` spun the per-chunk
  expansion loop for hours.
* **Defensive box-walker arithmetic**: `parse_box_header` +
  `read_u16/u32/u64` now use `checked_add` for every offset
  computation and refuse `usize::MAX`-adjacent positions instead of
  debug-panicking on the `start + 8 > buf.len()` shape.
* **Regression suite** in `tests/fuzz_regressions.rs` anchored on
  three real AVIF bitstreams captured from the daily fuzz workflow.
  Asserts decode does not panic — pixel correctness remains the
  cross-decode harness's responsibility (the residual Y-plane
  divergence is tracked as a sibling follow-up in `oxideav-av1`).

The remaining fuzz-discovered divergence (Y-plane pixels diverging
between `oxideav-avif` and an external AVIF decoder used as a black-box
oracle on the same AV1 bitstream) is a sibling-crate issue: the AVIF
container layer hands identical OBU bytes to both decoders, so any
divergence is an `oxideav-av1` decode-path bug, not an AVIF wrap issue.

### Round 22 — HDR metadata + AV1 wrap pass-through + multi-extent items

Three headroom items addressed for the 60% → 75% coverage push:

* **HDR metadata (`mdcv` / `clli` / `cclv`)**: All three HDR-signalling
  item property boxes are now parsed and surfaced through `AvifInfo`.
  `mdcv` (MasteringDisplayColourVolumeBox — SMPTE ST 2086) carries
  display primaries + white point + max/min luminance; `clli`
  (ContentLightLevelBox — ISO/IEC 14496-12 §12.1.5.4) carries MaxCLL +
  MaxFALL in cd/m²; `cclv` is a draft av1-avif extension with the same
  layout as `clli`. Grid primaries follow the same fallback chain as
  `colr`/`pixi`/`pasp` (grid item first, tile 0 second). Helper methods
  `has_hdr_metadata()`, `max_cll()`, and `max_fall()` provide convenient
  gates for downstream consumers.

* **AV1 wrap pass-through** (`bit_depth`, `monochrome`,
  `chroma_subsampling`): The `av1C` record's three key subsampling/depth
  fields are now decoded inline in `decode_av1c_flags()` and stored
  directly on `AvifInfo` — callers no longer need to re-parse the record.
  `bit_depth` maps `(high_bitdepth, twelve_bit)` → `(8, 10, 12)`;
  `monochrome` mirrors the `av1C` mono bit; `chroma_subsampling` carries
  `(subsampling_x, subsampling_y)` or `None` for monochrome streams
  (subsampling is undefined for 4:0:0).

* **Multi-extent `iloc` items** (`item_bytes_owned`): A new public
  helper `item_bytes_owned(file, loc) -> Result<Vec<u8>>` concatenates
  all extents when an item spans more than one `iloc` extent entry (HEIF
  §8.11.3.3). The existing `item_bytes` fast path is preserved for the
  common single-extent case; only the new helper allocates. The old
  `Unsupported` error from `item_bytes` on multi-extent items remains
  so callers can decide when to use the slower path.

### Round 21 — grid hardening

Two correctness fixes for multi-tile MIAF AVIFs (HEIF §6.6.2.3 +
av1-avif §4.2.1):

* **Tile-edge chroma alignment**: `composite_grid` now uses ceiling
  division of the trimmed luma copy extent when computing chroma copy
  width / height, so a 4:2:0 grid whose right-most or bottom-most
  tile is clipped to an odd luma column / row no longer drops the
  trailing chroma sample. Previously a `tile_w=4`, `output_w=7`
  grid lost the right-most chroma column. Same fix covers 4:2:2
  horizontal-only chroma. Source-side and destination-side clamps
  guard against tiles whose chroma plane is smaller than the
  luma-derived ceiling, and against tiles that overhang the canvas.
* **Grid `colr` / `pixi` / `pasp` resolution**: every descriptive
  property now follows the canonical HEIF chain — grid item first
  (describes the reconstructed canvas), tile-0 second (a real-world
  HEIF writer pattern; av1-avif §4.2 keeps per-tile values uniform).
  Previously only `colr` had the fallback wiring; `pixi` looked
  only at tile 0 and `pasp` only at the grid item, so two real
  encoder placement patterns went unread.

`AvifInfo::effective_cicp()` consequently returns the same triple for
a grid whether `colr` lives on the grid item, on each tile, or on
both (av1-avif §4.2.1 — derived items inherit the colour signalling
of their inputs). When the grid and its tiles all lack `colr`, the
triple folds to `Unspecified` per ITU-T H.273.

### Round 20 — CICP color path

Per av1-avif §4.2.3.1 ("No color space conversion, matrix coefficients,
or transfer characteristics function shall be applied to the input
samples"), AVIF readers do **not** transform decoded pixels. The CICP
triple is signalling: it tells downstream consumers which colour space
the samples occupy. The crate now exposes a resolved
`(primaries, transfer, matrix, full_range)` quadruple via
`AvifInfo::effective_cicp() -> CicpTriple`:

* `Some(Colr::Nclx { .. })` → fields surfaced verbatim.
* `Some(Colr::Icc(_))` → ICC bytes are authoritative; CICP folds to
  the spec-mandated `Unspecified (2, 2, 2, false)`.
* `None` (no `colr` property) → `Unspecified (2, 2, 2, false)`.

`CicpTriple` ships predicates for the common decision points
(`is_unspecified`, `is_identity_matrix` for matrix=0 RGB AVIFs,
`is_sdr_srgb_bt601_default` for the `(1, 13, 6)` SDR-sRGB triple,
`has_reserved` flagging any axis in an ITU-T H.273 reserved range)
plus three name lookup helpers (`primaries_name`, `transfer_name`,
`matrix_name`) covering BT.709, BT.2020, Display P3, sRGB, PQ, HLG,
identity matrix, BT.601, BT.2020 NCL, ICtCp.

For alpha auxiliary items, av1-avif §4.1 mandates `color_range = 1`
and instructs readers to ignore any attached `colr`. The crate
exposes that as `CicpTriple::ALPHA` / `CicpTriple::for_alpha()`.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-codec = "0.1"
oxideav-av1 = "0.0"
oxideav-avif = "0.0"
```

## Use

### Inspect an AVIF file without decoding

```rust
use oxideav_avif::inspect;

let bytes = std::fs::read("image.avif")?;
let info = inspect(&bytes)?;
println!("{}x{} bits_per_channel={:?} av1c_len={}",
    info.width, info.height, info.bits_per_channel, info.av1c.len());
println!("primary OBU stream is {} bytes", info.obu_bytes.len());
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Register with a `CodecRegistry`

```rust
use oxideav_codec::CodecRegistry;
use oxideav_core::{CodecId, CodecParameters, Error, Packet, TimeBase};

let mut reg = CodecRegistry::new();
// `register_with_av1` installs both the AVIF entry and the AV1 decoder
// factory in one call — AVIF delegates to oxideav-av1 internally.
oxideav_avif::register_with_av1(&mut reg);

let params = CodecParameters::video(CodecId::new(oxideav_avif::CODEC_ID_STR));
let mut dec = reg.make_decoder(&params)?;

let bytes = std::fs::read("image.avif")?;
let pkt = Packet::new(0, TimeBase::new(1, 1), bytes);
dec.send_packet(&pkt)?;

match dec.receive_frame() {
    Ok(frame) => {
        // Flat / simple inputs decode cleanly.
        eprintln!("decoded frame: {frame:?}");
    }
    Err(Error::Unsupported(msg)) => {
        // Rich content still hits oxideav-av1 gaps — the message
        // names which AV1 feature is unsupported.
        eprintln!("av1 unsupported: {msg}");
    }
    Err(other) => return Err(other.into()),
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Low-level: parse the container yourself

```rust
use oxideav_avif::parse;

let bytes = std::fs::read("image.avif")?;
let img = parse(&bytes)?;
for item in &img.meta.items {
    println!("item {} type={:?} name={:?}", item.id,
        std::str::from_utf8(&item.item_type), item.name);
}
// `img.primary_item_data` is the slice inside `mdat` holding the
// primary image's AV1 OBU stream. Pair it with `img.av1c` when
// building a CodecParameters for oxideav-av1.
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Codec id

- Codec: `"avif"`; capability name declared to the registry is
  `avif_heif_av1_decode` — end-to-end pipeline: parsed container + AV1
  frame decode + grid / alpha / transform composition.
- `CodecParameters::extradata` is the `av1C` byte record; width /
  height reflect `ispe` from the primary item.

## Test fixtures

`tests/fixtures/monochrome.avif` is `Monochrome.avif` from
[AOMediaCodec/av1-avif](https://github.com/AOMediaCodec/av1-avif/tree/main/testFiles/Microsoft)
— 1280×720, monochrome, single 8-bit plane. Integration tests walk
its complete HEIF hierarchy, extract the primary item, and decode it
end-to-end through `oxideav-av1`.

`tests/fixtures/{gray32,midgray,white16,red,black420}.avif` are tiny
(16×16 … 64×64) AVIFs produced by an off-the-shelf reference encoder
in lossless mode (monochrome + 4:4:4) or q60 (4:2:0). They exist so the
CI decode-gate covers every colour-plane layout we support without
depending on an AV1 implementation that decodes rich photos perfectly.

## License

MIT — see [LICENSE](LICENSE).
