# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

[Unreleased]: nothing yet.

## [0.2.2] - 2026-08-22

[0.2.2]: nothing yet.

### Added

- **SMask-coupled Flate-base downsampling (Phase 5 D-M3).** Over-resolution
  FlateDecode images with an eligible `/SMask` are now downsampled with the
  same atomic pair mechanics as D-M2: the base goes through the existing
  format-preserving Flate→Flate path (same `/ColorSpace`, predictor handling
  unchanged), the mask through the same resample-to-Flate path, both to the
  SAME target geometry — replaced together or not at all. The combined
  never-larger/5% minimum-savings guard, the shared-mask fail-safe, and every
  D-M1 skip rule (`/Matte`, stencils, ineligible mask shapes, corrupt
  streams) apply unchanged, and the pair honors `downsample_flate_images`.
  Pairs already at/below the DPI threshold are left byte-for-byte untouched —
  there is no requantization analogue for lossless Flate payloads (that would
  be a lossy re-encode, which still has no consent surface). On the 16.8 MB
  NASA reference report this takes amatl from 6.03 MB (D-M2) to 5.55 MB
  (5,547,684 B) vs Ghostscript's 3.72 MB, byte-stable across passes.

- **SMask-coupled downsampling (Phase 5 D-M2).** Over-resolution JPEG images
  with an eligible `/SMask` are now downsampled as an atomic pair: base
  (Lanczos3 → JPEG at `jpeg_quality`) and mask (Triangle → plain FlateDecode
  8-bit DeviceGray) are resampled to the SAME target geometry and replaced
  together — never one side alone. The combined candidate must beat the pair's
  original size by the full 5% minimum, and both halves pass decode-back
  verification (exact for the lossless mask). A mask referenced by more than
  one image is never resized (fail-safe: dedup merges byte-identical masks
  before planning, so sharing is reachable in practice), and all D-M1 skip
  rules (`/Matte`, stencils, ineligible mask shapes, corrupt streams) carry
  over. On the 16.8 MB NASA reference report this takes amatl from 11.51 MB
  (D-M1) to 6.03 MB vs Ghostscript's 3.72 MB, byte-stable across passes.

- **SMask-aware JPEG requantization (Phase 5 D-M1).** JPEG (`DCTDecode`)
  images carrying an `/SMask` soft mask are no longer unconditionally skipped:
  when the mask resolves to a plain 8-bit DeviceGray image stream with no
  `/Matte` (and is not an `/ImageMask` stencil), the base JPEG is decoded at
  its OWN dimensions and re-encoded at `OptimizeOptions::jpeg_quality` —
  never resized, so mask alignment is untouched by construction, and the
  `/SMask` stream itself is never modified. Every hard rule from the Phase 5
  contract is enforced: strict smaller-only replacement (never-larger guard),
  decode-back pixel verification of the re-encoded base before replacement,
  `/Mask` color-key/stencil images and all ineligible `/SMask` shapes
  (unresolvable reference, non-image object, non-DeviceGray color space,
  non-8-bit samples, `/Matte` anywhere in the pair, FlateDecode bases — those
  are covered by the D-M3 entry above) are left byte-for-byte untouched, and the panic-safe
  `catch_unwind` boundary is unchanged. Attacks the 9.16 MB masked-JPEG class
  on the reference corpus at zero geometric risk.

- **`amatl` CLI.** The crate now ships a command-line binary (`src/main.rs`)
  over the same library API: positional input, `-o/--output` (default
  `<input>.optimized.pdf`, never overwriting without `--force`), and flags
  mapping 1:1 to every `OptimizeOptions` builder
  (`--target-dpi`, `--jpeg-quality`, `--dpi-margin`, and `--flag` /
  `--no-flag` pairs for all five boolean options). All defaults — including
  the boolean on/off states printed under `--help` — are read from
  `OptimizeOptions::default()` at runtime so CLI and library cannot drift.
  Non-PDF input is rejected by `%PDF-` header sniffing; success reports
  input → output bytes, percent saved, and elapsed time; failures exit
  nonzero with a clean message (no panics on user-facing paths).

## [0.2.1] - 2026-08-22

[0.2.1]: nothing yet.

### Added

- **Opt-in lossless bitonal→G4 recompression (Phase 3 B-M1).**
  `OptimizeOptions::recompress_bitonal_images` (default `false`) and the
  `with_recompress_bitonal_images` builder losslessly re-encode 1-bit images
  to CCITT G4 via the `fax` 0.3.0 crate (vetted in docs/PHASE3-PLAN.md §B.3:
  MIT, `#![deny(unsafe_code)]`, zero transitive dependencies — promoted from
  dev-dependency to runtime dependency). Two source shapes: CCITT-stored
  (`/K -1` G4, and EOL-framed `/K 0` G3 1D with `/EndOfLine true`) and
  Flate-stored 1-bit images, including `/ImageMask` streams. Pixels are never
  resampled; `/BlackIs1` polarity is normalized at the sample level; a stream
  is replaced only when the G4 payload is strictly smaller (including
  `/DecodeParms` overhead) **and** an in-process decode-back pass reproduces
  the source samples bit-for-bit. All parameter doubt is a fail-safe skip:
  `/EncodedByteAlign true`, `/K > 0`, EOL-less `/K 0`, `/EndOfBlock false`,
  non-identity `/Decode`, geometry mismatches, and damaged streams (strict
  row-count + EOFB/RTC accounting — a short stream is never re-encoded as
  white-padded data).

