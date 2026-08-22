# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
