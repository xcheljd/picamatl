# picamatl

[![crates.io](https://img.shields.io/crates/v/picamatl.svg)](https://crates.io/crates/picamatl)
[![docs.rs](https://img.shields.io/docsrs/picamatl)](https://docs.rs/picamatl)
[![CI](https://github.com/xcheljd/picamatl/actions/workflows/rust.yml/badge.svg)](https://github.com/xcheljd/picamatl/actions/workflows/rust.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Shrink a real 58-page technical report by 74.4% with the accessibility
tags intact — without uploading it anywhere.** Picamatl is a pure-Rust
optimizer that downsamples over-resolution images to their effective
on-page DPI — lossless by default, never larger than the input, and safe
on untrusted files. No AGPL, no Ghostscript, no network.

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

## What is picamatl?

Picamatl is an open-source command-line tool and Rust library for reducing
the file size of PDF documents. It downsamples embedded raster images that
are stored at a higher resolution than the page actually displays, using
each image's effective on-page DPI rather than a fixed global target. It
runs entirely offline, performs no network requests, preserves the
document's accessibility structure tree and metadata by default, and never
returns an output larger than its input. It is written in Rust, licensed
under MIT OR Apache-2.0, and distributed on crates.io and GitHub
(xcheljd/picamatl) with prebuilt binaries for Linux, macOS, and Windows.

## Quick start

```bash
cargo install picamatl
picamatl report.pdf
```

Both lines below go to stderr; stdout stays empty so picamatl composes cleanly
in a pipeline:

```text
no --output given: writing to report.optimized.pdf (use -o to choose a path, --output-dir/--suffix for batches, or --force to overwrite the input)
report.pdf: 16804107 bytes -> 4297986 bytes (74.4% saved) in 1.21s -> report.optimized.pdf
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
business documents — the two-page JPEG fixture below ends up with 18,885 bytes
of image payload, against 40,164 bytes from Ghostscript 10.08.0 at mirrored
settings and the same pixel dimensions.

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
841.91998]`, the A4 page box Chrome writes when it prints to PDF, would come
back out as `595.92 841.92`. A viewer parses reals as doubles and grid-fits the whole page against
its box, so that 2e-5 pt is enough to move text by a pixel. picamatl reads the
input's own literals before the parse and splices them back into the saved
file (`src/reals.rs`), for every real in the document rather than page boxes
alone: 27 of 28 pages of the Chrome-printed Wikipedia article in the corpus
(`wiki-pdf`) rendered differently before the fix (measured for 0.4.0), and 0
of 28 do now (150 dpi, `--target-dpi 0`, 2026-11-17). Where the input spells one value two
ways, picamatl leaves the number as-is rather than pick.

**Digital signatures do not survive optimization — and picamatl tells you.**
Every run re-serializes the whole document, so a `/ByteRange` digest — which
pins file offsets — is invalid in the output. Before optimizing, picamatl
walks the `/AcroForm` field tree (through `/Kids`, inheriting `/FT` and
`/V`), the merged field/widget signature dictionaries some writers emit, and
the catalog's `/Perms` `/UR3` usage-rights and `/DocMDP` certification
entries — then prints one warning naming each signature it found and the
object number it lives in. Pass `--on-signature silence` in pipelines that
pre-filter signed files.

The warning is only printed when picamatl actually rewrote the document: a
run that declines and hands back your bytes byte for byte invalidates
nothing, and says nothing. (There is no mode that *preserves* a signature:
any re-serialization moves offsets. This is true of every PDF optimizer; the
difference is that picamatl says so at runtime instead of silently.)

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

// Same bytes, plus what the run found. The library never writes to stderr:
// callers that want the signature warning format it themselves.
let (optimized, report) = picamatl::optimize_with_report(&input_bytes, opts);
for signature in &report.signatures {
    println!("{signature}");  // e.g. "signature field (object 7 0)"
}
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
documented opt-in for callers who know their audience: on the corpus below it
buys up to 20.6 percentage points of additional reduction (irs-1040gi goes
from 58.6% to 38.0% of its input, adobe-spec from 30.5% to 23.8%; untagged
files gain nothing), and it degrades the file from tagged to untagged.

**The declared PDF version only goes up, and only for a feature the output
uses.** Picamatl never lowers the `%PDF-` header and never touches the
catalog `/Version`. When the file is written, the header becomes the higher
of the input's header and the lowest version that covers every feature the
output contains:

- A cross-reference stream or object streams: PDF 1.5. Packing (on by
  default) writes both. With `--no-pack-object-streams` the output keeps
  the input's cross-reference form, so only an input that already had a
  cross-reference stream gets one.
- A JBIG2 image: PDF 1.4. By default the lossless bitonal re-store writes
  one in place of a CCITT image wherever that is smaller;
  `--recompress-bitonal-images` also lets it replace 1-bit Flate images.
- A Flate-compressed stream or a CFF (`Type1C`) font: PDF 1.2. Picamatl
  deflates streams that arrive uncompressed and streams it re-encodes, and
  `--convert-type1` writes `Type1C` fonts.
- Features the input already carried count the same way: a JPEG 2000 image
  or a `Crypt` filter (1.5), an OpenType font program (1.6), a
  `CIDFontType0C` font (1.3), a TrueType font (1.1).

So a PDF 1.0 or 1.1 input written with `--no-pack-object-streams` says at
least `%PDF-1.2` once anything in it is deflated. A file whose catalog XMP
claims PDF/A-1 gets a classic cross-reference table and no object streams
even with packing on, because PDF/A-1 forbids both stream forms, so the
PDF 1.5 cross-reference rule never applies to it. `--strip-metadata`
removes that claim, and the file is then packed like any other. A file
picamatl cannot make smaller comes back byte for byte, header included.

Other document properties keep their values: `/Info` entries, the trailer
`/ID`, every page's boxes and `/Rotate` (no page auto-rotation). There is no
color-model conversion: CMYK stays CMYK, RGB stays RGB, ICC-based stays
ICC-based. Three paths write an image's `/ColorSpace`. A downsampled
`/Indexed` image with a soft mask is stored in its palette's base space.
Opt-in `--collapse-gray-images` stores RGB images whose three channels are
equal as DeviceGray. A JPEG 2000 image converted to JPEG under
`--allow-lossy` gets an explicit DeviceGray or DeviceRGB, the color space its
codestream declares.

**Metadata is kept by default.** Some producers stamp a full XMP packet on
every page and XObject — picamatl's output for the PDF 1.7 specification
file still carries 134 of them, 860 KB, 12.5% of its optimized size. `--strip-metadata`
(library: `.with_strip_metadata(true)`) removes every `/Metadata` entry. It is
render-identical but discards provenance and breaks PDF/A and PDF/UA
identification, so it is opt-in.

**Private authoring data is kept by default.** An Illustrator- or
InDesign-authored figure carries the producer's own editable copy of the
artwork in a `/PieceInfo` page-piece dictionary (ISO 32000-1 14.5), beside the
flattened page that actually draws it. No conforming reader consults it to
render: on the corpus, stripping it removes 261 KB of picamatl's 292 KB output
for a 374 KB file (89%) and 240 KB from a 2.2 MB paper. `--strip-private-data`
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
whose masks followed their base to the same geometry — for **+8,322 bytes**
(858,015 → 866,337, 0.97%). Rendered at 150 DPI (pdftoppm, grayscale) and
compared page by page against the *original*, both pages land measurably
closer to it than the defaults do (mean absolute difference 0.178 → 0.070 and
0.106 → 0.046) and the other 78 pages are unchanged. Classification costs one
full-resolution decode per over-resolution image; on the two heaviest corpus
files the flag costs little (median of three runs: `irs-1040gi` 4.15 s →
4.27 s, `adobe-spec` 3.87 s → 3.88 s).

**Picamatl will never lossy-symbol-encode your scans.** Symbol-mode JBIG2 — the
encoding behind the Xerox scanner scandal, where visually plausible character
substitution silently turned 6s into 8s in scanned documents — is permanently
out of scope, as a commitment rather than a missing feature. The JBIG2
picamatl does write is the other half of the standard: one *generic region*
per image, which codes every pixel exactly and never builds a symbol
dictionary, so there is nothing that could stand one character in for
another. Every stream is decoded back by picamatl's own decoder and must
reproduce the source samples bit for bit before it ships, and existing JBIG2
images are left exactly as stored.

## CLI

picamatl ships as a library and a command-line binary:

```bash
# Optimize with accessibility-preserving defaults; writes report.optimized.pdf
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

# Keep CCITT scans as CCITT, for readers without JBIG2 support
picamatl scan.pdf --no-jbig2-bitonal-images

# Keep charts and diagrams legible at a higher DPI than photographic content
picamatl paper.pdf --figure-dpi 195
```

Every flag maps 1:1 to an `OptimizeOptions` builder method — with one spelling
exception: `--allow-lossy` drives `with_allow_lossy_reencode()`. All defaults —
including the boolean flags' on/off state shown under `--help` — are read from
`OptimizeOptions::default()` at runtime, so the CLI can never drift from the
library. Non-PDF input is rejected by header sniffing; without `-o` or
`--force` the input file is never overwritten. On success the CLI prints
input → output size, percent saved, and elapsed time — on **stderr**, like
every other message it emits; stdout carries only `--help` and `--version`.

## Measured results

All numbers in this section were measured on 2026-11-17 with a release build
of `main` at `4e85845` and Ghostscript 10.08.0; the commands are in
[`docs/hunts/docs-refresh-notes.md`](docs/hunts/docs-refresh-notes.md).

On the committed synthetic fixture (`fixtures/sample.pdf`, four pages as of
0.2.0 — two JPEG pages and two FlateDecode pages, reproducible via
`scripts/bench-vs-gs.sh`): picamatl (strip, no packing) takes 662,107 bytes to
117,398 bytes (**17.7% of input**), downsampling the over-resolution JPEG *and*
Flate images while leaving both under-resolution pages byte-for-byte alone.
Ghostscript at the same mirrored settings takes the four-page fixture
to 71,742 bytes (10.8%): its forced `DCTEncode` re-encodes the two Flate pages
as JPEG, and the fixture's Flate content is synthetic noise — the class JPEG
compresses best and Flate compresses worst — so the gap overstates the
typical case. Picamatl keeps Flate images Flate by design (see Scope).

On the previous JPEG-only two-page fixture (the 0.1.0 `fixtures/sample.pdf`,
still in git history at `54872f9`; Ghostscript at mirrored settings — 130 DPI,
1.15 threshold, DCTEncode QFactor 0.4):

| Pipeline | Bytes | % of input |
| --- | ---: | ---: |
| input | 193,668 | 100% |
| **picamatl** (strip, no packing) | **19,889** | **10.3%** |
| Ghostscript pdfwrite | 43,725 | 22.6% |

On a public real-world document —
[NASA TM-20210010291](https://ntrs.nasa.gov/citations/20210010291), a 16.8 MB
58-page technical report — picamatl at defaults takes 16,804,107 bytes
to **4,297,986 bytes, a 74.4% reduction**, byte-stable under repeated
optimization (0.4.0 released at 4,443,883). Ghostscript's `/ebook` preset
produces 4,931,396 bytes — picamatl is **12.8% smaller**, stays lossless, and
keeps the accessibility structure tree. With explicit consent
(`--allow-lossy`) picamatl reaches 3,254,078 bytes, smaller than
Ghostscript's forced 130-DPI + `DCTEncode` lane, without ever re-encoding
without consent.

### NASA TM-20210010291 — flag comparison

| Pipeline | Bytes | % of input | Notes |
| --- | ---: | ---: | --- |
| input | 16,804,107 | 100% | 58-page technical report, tagged |
| Ghostscript `/ebook` | 4,931,396 | 29.3% | stores some of the file's lossless images as JPEG (60 → 48 non-JPEG images); strips accessibility; AGPL |
| **picamatl defaults** (lossless-only) | **4,297,986** | **25.6%** | every image class handled; fonts subset; no encoding-class changes |
| picamatl `--allow-lossy` q78 | 3,254,078 | 19.4% | + explicit consent: Flate photos → JPEG (incl. masked pairs), line art auto-declined |
| Ghostscript forced 130 DPI + DCT¹ | 3,691,333 | 22.0% | re-encodes *all* imagery incl. line art; strips accessibility; AGPL |
| picamatl kitchen sink² | 2,945,043 | 17.5% | every opt-in flag, accessibility stripped |

¹ The `gs` lane of `scripts/bench-full.sh`. Ghostscript 10.07.1 gave
3,054,642 bytes (18.2%) with the mirrored settings in August 2026; 10.08.0
does not reproduce that number.
² The kitchen lane of `scripts/bench-full.sh`, the flags of the
consolidated table below.

What each level buys:

- **Defaults (no flags):** transparency-masked JPEG requantization and coupled
  downsampling (D-M1/D-M2), masked and unmasked Flate coupled downsampling
  (D-M3), palette (`/Indexed`) bases with a soft mask (D-M4), shared-mask and
  under-threshold requantization (Phase 6), a lossless Huffman re-table of
  every JPEG, font subsetting (Type0/CIDFontType2 Identity-H/V and
  nonsymbolic simple TrueType — rendering-preserving, text extraction
  bit-identical), content-stream number minification, lossless re-storing of
  legacy LZW, ASCII85, ASCIIHex and RunLength streams as Flate, lossless
  JBIG2 re-storing of CCITT scans (G3, G4 and byte-aligned G4), then a
  serialization pass — the best PNG row filter for every 8-bit Flate image,
  a final level-9 re-deflate of every Flate stream through the zlib-rs
  backend, object-stream packing (PDF 1.5) and a compressed cross-reference
  stream with a PNG Up predictor when that is smaller. No image goes from
  lossless to lossy (a CCITT image that becomes JBIG2 keeps every sample);
  soft masks are never resized when another image shares them.
- **`--allow-lossy`:** additionally re-encodes *unmasked photographic-looking*
  FlateDecode images as JPEG at the configured quality. A built-in content
  heuristic declines line-art-like images (thin lines pick up visible JPEG
  mottling for marginal savings) and the flag never changes geometry the
  lossless path declined to change — one consent covers re-encoding only.
  Measured effect on this report: −24.3% over defaults, with converted
  streams visually indistinguishable at review zoom.
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
packet set), rendered at 150 dpi and compared with the original page by page
(`scripts/forms-verify.py`, pixels differing by more than 8/255):

| Pipeline | Size | of input | Form? | Pages differing from the original render |
| --- | ---: | ---: | --- | ---: |
| input | 2,150,352 | 100.0% | interactive | — |
| picamatl defaults | 1,369,502 | 63.7% | interactive | 1 of 11¹ |
| picamatl defaults + `--flatten-forms` | 229,153 | 10.7% | static | 1 of 11¹ |
| picamatl kitchen sink + `--flatten-forms` | **124,220** | **5.8%** | static | 1 of 11² |
| Ghostscript `/ebook` | 183,761 | 8.5% | removed | **11 of 11**, up to 0.56% of pixels |

¹ Page 1 only (0.54% of its pixels), all inside the 300-ppi header image
that defaults downsample to 130 ppi; the other ten pages are pixel-identical.
² Page 1 only: the same header image, plus glyph edges from
`--strip-hinting` (1.34% of the page).

Without `--flatten-forms` even the kitchen sink leaves this file at 57.4%,
because the XFA packets stay. With it, picamatl's output is smaller than
Ghostscript's, and at defaults + `--flatten-forms` the only thing that
renders differently is the one image picamatl downsamples.

`scripts/forms-vs-gs.sh` shows the contract earning its keep on a real filled
dynamic XFA form (`xfa_filled_imm1344e.pdf`, a Canadian IMM 1344E from the
pdf.js corpus): a static-flattening pipeline (Ghostscript `/ebook`) turns
3,023,968 bytes into 4,157 bytes of *"Please wait…"* placeholder page and
takes all **9,298 filled data nodes** with it. picamatl declines that
document — and still shrinks it 88.7% losslessly (343,055 bytes), with every
data node intact, because the XFA packets were stored undeflated.

Rendered at 150 dpi, all 9 pages of `census-brief.pdf`, the corpus's other
AcroForm file, are pixel-identical to the original at defaults, with
`--flatten-forms`, and with the kitchen sink. On the other 14 corpus files —
the 12 with no `/AcroForm` and the two dynamic XFA forms it declines — the
flag is completely inert: byte-for-byte the same output with and without it.

For calibration against Ghostscript's `/ebook` preset (which strips the
structure tree and stores some images as JPEG): picamatl's lossless defaults
total 38.4% of input over the sixteen-file corpus below against `/ebook`'s
40.6%, and are smaller on 7 of the 16 files. With the kitchen sink picamatl
is smaller than `/ebook` on 12 of the 16; the exceptions are arxiv-attention
(42.9% vs 9.7%), cmyk-jpeg (3.2% vs 2.2%) and the two dynamic XFA forms.
Ghostscript's forced-lossy settings re-encode all lossless imagery
(including line art), rewrite fonts, and strip the structure tree — trades
picamatl makes only with consent.

The differentiators are the CTM-aware placement analysis, the fail-safe
contract, and the license.

### Five-document public corpus — full matrix (2026-11-17, `main` at `4e85845`)

A five-file public corpus covering a 756-page technical spec, an arXiv paper,
a 126-page IRS tax guide, an SSD framework document, and a tiny synthetic
file. Ghostscript 10.08.0 at mirrored lossy settings (130 DPI, 1.15
threshold, DCTEncode QFactor 0.4). All sizes are % of input; lower is better.
picamatl rows are cumulative consent levels:

| Pipeline | adobe-spec | arxiv | irs-1040gi | nist-ssdf | dummy | TOTAL |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| input | 22,491,828 B | 2,233,053 B | 4,434,643 B | 739,891 B | 13,264 B | 29,912,679 B |
| **picamatl lossless** (defaults) | 30.5% | 65.8% | 58.6% | 75.2% | 93.9% | **38.4%** |
| picamatl `--allow-lossy` | 30.5% | 65.8% | 58.6% | 75.2% | 93.9% | 38.4% |
| + `--strip-accessibility` | 23.8% | 65.8% | 38.0% | 69.9% | 93.9% | 30.2% |
| kitchen sink¹ | 18.9% | 52.9% | 31.4% | 47.4% | 88.6% | **24.0%** |
| Ghostscript forced lossy² | 48.8% | 55.5% | 72.2% | 82.1% | 114.1% | 53.6% |

¹ `--allow-lossy --strip-accessibility --strip-metadata --convert-type1
--strip-hinting --recompress-bitonal-images --collapse-gray-images
--deflate-backend zopfli` (the consolidated table below adds
`--strip-private-data` and `--flatten-forms`)
² `-dDownsampleColorImages/GrayImages -d*ImageResolution=130
-dColorImageFilter=/DCTEncode` with QFactor 0.4 (the `gs` lane of
`scripts/bench-full.sh`).

Reading the matrix honestly:

- **At full throttle picamatl is smaller on every file** and grows none. The
  kitchen-sink run costs far more CPU (zopfli is the bulk of it; adobe-spec
  took about 115 s in this run against 3.9 s at defaults), a deliberate trade
  under its own flag.
- **On this corpus `--allow-lossy` alone is a no-op**: none of the five files
  contain qualifying lossless-Flate photographic images, so the row equals
  defaults by design — the flag only ever fires on content that matches its
  heuristic. The NASA report above shows what it does on photo-heavy input.
- **At defaults picamatl is already smaller than the forced-lossy Ghostscript
  lane on four of the five files.** The exception is arxiv-attention (65.8%
  vs 55.5%), whose remaining bytes are mostly fonts and figure authoring data:
  the kitchen sink takes it to 52.9%, and `--strip-private-data` (the
  consolidated table) to 42.9%.
- **Since the 0.4.4 release** the lossless defaults gained the legacy-filter
  re-store, palette-pair downsampling, the PNG row-filter choice, the
  predicted cross-reference stream and the CCITT → JBIG2 re-store: the 0.4.4
  binary takes these five files to 40.0% of input, `main` to 38.4%.

### Six-document expanded corpus — industry & edge-case sweep (2026-11-17)

A second corpus targeting document classes absent from the first:
academic preprint (arXiv 2303.08774), census statistical brief (government
charts), a CMYK-JPEG edge-case file (from Mozilla's pdf.js test suite), the
official IRS W-2 fillable form (**XFA/LiveCycle** — see below), the NIST
SP 800-63B standard, and a Wikipedia article render. Same settings and
footnotes as above:

| Pipeline | arxiv-gpt4 | census | cmyk-jpeg | irs-w2 | nist-sp800 | wiki | TOTAL |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| input | 5,245,564 B | 545,684 B | 374,080 B | 2,150,352 B | 1,480,377 B | 2,196,261 B | 11,992,318 B |
| **picamatl lossless** (defaults) | 37.9% | 51.2% | 78.1% | 63.7% | 58.0% | 44.4% | **48.1%** |
| picamatl `--allow-lossy` | 31.3% | 51.2% | 78.1% | 63.7% | 57.6% | 30.8% | 42.6% |
| + `--strip-accessibility` | 31.3% | 43.2% | 78.1% | 60.6% | 53.1% | 25.9% | 40.3% |
| kitchen sink¹ | 16.6% | 35.9% | 67.4% | 57.4% | 39.6% | 22.7% | **30.3%** |
| Ghostscript forced lossy² | 28.4% | 60.0% | 4.6% | 8.8% | 70.1% | 205.3%³ | 63.1% |

³ Larger than the input with Ghostscript 10.08.0; 10.07.1 gave 37.1% with
the same command in August 2026.

Findings from this sweep:

- **The IRS W-2 is an XFA form.** 1.58 MB of its bytes are a compressed XML
  Forms Architecture template that *describes* the form and which Acrobat
  renders dynamically. Without `--flatten-forms`, picamatl leaves the XFA
  model alone (rewriting XFA without Adobe's engine is how forms break), so
  even its kitchen-sink row stays at 57.4%. The comparison row's smaller
  number comes from output that is no longer a fillable form. For this class
  of document the choice without the flag is "64% of original, still works"
  vs "9%, no longer a form"; with `--flatten-forms` picamatl makes the static
  copy itself, at 5.8% (see [Interactive forms](#interactive-forms)).
- **cmyk-jpeg is the mirror image:** a 374 KB file whose payload is one large
  CMYK JPEG. Aggressive lossy re-compression takes that single image down to
  4.6%; picamatl re-encodes it as YCCK at `jpeg_quality`, which is a far more
  conservative trade, so 67.4% is its floor here — until
  `--strip-private-data` (below) removes the Illustrator data that is most of
  the remaining file.

  These two cells got *worse* than the numbers first published for this
  corpus (76.7% and 65.9%), and the correction is worth stating plainly: the
  old numbers were bought with a **corrupt page**. The pre-0.3.2 CMYK
  "fallback" did not decline as the text here claimed — it decoded the image
  to RGB and wrote three channels back under a `/ColorSpace` that still said
  `/DeviceCMYK`. Rendered through Ghostscript, the old output differed from
  the original by up to 251 levels across 5.8% of the page, exactly the image
  bounding box. Full analysis and the cross-decoder verification are in
  [`docs/CMYK-JPEG.md`](docs/CMYK-JPEG.md).
- **On the other four picamatl's kitchen sink is smaller:** arXiv preprint
  (16.6% vs 28.4%), census brief (35.9% vs 60.0% — vector charts plus fonts
  respond to subsetting and Type1→CFF conversion), NIST SP 800-63B (39.6% vs
  70.1%), and the Wikipedia render (22.7%; `--allow-lossy` alone takes it from
  44.4% to 30.8%; footnote 3 on the Ghostscript cell).
- Combined across both corpora, picamatl's kitchen-sink configuration is
  smaller on **9 of 11 documents**, never grows a file, keeps dates and
  provenance unless explicitly stripped, and preserves accessibility by
  default.

### Sixteen-document consolidated matrix (2026-11-17, `main` at `4e85845`)

The full corpus — both earlier sweeps plus the XFA fixtures added with
`--flatten-forms`. Kitchen sink now includes `--strip-private-data`
(drops `/PieceInfo` authoring data) and `--flatten-forms`:

| file | input | picamatl kitchen¹ | gs lossy² | winner |
| --- | ---: | ---: | ---: | --- |
| adobe-spec | 22,491,828 B | 18.9% | 48.8% | 🏆 picamatl |
| arxiv-attention | 2,233,053 B | 42.9% | 55.5% | 🏆 picamatl |
| dummy | 13,264 B | 88.6% | 114.1% | 🏆 picamatl |
| irs-1040gi | 4,434,643 B | 31.4% | 72.2% | 🏆 picamatl |
| nist-ssdf | 739,891 B | 47.4% | 82.1% | 🏆 picamatl |
| arxiv-diffusion | 10,267,274 B | 16.2% | 27.1% | 🏆 picamatl |
| arxiv-gpt4 | 5,245,564 B | 16.6% | 28.4% | 🏆 picamatl |
| census-brief | 545,684 B | 35.5% | 60.0% | 🏆 picamatl |
| cmyk-jpeg | 374,080 B | 3.2% | 4.6% | 🏆 picamatl |
| **irs-w2** | 2,150,352 B | **5.8%** | 8.8% | 🏆 picamatl |
| nist-sp800-63b | 1,480,377 B | 39.6% | 70.1% | 🏆 picamatl |
| pypdf-cmyk | 443,953 B | 94.2% | 101.1% | 🏆 picamatl |
| wiki-cmyk-topic | 544,864 B | 42.5% | 494.4%⁴ | 🏆 picamatl |
| wiki-pdf | 2,196,261 B | 22.7% | 205.3%⁴ | 🏆 picamatl |
| xfa_filled (dynamic) | 3,023,968 B | 10.4% | 0.2%³ | not comparable³ |
| xfa_issue14315 (dynamic) | 11,568 B | 52.6% | 28.8%³ | not comparable³ |
| **TOTAL** | **56,196,624 B** | **21.1%** | 52.6%⁴ | 🏆 **picamatl** |

¹ `--allow-lossy --strip-accessibility --strip-metadata --strip-private-data
--convert-type1 --strip-hinting --recompress-bitonal-images
--collapse-gray-images --flatten-forms --deflate-backend zopfli` (the
kitchen lane of `scripts/bench-full.sh`)
² Ghostscript 10.08.0, mirrored settings (130 DPI / QFactor 0.4).
³ gs's number on dynamic XFA is a 4 KB "Please wait…" placeholder — the
filled form data is destroyed. picamatl declines the XFA model (no static
fallback), compresses everything else, and keeps the data, so the two
results are not comparable.
⁴ Ghostscript 10.08.0's output is larger than the input on the two
Chrome-printed Wikipedia files, and on wiki-pdf it prints "Cannot use
DeviceIndependentColor for a Blending colour space" while writing it;
10.07.1 gave 37.1% and 65.8% with the same
command in August 2026. Without those two files the totals are 20.9%
(picamatl) and 41.8% (gs).

Reading the consolidated matrix:

- **Every non-dynamic-XFA file is a picamatl win.** Leaving out the two
  files where Ghostscript 10.08.0's output grew (footnote 4), the total is
  **20.9% of input vs gs's 41.8%**; with them it is 21.1% vs 52.6%, a gap
  that flatters picamatl. picamatl also wins both of those files against
  10.07.1's August numbers. Form data and provenance are kept unless
  explicitly stripped.
- **irs-w2 flips with `--flatten-forms`**: 2.15 MB → 121 KB (5.8%) — and
  the flattened output keeps the filled values. The AcroForm flatten path is
  pixel-fidelity-verified (see `docs/FORMS-PLAN.md`).
- **cmyk-jpeg beats gs** (3.2% vs 4.6%) because `--strip-private-data` drops
  the 261 KB of Illustrator authoring data; the YCCK re-encode alone stops at
  67.4%.
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
- **Little size upside on the measured corpus.** On the two-page JPEG
  fixture picamatl's re-encoded JPEG payload is less than half of
  Ghostscript's at the same pixel dimensions. Against Ghostscript's
  forced-lossy settings, picamatl's lossless defaults are smaller on 9 of the
  16 corpus files, and with matching consent (the kitchen sink) on every
  file except the two dynamic XFA forms, whose small Ghostscript outputs no
  longer contain the form. Against the `/ebook` preset the kitchen sink is
  smaller on 12 of 16 (see the calibration note above).

## Scope

Picamatl optimizes `DCTDecode` (JPEG) images — baseline, extended *and*
progressive, since 0.4.0 — in RGB, grayscale, or CMYK/YCCK (four-component
payloads are resampled in native CMYK space and re-encoded as YCCK, never
converted to RGB; streams carrying a `/Decode` array are declined — see
[`docs/CMYK-JPEG.md`](docs/CMYK-JPEG.md)), and — since 0.2.0 — 8-bit
`FlateDecode` raster images in DeviceRGB/DeviceGray/ICCBased(N=1,3),
downsampled in place with the format and color space preserved.

**Soft-masked images are handled, not skipped.** An over-resolution base and
its `/SMask` are downsampled as an atomic pair to the same target geometry —
both replaced or neither — for JPEG bases (D-M2) and Flate bases (D-M3),
including 8-bit palette (`/Indexed`) Flate bases (D-M4), whose palette is
expanded before resampling so no in-between index is ever invented (a
palette pair that is text drawn as an image — one flat color, letter shapes
in a hard-edged mask — stays at full resolution). A
masked JPEG pair already at or below the target instead takes the
dimension-preserving requantization (D-M1): the base is re-encoded at its own
dimensions and the mask is not touched at all. All of them require the mask to
be a plain 8-bit DeviceGray image stream with no `/Matte`, and a mask shared
by more than one base is never resized. `/Mask` (stencil/color-key) images and
ineligible `/SMask`s stay untouched.

**Bitonal scans are re-stored losslessly, by default.** In a file picamatl
rewrites, every CCITT image — G4 (`/K<0`, including `/EncodedByteAlign`
streams, decoded by picamatl's own strict T.6 decoder) and EOL-framed G3 —
is coded again as one lossless JBIG2 generic region by picamatl's own
encoder, and the new stream ships only when it is strictly smaller and
decodes back to exactly the same samples. `/Decode`, `/ColorSpace` and
`/ImageMask` are untouched, so the page paints identically. The output then
needs a JBIG2-capable reader (PDF 1.4); `--no-jbig2-bitonal-images` keeps
CCITT images as stored. `--recompress-bitonal-images` (opt-in since 0.4.0)
adds a CCITT G4 candidate and extends the re-store to 1-bit images stored as
Flate; the smaller lossless candidate wins. The JBIG2 re-store skips
encrypted documents. **`JPXDecode` (JPEG 2000)** streams are converted to `DCTDecode`
under `--allow-lossy`, via a pure-Rust decoder; any header or decode failure
leaves the stream byte-identical.

What stays untouched, deliberately: Flate images that are Indexed (other
than the soft-masked pairs above), 2/4/16-bit, `/Decode`-remapped, or
TIFF-predicted, and 1-bit ones unless `--recompress-bitonal-images` is on;
byte-aligned G3 (`/K >= 0` with `/EncodedByteAlign`); and **every image
already stored as JBIG2** — picamatl never re-encodes one, and never writes
symbol-mode JBIG2 (the standing commitment above).

**Files that claim PDF/A** keep their claim valid: a PDF/A-1 file is saved
with a classic cross-reference table and no object streams, and its catalog
XMP keeps its exact bytes. The font passes, the content minifier, the
legacy-filter re-store, the JPEG Huffman re-table, the PNG row-filter choice
and form flattening skip these files, and the final re-deflate touches only
their page content streams. The lossless JBIG2 re-store, image downsampling
and the opt-in strips run as on any other file (`--strip-metadata` removes
the claim itself).

On text-heavy PDFs with default options picamatl will honestly do very little,
and by contract it returns the input unchanged rather than a worse file.

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

- **lopdf 0.47 with `default-features = false`**: keeps lopdf's datetime
  backends (`chrono`/`jiff`/`time`) and its `rayon` parsing out of the
  tree — picamatl never parses PDF datetimes (Info-dict date strings pass
  through as opaque bytes), so none of those features are needed. Re-checked
  2026-11-17: no datetime crate in `cargo tree`. (Turning them off at the
  0.42 → 0.44 bump left output byte-identical and the binary ~71 KB
  smaller.)
- **Build-time requirements:** mozjpeg compiles libjpeg-turbo, which needs
  NASM and a C compiler at build time (CI uses `ilammy/setup-nasm`). There is
  no native *runtime* dependency.
- **MSRV: 1.88.0**, enforced by a dedicated CI job. The floor is
  dependency-driven: lopdf 0.47 and `image` 0.25.10 both declare
  `rust-version = 1.88`.
- mozjpeg's IJG license requires an attribution notice in documentation
  accompanying *binary* distributions. [`NOTICE`](NOTICE) carries it; it is
  committed at the repository root, included in the crates.io package, and
  shipped in the release archives for all five prebuilt targets.

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
compiled into any release binary. The font-encoding tables in
`src/encodings.rs` are the ISO 32000-1 Annex D tables plus Adobe's
BSD-licensed Adobe Glyph List, generated from those public sources.
