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
    .with_strip_accessibility(true)   // opt-in, off by default
    .with_pack_object_streams(true);  // PDF 1.5 ObjStm packing, off by default
let optimized = amatl::optimize_with_options(&input_bytes, opts);
```

Beyond downsampling, amatl also merges byte-identical duplicate objects and
image streams (a logo re-embedded once per page is stored — and re-encoded —
once), and can optionally strip the accessibility structure tree and pack
objects into PDF 1.5 object streams.

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
58-page technical report — amatl 0.2.0 (strip, no packing) takes 16,804,107
bytes to 11,353,517 bytes, a 32% reduction (0.1.x, JPEG-only, managed 24.5%
on the same file). Ghostscript 10.07.1 (`/ebook` with
`-dDetectDuplicateImages=true`) takes the same file to 4,931,402 bytes —
clearly ahead on raw ratio for this corpus. Most of that gap is Ghostscript's
willingness to re-encode lossless images as JPEG (an artifact-class change
amatl treats as a future explicit opt-in, not a default), plus font
subsetting and structural micro-optimization. Shown honestly: on
scan-heavy/lossless-image corpora, Ghostscript currently wins on ratio; amatl
wins on the contract, the license, and the security surface.

On a real-world retail promotion flyer (1,376 KB, image-heavy, ~2,000
structure-tree objects) — **from a private corpus, not redistributable**:

| Pipeline | Size | Reduction | `qpdf --check` | Accessibility |
| --- | ---: | ---: | --- | --- |
| amatl, library defaults (no strip) | 821 KB | 40% | clean | preserved |
| amatl, strip + pack | 572 KB | 59% | clean | stripped |
| Ghostscript 130 DPI / QFactor 0.4 | 530 KB | 62% | clean | stripped |

Amatl deliberately ties Ghostscript on image payload (within ~0.01% — the
images are the ~80% of bytes that matter) rather than chasing the last few
points, which come from structural micro-optimizations like font rewriting.
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

Since 0.2.1, embedded Type0/CIDFontType2 (Identity-H/V) fonts
can be subset to the glyphs actually shown via the opt-in
`with_subset_fonts(true)`. The implementation replaces only `/FontFile2` and
`/CIDToGIDMap` (as an old-CID → new-GID stream), so content-stream text
bytes are never rewritten, `/W`/`/DW`/`/ToUnicode` stay untouched, and text
extraction is bit-identical pre/post — the "rewrote your text wrong" bug
class is structurally impossible. Any parse uncertainty anywhere disables
subsetting for the affected font or the whole document; PDF/A-declared and
encrypted documents are skipped. The flag stays opt-in until it has soaked
against real corpora (Ghostscript subsets by default; default-ON is the
explicit target).

On fonts, two limitations are permanent by design, not roadmap gaps:
**simple-font (non-Type0) subsetting is out of scope** — subsetting a simple
TrueType or Type1 font would require converting it to Type0 and rewriting
every text-show string in the document, which is exactly the
"silently rewrote your text wrong" bug class amatl refuses to ship — and
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