- **Opt-in font subsetting (Phase 3 C-M1).**
  `OptimizeOptions::subset_fonts` (default `false`) and the
  `with_subset_fonts` builder subset embedded Type0/CIDFontType2
  (Identity-H/V) fonts to the glyphs the document actually shows, via
  `subsetter` 0.2.6 (`default-features = false`; one transitive dependency).
  The `/CIDToGIDMap`-stream technique replaces only `/FontFile2` and
  `/CIDToGIDMap` (plus a deterministic `TAG+` name): content-stream text
  bytes are **never rewritten**, and `/W`, `/DW`, and `/ToUnicode` stay
  untouched, so text extraction is bit-identical pre/post (pinned by tests
  and a Ghostscript/pdftotext harness in `scripts/verify-fonts.sh`).
- Resource-aware glyph discovery over all content-bearing stream classes:
  pages, form XObjects (recursive, cycle-guarded), annotation appearance
  streams, tiling patterns, and Type3 char procs. Any parse failure anywhere
  disables subsetting for the whole document; any per-font doubt (shared
  descendant/descriptor/font program, non-Identity encoding, CFF/CFF2,
  unresolvable map, glyphs out of range, not net-smaller) leaves that font
  untouched. PDF/A-declared (`pdfaid` XMP) and encrypted documents are
  skipped entirely.
- OFL-licensed fixture font (`fixtures/fonts/NotoSans-Regular.ttf` +
  `fixtures/fonts/OFL.txt`) powering the verification battery: per-glyph
  outline and advance equality via `ttf-parser` (dev-dependency only),
  composite-glyph closure, CIDToGIDMap-stream inputs, multi-page
  accumulation, and walker coverage of forms/appearances.

### Fixed

- **`optimize(optimize(x))` is now byte-stable on real-world files
  (idempotence).** Duplicate-object merging ran exactly one generation per
  call: merging duplicate leaf objects (e.g. repeated ColorSpace entries)
  remaps references, which can make their *parents* — image streams, then
  dicts referencing those — newly byte-identical, and that next generation of
  merges was left to the next `optimize` call. Repro: a 16 MB NASA scan where
  pass two removed 7 more objects (−3,725 bytes). The non-stream and stream
  dedup passes now alternate to a fixpoint inside a single call (each
  effective round strictly removes at least one object, so termination is
  guaranteed).
- **Page-tree nodes are never deduplicated, even when byte-identical.**
  Surfaced by the fixpoint change on the same NASA file: two blank pages with
  identical dicts (after their empty content streams and thumbnails merged)
  were collapsed into one object, putting the same id in `/Kids` twice —
  which changes what GoTo destinations resolve to, and breaks lopdf 0.42's
  `renumber_objects_with` page-reordering pass (it assumes distinct page ids
  and silently overwrote an unrelated scanned page, orphaning its 1.37 MB
  image subtree). Page identity is load-bearing; `/Type /Page` and
  `/Type /Pages` dicts are now excluded from dedup. The same hazard was
  latent in earlier releases for documents whose pages are byte-identical at
  load time.

### Notes

- Fonts amatl does not subset are always left byte-for-byte untouched:
  simple (non-Type0) fonts and predefined CJK CMaps are permanently out of
  scope; embedded CMap streams and CIDFontType0 (CFF) are future milestones.
- The default-ON flip for `subset_fonts` is planned only after a clean
  corpus soak (see docs/PHASE3-PLAN.md, decision #3).

## [0.2.0] - 2026-08-21

### Added

- **FlateDecode image downsampling, on by default.** Over-resolution
  Flate-compressed raster images (the PNG-like class: screenshots, exported
  bitmaps, lossless scans) are now downsampled to the effective target DPI *in
  the same format* — decoded (including PNG predictors 10–15), resized with
  Lanczos3, and re-deflated at maximum compression, choosing the smaller of a
  PNG Up-predictor and a plain no-predictor encoding. `/ColorSpace` is never
  touched, so no JPEG-class artifacts can be introduced. Scope: 8-bit
  DeviceRGB / DeviceGray / ICCBased (N = 1 or 3) images without `/SMask`,
  `/Mask`, or `/Decode`; everything else (Indexed, 1/2/4/16-bit, TIFF
  Predictor 2, array-form `/DecodeParms`) is deliberately left byte-for-byte
  untouched.
- `OptimizeOptions::downsample_flate_images` (default `true`) and the
  `with_downsample_flate_images` builder, for callers who need the 0.1.x
  behavior (`false` restores it exactly).
- Decompression-bomb guard on the Flate path: a hard 256 MiB raw-pixel cap
  enforced *during* inflation, plus an exact decoded-length check — any
  mismatch (truncated stream, lying dimensions) leaves the image untouched.

### Changed

- **Behavior change:** documents containing over-resolution Flate images now
  produce different (smaller) output than 0.1.x by default. Pin
  `downsample_flate_images = false` for bit-stable output across the upgrade.
- The committed test fixture (`fixtures/sample.pdf`) grew from two JPEG pages
  to four pages (two JPEG + two FlateDecode), exercising both pipelines.

### Fixed

- PNG predictor inversion is performed by amatl itself (spec-correct,
  including the Avg filter) instead of lopdf 0.42's
  `Stream::decompressed_content`, whose Avg-filter implementation produces
  incorrect pixels and which returns partial data on corrupt zlib streams.

## [0.1.0] - 2026-08-21

### Added

- Initial release: CTM-aware effective-DPI downsampling of over-resolution
  DCTDecode (JPEG) images via mozjpeg, duplicate object/stream deduplication,
  optional accessibility-tree stripping, optional PDF 1.5 object-stream
  packing, and the hard fail-safe contract (never larger, never corrupt,
  original bytes on any failure).
