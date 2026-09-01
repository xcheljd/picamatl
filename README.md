# picamatl

[![CI](https://github.com/xcheljd/picamatl/actions/workflows/rust.yml/badge.svg)](https://github.com/xcheljd/picamatl/actions/workflows/rust.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Shrink PDFs up to 80% without uploading them anywhere.** Picamatl is a
pure-Rust optimizer that downsamples over-resolution images to their
effective on-page DPI — lossless by default, never larger than the input,
and safe on untrusted files. No AGPL, no Ghostscript, no network.

**Your documents never leave your machine.** Free web tools make you upload
client contracts, medical forms, and financial records to someone else's
server. Picamatl makes zero network calls — the entire dependency tree is
local-only. What happens on your disk stays on your disk.

**Your PDFs come out working.** Accessibility tags survive. Forms survive.
Text extracts identically. Output is never larger than the input — any
change that can't prove it shrinks is declined. Malformed or hostile PDFs
come back byte-for-byte unchanged, never corrupted.

**No Ghostscript.** Picamatl is pure Rust — no shelled-out processors, no
AGPL license contaminating your product, no PostScript interpreter CVE
surface pointed at untrusted input. One of the very few PDF compressors
that can say that.

- **Defaults are lossless** — encoding classes preserved, accessibility
  kept, metadata kept.
- **Effective-DPI aware** — an image reused across pages is sized to its
  largest placement; only genuinely over-resolved images are downsampled.

The name fuses *pico-* (tiny) with *āmatl*, the Nahuatl word for the
fig-bark paper of pre-Columbian Mesoamerican codices — small pages of
āmatl.

## Quick start

```bash
cargo install picamatl
picamatl report.pdf
# report.pdf: 16,804,107 bytes -> 4,448,544 bytes (73.5% saved) in 0.67s
```

No toolchain? Prebuilt binaries for Linux, macOS, and Windows are on the
[releases page](https://github.com/xcheljd/picamatl/releases) — download,
extract, run.

Batch a whole folder:

```bash
picamatl ~/scans --output-dir ~/scans-small
```

## Who this is for

**You embed PDF handling in a product** — and need a permissively licensed
(MIT/Apache) compressor without AGPL contamination, a C-library bundling
tax, or an upload-to-our-servers privacy policy. Picamatl is a library with
a stable API and a hard fail-safe contract, built for pipelines that
process untrusted input.

**You handle sensitive documents professionally** — client files, legal,
medical, financial — and "just upload it to our free tool" has never been
an acceptable answer. Picamatl is a local command that never phones home.

**You maintain infrastructure that processes PDFs in bulk** — and you need
the guarantee that malformed input cannot crash the worker: picamatl
returns the input unchanged on any error, panic, or non-shrinking result.

→ **Architecture**: see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the
pipeline, invariants, and source map.

## The core idea: effective DPI, not blind DPI

Most compressors downsample every image toward a fixed DPI, as if each image
filled the page. Picamatl instead walks each page's content stream tracking the
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
parse error. Picamatl is safe to run on untrusted input in an automated pipeline.

**Numbers keep the value they had in the input.** The PDF library picamatl
builds on stores every real as an `f32`, which silently shortens any literal
needing more than ~7 significant digits — `/MediaBox [0 0 595.91998
841.91998]`, LibreOffice's ordinary A4, would come back out as `595.92
841.92`. A viewer parses reals as doubles and grid-fits the whole page against
its box, so that 2e-5 pt is enough to move text by a pixel. picamatl reads the
input's own literals before the parse and splices them back into the saved
file (`src/reals.rs`), for every real in the document rather than page boxes
alone: 27 of 28 pages of the LibreOffice document in the corpus rendered
differently before, and 0 of 28 do now. Where the input spells one value two
ways, picamatl leaves the number as-is rather than pick.

**Digital signatures do not survive optimization.** Every picamatl run
re-serializes the whole document, so a `/ByteRange` digest — which pins file
offsets — is invalid in the output. This has always been true (font
subsetting, image downsampling and object-stream packing were never gated on
signatures); as of the current release the three entropy-level passes
(re-deflate, JPEG Huffman re-optimization, content minification) no longer
pretend otherwise by declining, which is worth up to 24% of the output on a
Reader-extended form. If a signature must stay valid, do not optimize the
file.

## Library usage

```rust
// Accessibility-preserving defaults:
let optimized: Vec<u8> = picamatl::optimize(&input_bytes);

// Or tune the pipeline:
use picamatl::OptimizeOptions;
let opts = OptimizeOptions::default()
    .with_target_dpi(110.0)
    .with_jpeg_quality(70)
    .with_strip_accessibility(true)     // opt-in, off by default
    .with_strip_metadata(true)          // drop XMP packets; opt-in, off by default
    .with_pack_object_streams(false);   // PDF 1.5 ObjStm packing, ON by default
let optimized = picamatl::optimize_with_options(&input_bytes, opts);
```

Beyond downsampling, picamatl also merges byte-identical duplicate objects and
image streams (a logo re-embedded once per page is stored — and re-encoded —
once), re-deflates every FlateDecode stream at zlib level 9, packs objects into
PDF 1.5 object streams, and compresses the cross-reference stream — and can
optionally strip the accessibility structure tree.

**Object-stream packing is on by default** (it was off through 0.3.0). It is lossless — same
objects, different serialization — but puts a **PDF 1.5 floor** on the output:
readers older than Acrobat 6 (2003) cannot open an `ObjStm` file at all. Pass
`--no-pack-object-streams` (library: `.with_pack_object_streams(false)`) if
your audience may include one.

**Accessibility-preserving by default.** Picamatl keeps the PDF structure tree
that screen readers navigate. `strip_accessibility` is a deliberate,
documented opt-in for callers who know their audience (it buys roughly 18
percentage points of additional reduction on structure-heavy documents, and
degrades the file from tagged to untagged).

**The declared PDF version is never altered.** Picamatl preserves the input's
declared PDF version and every document property byte-identically: no version
downgrades, no color-space conversions, no page auto-rotation, nothing
rewritten that the document did not already say. If your workflow needs output
targeted at an older viewer generation, that is a decision you make per
pipeline — not something an optimizer does to your files on the way through.

**Metadata is kept by default.** Some producers stamp a full XMP packet on
every page and XObject — the PDF 1.7 specification file carries 134 of them,
860 KB, 12% of its optimized size. `--strip-metadata`
(library: `.with_strip_metadata(true)`) removes every `/Metadata` entry. It is
render-identical but discards provenance and breaks PDF/A and PDF/UA
identification, so it is opt-in.

**Private authoring data is kept by default.** An Illustrator- or
InDesign-authored figure carries the producer's own editable copy of the
artwork in a `/PieceInfo` page-piece dictionary (ISO 32000-1 14.5), beside the
flattened page that actually draws it. No conforming reader consults it to
render: on the corpus it is 295 KB of a 374 KB file (96% of picamatl's output for
that file) and 240 KB inside a 2.2 MB paper. `--strip-private-data`
(library: `.with_strip_private_data(true)`) removes every `/PieceInfo` entry.
It is pixel-identical — verified page by page — but the producing application
loses its round trip, so it is opt-in.

**Figures can keep a higher DPI than photos.** `--figure-dpi <DPI>`
(library: `.with_figure_dpi(195.0)`, **unset by default**) is a *quality knob*,
not a consent tier: images that a cheap pixel-statistic heuristic classifies as
figure-like — charts, graphs, and diagrams carrying rendered text, where the
ordinary target turns axis labels and wordmarks to mush while a photograph at
the same DPI looks fine — are downsampled to this higher target instead of
`--target-dpi`. Everything else keeps compressing at the ordinary target.

```sh
picamatl report.pdf --figure-dpi 195
```

The classifier is heuristic-only — no OCR, no new dependencies, and it reuses
the metrics pass the line-art guard already runs. An image is promoted when its
dominant color covers 25–75% of the pixels *and* at least 10% of pixels sit on
a sharp edge: rendered text and plot rules produce many short high-contrast
runs on flat paper, while a photograph's tonal gradients leave no flat
background at all. The upper bound is exactly the line-art guard's lower bound,
so the two defer to each other rather than overlapping. The operating point is
tuned to be **false-positive-free** — it would rather leave a chart at the
ordinary target than spend bytes on a photograph.

It never makes output lossier. A higher figure target only raises the geometry
fed to the candidates the pipeline was already going to build, so output is
more faithful and at most larger than with the flag unset — and the
never-larger-than-the-original contract is untouched. Values at or below
`--target-dpi` are inert (that is what keeps the promise true), and it requires
a positive `--target-dpi`, since `<= 0` already means "do not downsample".

Measured on `corpus-expanded/nist-sp800-63b.pdf` (80 pages, 69
over-resolution images) at `--figure-dpi 195`: two images promoted from 130 to
195 DPI — the NIST wordmark on p1 and a diagram on p26, both `/SMask` pairs
whose masks followed their base to the same geometry — for **+8,301 bytes**
(867,951 → 876,252, 0.96%). Rendered at 150 DPI and compared page by page
against the *original*, both pages land measurably closer to it than the
defaults do (mean absolute difference 0.333 → 0.225 and 0.106 → 0.046) and the
other 78 pages are unchanged. Classification costs one full-resolution decode
per over-resolution image; on the two heaviest corpus files the flag is inside
run-to-run noise (`irs-1040gi` 15.98s → 16.04s, `adobe-spec` 22.65s → 22.64s).

**Picamatl will never lossy-symbol-encode your scans.** Symbol-mode JBIG2 — the
encoding behind the Xerox scanner scandal, where visually plausible character
substitution silently turned 6s into 8s in scanned documents — is permanently
out of scope, as a commitment rather than a missing feature. Any future
bitonal recompression work is restricted to lossless encodings whose output
can be verified by exact decode-back comparison.

## CLI

picamatl ships as a library and a command-line binary:

```bash
# Optimize with accessibility-preserving defaults; writes sample.optimized.pdf
picamatl report.pdf

# Choose the output path explicitly
picamatl report.pdf -o report.small.pdf

# Batch: every *.pdf in a folder, outputs into another folder
picamatl ~/scans --output-dir ~/scans-small

# Batch: keep outputs beside the inputs with a suffix
picamatl ~/scans --suffix "-small"

# Both together: ~/scans/*.pdf -> ~/small/report-min.pdf ...
picamatl ~/scans --output-dir ~/small --suffix "-min"

# Overwrite in place, tune the pipeline
picamatl scan.pdf --force --target-dpi 150 --jpeg-quality 70 \
  --recompress-bitonal-images

# Keep the output readable by pre-PDF-1.5 viewers (no ObjStm packing)
picamatl report.pdf --no-pack-object-streams

# Keep charts and diagrams legible at a higher DPI than photographic content
picamatl paper.pdf --figure-dpi 195
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
`scripts/bench-vs-gs.sh`): picamatl (strip, no packing) takes 662,107 bytes to
117,459 bytes (**18% of input**), downsampling the over-resolution JPEG *and*
Flate images while leaving both under-resolution pages byte-for-byte alone.
Ghostscript 10.07.1 at the same mirrored settings takes the four-page fixture
to 71,742 bytes (11%): its forced `DCTEncode` re-encodes the two Flate pages
as JPEG, and the fixture's Flate content is synthetic noise — the class JPEG
compresses best and Flate compresses worst — so the gap overstates the
typical case. Picamatl keeps Flate images Flate by design (see Scope).

On the previous JPEG-only two-page fixture (Ghostscript 10.07.1 at mirrored
settings — 130 DPI, 1.15 threshold, DCTEncode QFactor 0.4):

| Pipeline | Bytes | % of input |
| --- | ---: | ---: |
| input | 193,668 | 100% |
| **picamatl** (strip, no packing) | **27,087** | **13%** |
| Ghostscript pdfwrite | 43,722 | 22% |

On a public real-world document —
[NASA TM-20210010291](https://ntrs.nasa.gov/citations/20210010291), a 16.8 MB
58-page technical report — picamatl 0.4.0 at defaults takes 16,804,107 bytes
to **4,448,544 bytes, a 73.5% reduction**, byte-stable under repeated
optimization (0.3.1 released at 4,655,752; the difference is the zlib-rs
deflate backend, −82,722 B, plus default-on font subsetting, −124,486 B).
Ghostscript 10.07.1 at the same *matched-intent* settings (`/ebook`, which
keeps lossless images lossless) produces 4,931,425 bytes — picamatl is
**9.8% smaller**, and keeps the accessibility structure tree. Only when
Ghostscript is pushed to aggressive lossy settings (forced 130-DPI
downsampling + `DCTEncode`) does it go smaller; picamatl closes most of that gap
with the opt-in `--allow-lossy` flag (below) without ever re-encoding without
consent.

### NASA TM-20210010291 — flag comparison

| Pipeline | Bytes | % of input | Notes |
| --- | ---: | ---: | --- |
| input | 16,804,107 | 100% | 58-page technical report |
| Ghostscript `/ebook` (matched intent) | 4,931,425 | 29.3% | keeps lossless images lossless; strips accessibility; AGPL |
| **picamatl defaults** (lossless-only) | **4,448,544** | **26.5%** | every image class handled; fonts subset; no encoding-class changes |
| picamatl `--allow-lossy` q78 | 3,342,293 | 19.9% | + explicit consent: Flate photos → JPEG (incl. masked pairs), line art auto-declined |
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
- **`--flatten-forms`:** turns an interactive AcroForm document into a static
  one — every widget appearance is painted into the page it sat on, then
  `/AcroForm`, the field tree, the XFA packet set and every `/Widget`
  annotation go. A semantic change (the output cannot be filled in or signed
  any more), never a silent data loss: a filled-in value survives only because
  the appearance stream that *showed* it became page content, and a document
  where some value could not be preserved that way is declined whole. See
  [Interactive forms](#interactive-forms).

### Interactive forms

`--flatten-forms` (library `with_flatten_forms(true)`, **off by default**) is
picamatl's answer to form-heavy PDFs, the one file class where pure
lossless-only pipelines have historically had nothing to offer.

picamatl flattens under a contract instead. A field's value survives either
because its appearance stream — the object that actually paints the value — is
moved into the page's content stream at the position ISO 32000-1 12.5.5 places
it, or because the field has no value to lose. If neither holds for any field,
the whole document declines and you get the bytes you would have got without
the flag. Dynamic XFA / LiveCycle forms (`/NeedsRendering true`) always
decline: their pages are a placeholder the reader builds from an XML template,
picamatl does not render XFA, and pretending otherwise would ship a blank page.
The full decline table is in [`docs/FORMS-PLAN.md`](docs/FORMS-PLAN.md).

On `corpus-expanded/irs-w2.pdf` (the official IRS Form W-2 — a *static* XFA
form whose 568 widget annotations draw no ink at all, next to a 1.58 MB XFA
packet set):

| Pipeline | Size | of input | Form? | Pages differing from the original render |
| --- | ---: | ---: | --- | ---: |
| input | 2,150,352 | 100.0% | interactive | — |
| picamatl defaults | 1,392,531 | 64.8% | interactive | 0 of 11 |
| picamatl defaults + `--flatten-forms` | 250,229 | 11.6% | static | 0 of 11 |
| picamatl kitchen sink + `--flatten-forms` | **140,215** | **6.5%** | static | 0 of 11 |
| Ghostscript `/ebook` | 189,094 | 8.8% | removed | **11 of 11**, up to 0.55% of pixels |

This is a file class where picamatl previously lost badly on size (57.9% in
`scripts/bench-full.sh`). It is now picamatl that is smaller — and picamatl's
output is the pixel-identical one.

`scripts/forms-vs-gs.sh` shows the contract earning its keep on a real filled
dynamic XFA form (`xfa_filled_imm1344e.pdf`, a Canadian IMM 1344E from the
pdf.js corpus): a naive static-flattening pipeline turns 3,023,968 bytes into
4,158 bytes of *"Please wait…"* placeholder page and takes all **9,298 filled
data nodes** with it. picamatl declines that document — and still shrinks it
88.7% losslessly, with every data node intact, because the XFA packets were
stored undeflated.

Rendered at 150 dpi, all 11 pages of the flattened output
are **pixel-identical** to the original — as are the 9 pages of
`census-brief.pdf`, the corpus's other AcroForm file. The flag is completely
inert on the 14 corpus files that carry no `/AcroForm`: byte-for-byte the same
output with and without it.

For calibration: at matched intent (lossless in, lossless out) picamatl now
*beats* Ghostscript on this corpus while preserving the accessibility tree.
The remaining distance appears only at Ghostscript's most aggressive settings,
which re-encode all lossless imagery (including line art), rewrite fonts, and
strip the structure tree — trades picamatl declines by contract even under
`--allow-lossy`.

The differentiators are the CTM-aware placement analysis, the fail-safe
contract, and the license.

### Five-document public corpus — full matrix (2026-08-24, picamatl post-progressive-JPEG)

A five-file public corpus covering a 756-page technical spec, an arXiv paper,
an IRS tax guide (scanned imagery), an SSD framework document, and a tiny
synthetic file. Ghostscript 10.07.1 at mirrored lossy settings (130 DPI,
1.15 threshold, DCTEncode QFactor 0.4). All sizes are % of input; lower is
better. picamatl rows are cumulative consent levels:

| Pipeline | adobe-spec | arxiv | irs-1040gi | nist-ssdf | dummy | TOTAL |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| input | 22,491,828 B | 2,233,053 B | 4,434,643 B | 739,891 B | 13,264 B | 29,912,679 B |
| **picamatl lossless** (defaults) | 31.9% | 66.1% | 92.6% | 75.9% | 93.9% | **44.5%** |
| picamatl `--allow-lossy` | 31.9% | 66.1% | 92.6% | 75.9% | 93.9% | 44.5% |
| + `--strip-accessibility` | 24.1% | 66.1% | 68.5% | 70.0% | 93.9% | **35.0%** |
| kitchen sink¹ | **19.2%** | **53.1%** | **61.8%** | **47.4%** | **88.6%** | **28.8%** |
| Ghostscript forced lossy² | 48.8% | 55.5% | 72.2% | 82.1% | 114.1% | 53.6% |

¹ `--allow-lossy --strip-accessibility --strip-metadata --convert-type1
--strip-hinting --recompress-bitonal-images --collapse-gray-images
--deflate-backend zopfli`
² `-dDownsampleColor/GrayImages -d*ImageResolution=130
-dColorImageFilter=/DCTEncode` with QFactor 0.4.

Reading the matrix honestly:

- **At full throttle picamatl wins every file**, including the scanned-tax-guide
  class that lossy-only pipelines previously led on — and it grows no file.
  The kitchen-sink run costs ~30× more CPU (zopfli), a deliberate trade under
  its own flag.
- **On this corpus `--allow-lossy` alone is a no-op**: none of the five files
  contain qualifying lossless-Flate photographic images, so the row equals
  defaults by design — the flag only ever fires on content that matches its
  heuristic. The NASA report above shows what it does on photo-heavy input.
- **Where picamatl still trails at low consent levels is structural, not a
  defect:** irs-1040gi's payload is scan raster whose bytes only shrink when
  pixels are degraded. picamatl's lossless row (92.6%) reflects its contract —
  without consent it leaves those pixels untouched, where a lossy pipeline
  degrades them by default. Once equivalent consent is granted (a11y strip +
  font/hinting trades), picamatl passes gs on the same file (61.8% vs 72.2%).
  arxiv behaves the same: its figures respond to forced DCT re-encoding until
  picamatl is given the same license.
- **The progressive-JPEG Huffman pass (this release)** removed the last
  outright decline in the entropy-recode path: progressive (`SOF2`) streams
  are now re-tabled like baseline ones, with losslessness proven by an
  independent in-suite coefficient decoder and jpegtran cross-checks.

### Six-document expanded corpus — industry & edge-case sweep (2026-08-24)

A second corpus targeting document classes absent from the first:
academic preprint (arXiv 2303.08774), census statistical brief (government
charts), a CMYK-JPEG edge-case file (from Mozilla's pdf.js test suite), the
official IRS W-2 fillable form (**XFA/LiveCycle** — see below), the NIST
SP 800-63B standard, and a Wikipedia article render. Same Ghostscript
settings as above:

| Pipeline | arxiv-gpt4 | census | cmyk-jpeg | irs-w2 | nist-sp800 | wiki | TOTAL |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| input | 5,245,564 B | 545,684 B | 374,080 B | 2,150,352 B | 1,480,377 B | 2,196,261 B | 11,992,318 B |
| **picamatl lossless** | 39.6% | **52.3%** | 78.2% | 88.4% | **58.6%** | 48.5% | 54.1% |
| + `--strip-accessibility` | 33.1% | 43.2% | 78.2% | 84.7% | 53.2% | **26.0%** | 45.4% |
| kitchen sink | 32.2% | **35.9%** | 67.4% | 82.9% | **39.7%** | **22.8%** | 41.7% |
| Ghostscript forced lossy | **28.4%** | 60.0% | 4.6%* | **8.8%*** | 70.1% | 37.1% | **32.3%** |

\* Not a fair fight — see below.

Findings from this sweep:

- **The IRS W-2 result exposed picamatl's biggest honest gap: XFA forms.**
  The official W-2 is a LiveCycle/XFA document — 1.58 MB of its bytes are a
  compressed XML Forms Architecture template that *describes* the form and
  which Acrobat renders dynamically. Picamatl correctly refuses to touch it
  (rewriting XFA without Adobe's engine is how forms break), so its lossless
  row sits at 88.4%. The comparison row's smaller number is not compression —
  that pipeline **discards the entire XFA model and re-renders the pages to
  static content**, destroying the fillable form. Verify before trusting any
  optimizer on this file class: the output still opens at 11 identical-looking
  pages, but it is no longer a dynamic form. For this class of document the
  choice is "88% of original, still works" vs "9%, no longer a form".
- **cmyk-jpeg is the mirror image:** a 374 KB file whose payload is one large
  CMYK JPEG. Aggressive lossy re-compression takes that single image down to
  4.6%; picamatl re-encodes it as YCCK at `jpeg_quality`, which is a far more
  conservative trade, so 67.4% is its floor here.

  These two cells got *worse* than the numbers first published for this
  corpus (76.7% and 65.9%), and the correction is worth stating plainly: the
  old numbers were bought with a **corrupt page**. The pre-0.3.2 CMYK
  "fallback" did not decline as the text here claimed — it decoded the image
  to RGB and wrote three channels back under a `/ColorSpace` that still said
  `/DeviceCMYK`. Rendered through Ghostscript, the old output differed from
  the original by up to 251 levels across 5.8% of the page, exactly the image
  bounding box. Full analysis and the cross-decoder verification are in
  [`docs/CMYK-JPEG.md`](docs/CMYK-JPEG.md).
- **Everywhere else picamatl wins or ties:** census brief (35.9% vs 60.0%) —
  vector charts plus fonts respond to subsetting and Type1→CFF conversion;
  NIST SP 800-63B (39.7% vs 70.1%); the Wikipedia render collapses to 22.8%
  once the accessibility tree (26 points of it) is stripped with consent.
  Only the academic preprint class stays marginally ahead under Ghostscript
  (28.4% vs 32.2%).
- Combined across both corpora, picamatl's kitchen-sink configuration wins
  **8 of 11 documents**, never grows a file, keeps dates and provenance
  unless explicitly stripped, and preserves accessibility by default.

### Sixteen-document consolidated matrix (2026-08-24, post CMYK/flatten/f32)

The full corpus — both earlier sweeps plus the XFA fixtures added with
`--flatten-forms`. Kitchen sink now includes `--strip-private-data`
(drops `/PieceInfo` authoring data) and `--flatten-forms`:

| file | input | picamatl kitchen¹ | gs lossy² | winner |
| --- | ---: | ---: | ---: | --- |
| adobe-spec | 22,491,828 B | 19.2% | 48.8% | 🏆 picamatl |
| arxiv-attention | 2,233,053 B | 43.1% | 55.5% | 🏆 picamatl |
| dummy | 13,264 B | 88.6% | 114.1% | 🏆 picamatl |
| irs-1040gi | 4,434,643 B | 31.4% | 72.2% | 🏆 picamatl |
| nist-ssdf | 739,891 B | 47.4% | 82.1% | 🏆 picamatl |
| arxiv-diffusion | 10,267,274 B | 16.2% | 27.1% | 🏆 picamatl |
| arxiv-gpt4 | 5,245,564 B | 16.7% | 28.4% | 🏆 picamatl |
| census-brief | 545,684 B | 35.5% | 60.0% | 🏆 picamatl |
| cmyk-jpeg | 374,080 B | 3.3% | 4.6% | 🏆 picamatl |
| **irs-w2** | 2,150,352 B | **6.2%** | 8.8% | 🏆 picamatl |
| nist-sp800-63b | 1,480,377 B | 39.7% | 70.1% | 🏆 picamatl |
| pypdf-cmyk | 443,953 B | 94.2% | 101.1% | 🏆 picamatl |
| wiki-cmyk-topic | 544,864 B | 42.5% | 65.8% | 🏆 picamatl |
| wiki-pdf | 2,196,261 B | 22.8% | 37.1% | 🏆 picamatl |
| xfa_filled (dynamic) | 3,023,968 B | 10.4% | 0.2%³ | picamatl* |
| xfa_issue14315 (dynamic) | 11,568 B | 52.5% | 28.8%³ | picamatl* |
| **TOTAL** | **56,196,624 B** | **21.3%** | 41.8% | 🏆 **picamatl** |

¹ `--allow-lossy --strip-accessibility --strip-metadata --strip-private-data
--convert-type1 --strip-hinting --recompress-bitonal-images
--collapse-gray-images --flatten-forms --deflate-backend zopfli`
² Ghostscript 10.07.1, mirrored settings (130 DPI / QFactor 0.4).
³ gs's number on dynamic XFA is a 4 KB "Please wait…" placeholder — the
filled form data is destroyed. picamatl declines the XFA model (no static
fallback), compresses everything else, and keeps the data; the * flag
marks the comparison as not apples-to-apples.

Reading the consolidated matrix:

- **Every non-dynamic-XFA file is an picamatl win.** Total: **21.3% of input
  vs gs's 41.8%** across the 56 MB corpus while preserving accessibility,
  form data, and provenance unless explicitly stripped.
- **irs-w2 flipped with `--flatten-forms`**: 2.15 MB → 133.7 KB (6.2%) —
  and the flattened output keeps the filled values. The AcroForm flatten
  path is pixel-fidelity-verified (see `docs/FORMS-PLAN.md`).
- **cmyk-jpeg now beats gs outright** (3.3% vs 4.6%) after the CMYK
  decode/resample/YCCK work; the old 67.4% floor was the honest
  pre-fix number.
- **Dynamic XFA documents**: the comparison row's small sizes come from
  discarding the form model and all filled data. picamatl's contract declines
  those; the gap is a deliberate refusal, not a missing feature (see
  `docs/FORMS-PLAN.md`).

## Why not Ghostscript?

Picamatl exists because shelling out to `gs` was evaluated and rejected:

- **AGPL-3.0.** A landmine for anyone embedding compression in a product.
  Picamatl and all of its dependencies are permissively licensed.
- **Security surface.** Ghostscript has a long CVE history of RCE via crafted
  input (`-dSAFER` escapes) — exactly the wrong tool to point at arbitrary
  user-supplied PDFs. Picamatl is a narrow pure-Rust parser plus a JPEG codec,
  wrapped in a fail-safe boundary, not a PostScript interpreter.
- **Bundling cost.** Portable static `gs` builds and per-platform signing are
  an ongoing tax. Picamatl is a Cargo dependency with no runtime dependencies.
- **Marginal upside on the target corpus.** With JPEG payloads already matched
  byte-for-byte, Ghostscript's remaining ~3-4 point advantage on
  JPEG-dominated business documents is structural micro-optimization — not
  worth AGPL + an RCE surface + bundling. (On lossless-image-heavy corpora
  Ghostscript's ratio lead is real and larger — see the NASA numbers above —
  because it converts those images to JPEG by default, a trade picamatl only
  ever makes as an explicit opt-in.)

## Scope

Picamatl currently optimizes baseline-JPEG (`DCTDecode`) images without soft
masks, in RGB, grayscale, or CMYK/YCCK (four-component payloads are resampled
in native CMYK space and re-encoded as YCCK, never converted to RGB; streams
carrying a `/Decode` array are declined — see
[`docs/CMYK-JPEG.md`](docs/CMYK-JPEG.md)), and — since 0.2.0 — 8-bit `FlateDecode` raster images in
DeviceRGB/DeviceGray/ICCBased(N=1,3), downsampled in place with the format and
color space preserved. Flate images that are Indexed, 1/2/4/16-bit,
soft-masked, `/Decode`-remapped, or TIFF-predicted are deliberately left
untouched. It does not (yet) recompress `CCITT`/`JBIG2` images. On
text-heavy PDFs with default options it will honestly do very little, and by
contract it returns the input unchanged rather than a worse file.

Embedded fonts are subset to the glyphs actually shown — on by default since
0.4.0 (opt out with `--no-subset-fonts` / `with_subset_fonts(false)`;
introduced opt-in in 0.2.1). Two font classes are covered, and neither ever
rewrites content-stream text bytes:

- **Type0/CIDFontType2 (Identity-H/V)**: only `/FontFile2` and
  `/CIDToGIDMap` (as an old-CID → new-GID stream) are replaced, so
  `/W`/`/DW`/`/ToUnicode` stay untouched and text extraction is bit-identical
  pre/post — the "rewrote your text wrong" bug class is structurally
  impossible.
- **Simple TrueType** (nonsymbolic, WinAnsi/MacRoman encodings incl.
  `/Differences`, since 0.4.0): the subset font gets a freshly written
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

- **lopdf 0.44 with `default-features = false`** (verified 2026-08-24): the
  bump from 0.42 dropped lopdf's unneeded datetime backends
  (`chrono`/`jiff`/`time`) and its `rayon` activation — picamatl never parses PDF
  datetimes (Info-dict date strings pass through as opaque bytes), so none of
  those features are needed. Output is byte-identical; binary ~71 KB smaller.
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

**Third-party code.** The embedded JPEG codec is
[mozjpeg](https://github.com/mozilla/mozjpeg) (built on
libjpeg-turbo), licensed under the
[IJG license](https://www.ijg.org/files/copyright.htm) with an added
BSD-3-Clause component — a permissive, BSD-style license that requires this
acknowledgment in derivative works' documentation. All other dependencies are
MIT, Apache-2.0, BSD, Zlib, CC0, or the Unicode license — no copyleft code is
compiled into any release binary.
