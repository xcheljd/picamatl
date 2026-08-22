# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
