# amatl

[![CI](https://github.com/xcheljd/amatl/actions/workflows/rust.yml/badge.svg)](https://github.com/xcheljd/amatl/actions/workflows/rust.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Pure-Rust PDF size optimization. Named for the Nahuatl word *āmatl*, the
fig-bark paper of pre-Columbian Mesoamerican codices.

Amatl shrinks PDFs by downsampling over-resolution embedded JPEGs to the
resolution they are **actually rendered at** — and nothing else. Text and
vectors are never touched, output is never larger than the input, and the
library never panics on untrusted bytes.

## Who this is for

Developers and companies embedding PDF compression into their own product or
pipeline — who can't ship AGPL Ghostscript and won't upload customer documents
to a third-party web service. Amatl is a permissively licensed library with no
native runtime dependencies; everything happens locally. It is not aimed at the
consumer "shrink my PDF" use case, which free web tools already serve.

## The core idea: effective DPI, not blind DPI

Most compressors downsample every image toward a fixed DPI, as if each image
filled the page. Amatl instead walks each page's content stream tracking the
current transformation matrix (CTM), so it knows the on-page rendered size in
points of every painted image XObject. From that it computes each image's
**effective DPI** — pixels divided by the inches it actually occupies — and
downsamples only the images that are genuinely over-resolved for how they are
displayed:

- An image reused on several pages is sized to its **largest** placement, so a
  shared asset is never under-resolved.
- Effective DPI is evaluated on **both axes**, so a non-uniformly scaled image
  (say 1000×1000 px drawn into a 500×100 pt box) is caught even when one axis
  looks fine.
- Images at or below the target are left byte-for-byte untouched — no
  churn, no upscaling, no generation loss.

Over-resolved JPEGs are decoded with libjpeg's DCT-domain scaled decoding (the
full-resolution bitmap is never materialized), resized with Lanczos3, and
re-encoded with [mozjpeg] (optimized Huffman + trellis quantization). At the
default 130 DPI / quality 78 — a measured visual-lossless sweet spot for
business documents — the re-encoded image payload matches Ghostscript's within
~0.01%.

Since 0.2.0, over-resolution **FlateDecode** raster images (screenshots,
exported bitmaps, lossless scans — the PNG-like class) are downsampled by
default too, *in the same format*: decoded (including PNG predictors), resized
with Lanczos3, and re-deflated at maximum compression, with `/ColorSpace`
untouched. Because the format never changes, no JPEG ringing is ever
introduced on the flat-color/line-art content Flate typically holds. Flate
images run through the same effective-DPI placement analysis as JPEGs, under
the same never-larger / never-corrupt contract. Callers who need the previous
behavior can opt out with `with_downsample_flate_images(false)`.

[mozjpeg]: https://github.com/mozilla/mozjpeg

## Fail-safe contract

For any input — malformed PDFs, truncated streams, crafted attacker input,
empty slices — `optimize` returns without panicking. On any error, panic, or
non-shrinking result, the returned bytes equal the input. Callers can treat the
output as always valid and always at most as large as the input.

This is enforced by a `std::panic::catch_unwind` boundary around the whole
pipeline (a panic deep in the JPEG decoder, the mozjpeg encoder, or the PDF
parser becomes the same graceful fallback as an ordinary error) and pinned by
regression tests for the three failure shapes: panic, degenerate input, and
parse error. Amatl is safe to run on untrusted input in an automated pipeline.

## Usage

```rust
// Accessibility-preserving defaults:
let optimized: Vec<u8> = amatl::optimize(&input_bytes);

// Or tune the pipeline:
use amatl::OptimizeOptions;
let opts = OptimizeOptions::default()
    .with_target_dpi(110.0)
    .with_jpeg_quality(70)
    .with_strip_accessibility(true)     // opt-in, off by default
    .with_pack_object_streams(false);   // PDF 1.5 ObjStm packing, ON by default
let optimized = amatl::optimize_with_options(&input_bytes, opts);
```

Beyond downsampling, amatl also merges byte-identical duplicate objects and
image streams (a logo re-embedded once per page is stored — and re-encoded —
once), re-deflates every FlateDecode stream at zlib level 9, packs objects into
PDF 1.5 object streams, and compresses the cross-reference stream — and can
optionally strip the accessibility structure tree.

**Object-stream packing is on by default** (it was off through 0.3.0). It is lossless — same
objects, different serialization — but puts a **PDF 1.5 floor** on the output:
readers older than Acrobat 6 (2003) cannot open an `ObjStm` file at all. Pass
`--no-pack-object-streams` (library: `.with_pack_object_streams(false)`) if
your audience may include one.

**Accessibility-preserving by default.** Ghostscript's `/ebook` and `/screen`
presets silently remove the PDF structure tree that screen readers navigate.
Amatl's default keeps it; `strip_accessibility` is a deliberate, documented
opt-in for callers who know their audience (it buys roughly 18 percentage
points of additional reduction on structure-heavy documents, and degrades the
file from tagged to untagged).

**Amatl will never lossy-symbol-encode your scans.** Symbol-mode JBIG2 — the
encoding behind the Xerox scanner scandal, where visually plausible character
substitution silently turned 6s into 8s in scanned documents — is permanently
out of scope, as a commitment rather than a missing feature. Any future
bitonal recompression work is restricted to lossless encodings whose output
can be verified by exact decode-back comparison.

## CLI

amatl ships as a library and a command-line binary:

```sh
# Install from source (crates.io publishing is a later roadmap phase)
cargo install amatl

# Optimize with accessibility-preserving defaults; writes sample.optimized.pdf
amatl report.pdf

# Choose the output path explicitly
amatl report.pdf -o report.small.pdf

# Overwrite in place, tune the pipeline
amatl scan.pdf --force --target-dpi 150 --jpeg-quality 70 \
  --recompress-bitonal-images

# Keep the output readable by pre-PDF-1.5 viewers (no ObjStm packing)
amatl report.pdf --no-pack-object-streams
```

Every flag maps 1:1 to an `OptimizeOptions` builder method, and all defaults —
including the boolean flags' on/off state shown under `--help` — are read from
`OptimizeOptions::default()` at runtime, so the CLI can never drift from the
library. Non-PDF input is rejected by header sniffing; without `-o` or
`--force` the input file is never overwritten. On success the CLI prints
input → output size, percent saved, and elapsed time.

## Measured results

On the committed synthetic fixture (`fixtures/sample.pdf`, four pages as of
0.2.0 — two JPEG pages and two FlateDecode pages, reproducible via
`scripts/bench-vs-gs.sh`): amatl (strip, no packing) takes 662,107 bytes to
123,948 bytes (**18% of input**), downsampling the over-resolution JPEG *and*
Flate images while leaving both under-resolution pages byte-for-byte alone.
Ghostscript 10.07.1 at the same mirrored settings takes the four-page fixture
to 71,742 bytes (11%): its forced `DCTEncode` re-encodes the two Flate pages
as JPEG, and the fixture's Flate content is synthetic noise — the class JPEG
compresses best and Flate compresses worst — so the gap overstates the
typical case. Amatl keeps Flate images Flate by design (see Scope).

On the previous JPEG-only two-page fixture (Ghostscript 10.07.1 at mirrored
settings — 130 DPI, 1.15 threshold, DCTEncode QFactor 0.4):

| Pipeline | Bytes | % of input |
| --- | ---: | ---: |
| input | 193,668 | 100% |
| **amatl** (strip, no packing) | **27,087** | **13%** |
| Ghostscript pdfwrite | 43,722 | 22% |

On a public real-world document —
[NASA TM-20210010291](https://ntrs.nasa.gov/citations/20210010291), a 16.8 MB
58-page technical report — amatl 0.3.1-dev at defaults takes 16,804,107 bytes
to **4,448,544 bytes, a 73.5% reduction**, byte-stable under repeated
optimization (0.3.1 released at 4,655,752; the difference is the zlib-rs
deflate backend, −82,722 B, plus default-on font subsetting, −124,486 B).
Ghostscript 10.07.1 at the same *matched-intent* settings (`/ebook`, which
keeps lossless images lossless) produces 4,931,425 bytes — amatl is
**9.8% smaller** and keeps the accessibility structure tree that GS
silently strips. Only when Ghostscript is pushed to aggressive lossy settings
(forced 130-DPI downsampling + `DCTEncode`) does it go smaller; amatl closes
most of that gap with the opt-in `--allow-lossy` flag (below) without ever
re-encoding without consent.

### NASA TM-20210010291 — flag comparison

| Pipeline | Bytes | % of input | Notes |
| --- | ---: | ---: | --- |
| input | 16,804,107 | 100% | 58-page technical report |
| Ghostscript `/ebook` (matched intent) | 4,931,425 | 29.3% | keeps lossless images lossless; strips accessibility; AGPL |
| **amatl defaults** (lossless-only) | **4,448,544** | **26.5%** | every image class handled; fonts subset; no encoding-class changes |
| amatl `--allow-lossy` q78 | 3,342,293 | 19.9% | + explicit consent: Flate photos → JPEG (incl. masked pairs), line art auto-declined |
| Ghostscript forced 130 DPI + DCT (aggressive) | 3,054,642 | 18.2% | re-encodes *all* imagery incl. line art; strips accessibility; AGPL |

What each level buys:

- **Defaults (no flags):** transparency-masked JPEG requantization and coupled
  downsampling (D-M1/D-M2), masked and unmasked Flate coupled downsampling
  (D-M3), shared-mask and under-threshold requantization (Phase 6), font
  subsetting (Type0/CIDFontType2 Identity-H/V and nonsymbolic simple
  TrueType — rendering-preserving, text extraction bit-identical), then a
  serialization pass — object-stream packing (PDF 1.5), a final level-9
  re-deflate of every Flate stream through the zlib-rs backend, and a
  compressed cross-reference stream. Every image keeps its encoding class;
  lossless images stay lossless; soft masks are never resized when another
  image shares them.
- **`--allow-lossy`:** additionally re-encodes *unmasked photographic-looking*
  FlateDecode images as JPEG at the configured quality. A built-in content
  heuristic declines line-art-like images (thin lines pick up visible JPEG
  mottling for marginal savings) and the flag never changes geometry the
  lossless path declined to change — one consent covers re-encoding only.
  Measured effect on this corpus: −24.9% over defaults, with converted streams
  visually indistinguishable at review zoom.

For calibration: at matched intent (lossless in, lossless out) amatl now
*beats* Ghostscript on this corpus while preserving the accessibility tree.
The remaining distance appears only at Ghostscript's most aggressive settings,
which re-encode all lossless imagery (including line art), rewrite fonts, and
strip the structure tree — trades amatl declines by contract even under
`--allow-lossy`.

On a real-world retail promotion flyer (1,376 KB, image-heavy, ~2,000
structure-tree objects) — **from a private corpus, not redistributable**:

| Pipeline | Size | Reduction | `qpdf --check` | Accessibility |
| --- | ---: | ---: | --- | --- |
| amatl, library defaults (no strip) | 821 KB | 40% | clean | preserved |
| amatl, strip + pack | 572 KB | 59% | clean | stripped |
| Ghostscript 130 DPI / QFactor 0.4 | 530 KB | 62% | clean | stripped |

Amatl matches or beats Ghostscript on image payload at matched intent (on
NASA defaults it is 5.6% smaller overall) rather than chasing the last few
points of its most aggressive settings, which come from re-encoding *all*
lossless imagery — including line art — and stripping the accessibility tree.
The differentiators are the CTM-aware placement analysis, the fail-safe
contract, and the license.

## Why not Ghostscript?

Amatl exists because shelling out to `gs` was evaluated and rejected:

- **AGPL-3.0.** A landmine for anyone embedding compression in a product.
  Amatl and all of its dependencies are permissively licensed.
- **Security surface.** Ghostscript has a long CVE history of RCE via crafted
  input (`-dSAFER` escapes) — exactly the wrong tool to point at arbitrary
  user-supplied PDFs. Amatl is a narrow pure-Rust parser plus a JPEG codec,
  wrapped in a fail-safe boundary, not a PostScript interpreter.
- **Bundling cost.** Portable static `gs` builds and per-platform signing are
  an ongoing tax. Amatl is a Cargo dependency with no runtime dependencies.
- **Marginal upside on the target corpus.** With JPEG payloads already matched
  byte-for-byte, Ghostscript's remaining ~3-4 point advantage on
  JPEG-dominated business documents is structural micro-optimization — not
  worth AGPL + an RCE surface + bundling. (On lossless-image-heavy corpora
  Ghostscript's ratio lead is real and larger — see the NASA numbers above —
  because it converts those images to JPEG by default, a trade amatl only
  ever makes as an explicit opt-in.)

## Scope

Amatl currently optimizes baseline-JPEG (`DCTDecode`) images without soft
masks, in RGB or grayscale (CMYK/YCCK decode through a conservative fallback
path), and — since 0.2.0 — 8-bit `FlateDecode` raster images in
DeviceRGB/DeviceGray/ICCBased(N=1,3), downsampled in place with the format and
color space preserved. Flate images that are Indexed, 1/2/4/16-bit,
soft-masked, `/Decode`-remapped, or TIFF-predicted are deliberately left
untouched. It does not (yet) recompress `CCITT`/`JBIG2` images. On
text-heavy PDFs with default options it will honestly do very little, and by
contract it returns the input unchanged rather than a worse file.

Embedded fonts are subset to the glyphs actually shown — on by default since
0.3.1-dev (opt out with `--no-subset-fonts` / `with_subset_fonts(false)`;
introduced opt-in in 0.2.1). Two font classes are covered, and neither ever
rewrites content-stream text bytes:

- **Type0/CIDFontType2 (Identity-H/V)**: only `/FontFile2` and
  `/CIDToGIDMap` (as an old-CID → new-GID stream) are replaced, so
  `/W`/`/DW`/`/ToUnicode` stay untouched and text extraction is bit-identical
  pre/post — the "rewrote your text wrong" bug class is structurally
  impossible.
- **Simple TrueType** (nonsymbolic, WinAnsi/MacRoman encodings incl.
  `/Differences`, since 0.3.1-dev): the subset font gets a freshly written
  `cmap` replicating the original's subtables (restricted to retained
  glyphs), so every viewer lookup path of ISO 32000-1 9.6.6.4 resolves each
  code to the same outline as before; codes, `/Encoding`, `/Widths`, and
  `/ToUnicode` never change. Symbolic fonts, unknown encodings or glyph
  names, and any used code the font's `cmap` cannot resolve disqualify that
  font, untouched.

Any parse uncertainty anywhere disables subsetting for the affected font or
the whole document; PDF/A-declared and encrypted documents are skipped.

On fonts, two limitations are permanent by design, not roadmap gaps:
**Type1 subsetting is out of scope** (CFF/Type1 rewriting is a different
machine, and non-embedded Type1 base fonts carry no bytes to subset), and
**predefined CJK CMaps** (`UniGB-UCS2-H` and friends) are unsupported, since
supporting them means bundling megabytes of Adobe mapping tables for a
shrinking legacy corpus. In both cases the affected fonts are left entirely
untouched and the output is always a valid PDF.

## Maintenance notes & constraints

- **lopdf is pinned at 0.42** (hard ceiling): 0.43 and 0.44 fail to compile
  against current `time` (their `datetime.rs` calls
  `FormatItem::StringLiteral`, which no longer exists in `time` 0.3.47 — the
  error is inside lopdf, not amatl). Re-test the bump when lopdf publishes a
  fix.
- **Build-time requirements:** mozjpeg compiles libjpeg-turbo, which needs
  NASM and a C compiler at build time (CI uses `ilammy/setup-nasm`). There is
  no native *runtime* dependency.
- **MSRV: 1.88.0**, enforced by a dedicated CI job. The floor is
  dependency-driven: lopdf 0.42.0's manifest declares edition 2024 (Cargo/Rust
  >= 1.85), and current `image`/`time` releases require rustc 1.88.
- mozjpeg's IJG license requires an attribution notice in documentation
  accompanying *binary* distributions; a `NOTICE` file will ship if and when
  prebuilt binaries do. Source distribution does not trigger it.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
