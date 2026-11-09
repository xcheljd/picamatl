# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Font subsetting now tracks the text font through `q`/`Q`.** The font is
  part of the graphics state, so text shown after a `Q` is drawn in the font
  that was current at the matching `q`. The glyph walk credited it to the
  last `Tf` instead, and subsetting could drop glyphs the restored font
  really draws.
- **Signature detection and form flattening walk each field node once.** A
  field tree that reaches one node through many paths (`/Kids [A A]` at
  every level) was walked once per path — 2^32 visits from 33 small objects —
  and signature detection runs on every input. Flattening declines such a
  tree.
- **The glyph walk and the image-placement walk each have a total budget.**
  Both re-walk a form per use (the glyph walk when the form inherits its
  font, the placement walk because the drawn size depends on each draw), so
  a short chain of forms each drawing the next one several times demanded
  billions of walks; depth caps bound depth, not width. Past the budget the
  glyph walk gives up and no font is touched; the placement walk discards
  its map and every image is left alone.
- **The JPEG Huffman pass declines frames whose header declares an absurd
  size** before sizing any per-block state from it. A 30-byte progressive
  header declaring 65535 × 65535 made it allocate about 2 GB; under a memory
  limit that aborts the process, which the fail-safe cannot catch.
- **TrueType table parsing bounds the bytes it copies by the font's size**,
  so a table directory that names one range 65535 times no longer copies it
  65535 times.
- **The lossless JPEG table rewrite runs under a 1 GiB memory budget.**
  Every JPEG was re-tabled at once, each needing up to ~72× its size, so a
  32 MB PDF of eight photos peaked at 2.2 GB. JPEGs now run in parallel
  batches that fit the budget (877 MiB peak on the same file, output
  byte-identical). Nothing is skipped: a JPEG too large for the budget on
  its own runs by itself.
- **Type1 → CFF conversion no longer copies a subroutine on every call.**
  `callsubr` cloned the whole subroutine each time, so a large subroutine
  that returns immediately, called up to the per-glyph token budget, meant
  hundreds of GB of copying for one glyph (`--convert-type1` only).
- **`--collapse-gray-images` skips zero-width or zero-height images.** A
  zero-width RGB image with an empty Flate stream passed every length check
  and then made the PNG filter panic on zero-byte rows; the fail-safe
  caught it, but returned the whole document unoptimized.
- **Form flattening reads an orphan widget's inherited value.** A widget no
  `/Fields` entry reaches was checked only for its own `/V`; a value
  inherited from an equally unreachable `/Parent` (a malformed tree) was
  dropped with the widget instead of declining.
- **Hint stripping zeroes `maxSizeOfInstructions`**, not the adjacent
  `maxStackElements`, in `maxp`.
- **Form flattening declines a page whose `/XObject` resources already use
  the `AmXf` burn-name prefix**, instead of overwriting the entry.
- Glyph-name interning in the Type1 → CFF converter and the Type1C merge is
  hashed rather than a linear search per glyph (quadratic in the glyph count
  of an untrusted font).
- The line-art classifier's documented edge threshold (8%) matches the code;
  its metrics function also declines a pixel buffer shorter than its stated
  dimensions instead of indexing past it (callers already size buffers
  exactly; defensive).
- **Unmasked JPEG downsampling now verifies its output by decoding it back**,
  like every other re-encode path (ARCHITECTURE.md invariant 5). The
  unverified copy of the routine is removed; the masked path's verified one
  now serves both.
- **Bounded page-content inflate in the image-placement walk.** A page
  whose content stream inflated past the 128 MiB budget was decompressed
  without limit; it is now skipped (its images are left untouched), so a
  small crafted file can no longer exhaust memory there.
- **The decoded-payload dedup no longer holds every decoded stream in
  memory at once.** It buckets by length + hash and re-inflates only
  candidate duplicates for a byte-for-byte comparison before merging.
- **`--force` writes atomically**: the output goes to a sibling temporary
  file that is synced and renamed over the input, so a crash, full disk or
  kill mid-write can no longer leave the input truncated.
- **Signature detection** now reports a merged field/widget signature
  dictionary stored directly (not by reference) in `/Fields` or `/Kids`.
- `--help` no longer claims the CLI expands quoted glob patterns; it never
  did (shell globs still work as usual).
- **The same first-pass font fix now covers CID (Type0) subsetting and
  Type1 → Type1C conversion.** Both planned per font or per descriptor, so a
  program the stream dedup had merged from byte-identical copies was
  declined and only handled on a second run. They now plan per program,
  like simple TrueType. The CID size guard also counts an old
  `/CIDToGIDMap` stream as freed only when nothing outside the rewrite
  still references it, and counts a shared one once.
- **A second pass could shrink a file further** when a document embedded
  the same simple TrueType font program more than once (one copy per page
  or section, as some producers write). The stream dedup merged the copies
  before font subsetting, the subsetter then declined a program shared by
  several font dictionaries, and a later dedup merged those dictionaries —
  so only the *next* run subset the font. Simple TrueType fonts are now
  planned per font program: when every font dictionary and descriptor that
  references the program is one whose usage was attributed, their glyph
  sets union into one subset on the first pass. Measured on
  `corpus-expanded/arxiv-gpt4.pdf`: the first pass now lands where the
  second used to (defaults 2,001,418 → 2,001,370 B; with every lossy and
  strip flag on, 874,428 → 870,171 B) and a second pass is byte-stable.
  Every other corpus file's output is byte-identical to before the fix.

### Changed

- **Streams stored with legacy filters are re-stored as Flate.**
  `/LZWDecode` (common in Acrobat Capture-era scans), `/ASCII85Decode`,
  `/ASCIIHexDecode` and `/RunLengthDecode` streams, and chains of them that
  may end in Flate, are decoded and stored again as one `/FlateDecode`
  stream when that is strictly smaller, dictionary text included. Lossless:
  every candidate is inflated back and compared with the decoded bytes,
  and a predictor in `/DecodeParms` carries over unchanged. The decoders are
  strict and capped — a malformed stream, an LZW table that overflows
  without a clear code, or output past the 128 MiB cap leaves the stream as
  it was. Images are re-stored only after the image passes have run, so an
  LZW image is never newly downsampled. Encrypted and PDF/A-declared
  documents are left alone, unless `--strip-metadata` drops the PDF/A
  claim. Over 15 public scans, default outputs shrink by 391,551 B in
  total: gao-104000, which picamatl used to return unchanged, goes from
  5.28 MB to 4.89 MB (7.3%), and fbi-custodial saves 4,987 B; every other
  scan's default output is byte-identical. The 17-file corpus has no LZW;
  its only change is cmyk-jpeg, whose two ASCII85+Flate streams lose their
  text layer (226 B), and every other corpus output is byte-identical.
- **Over-resolution palette (`/Indexed`) images with a soft mask are
  downsampled.** They were left at full size, because the Flate downsample
  cannot resample palette indices. The palette is now expanded into its
  base color space first (DeviceGray, DeviceRGB, or ICCBased with one or
  three components — the expansion is exact for these), the pixels are
  resampled with the base and its `/SMask` as one unit, and the base ships
  either re-indexed into its own palette (only when every resampled pixel is
  exactly a palette color) or in the base color space, whichever is smaller;
  with `--allow-lossy`, a JPEG competes too. Indices are never interpolated,
  every candidate is decoded back before it ships, and the pair is still
  replaced only when it saves at least 5%. Other base spaces, a `/Decode`
  array, or a `/Matte` mask leave the pair untouched, as before. Text drawn
  as an image — one flat color whose letter shapes live in a hard-edged
  mask, like the nist-ssdf appendix headings and the nist-sp800-63b section
  numbers — also stays at full resolution, so it stays as crisp as the real
  text around it. Over the 17-file corpus, default outputs shrink by
  77,538 B (all in nasa, 1.7%), and by 74,913 B with every lossy and strip
  flag on; every other file's output is byte-identical. The
  `--figure-dpi` classifier now reads these images through their palette
  as well.
- **The cross-reference stream is written with a PNG Up predictor** when
  that makes the file smaller (`/DecodeParms<</Columns <W sum>/Predictor
  12>>`). Consecutive xref rows differ only in their low bytes, so filtering
  each row against the one above turns the table into long zero runs. Both
  encodings are built and decoded back; the predicted one ships only when
  the whole patched file is strictly smaller than the plain-deflate one, so
  no output grows. Lossless — only the table's encoding changes. Over the
  17-file corpus, default outputs shrink by 467,633 B in total (1.76%;
  irs-1040gi 5.71%, adobe-spec 3.40%); with `--deflate-backend zopfli`,
  which already finds most of that redundancy, by 19,017 B.
- **lopdf 0.44 → 0.47.** Upstream's three releases are mostly parser
  robustness for malformed and hostile files (crash fixes, cross-reference
  reconstruction, more stream filters, bounded decompression), plus faster
  parsing. Both upstream bugs picamatl works around are still present in 0.47
  (`Object::Real` stored as `f32`; the object-stream index written as `u16`),
  so the real-literal restoration pass and the 65,535-objects-per-stream cap
  stay. Two tests pinned old lopdf behavior and were updated: a canary for
  lopdf's `d0`/`d1` tokenizing (now correct upstream), and a forms fixture
  whose appearance stream is now built from integers so it stays in compact
  form under lopdf's new whole-number real printing.

## [0.4.4] - 2026-09-08

### Changed

- **Internal restructuring: `src/lib.rs` split from ~12,300 lines into five
  cohesive modules** (`signatures`, `cmyk`, `serialize`, `classify`,
  `deflate`), leaving the plan/apply pipeline intact in `lib.rs`. No public
  API or behavior change; `picamatl::SignatureInfo` / `SignatureKind` paths
  unchanged.
- Documentation repairs: seven fused doc-comments reattached to their own
  functions (including `try_optimize`'s block, which had been sitting on a
  type alias), a stale cross-reference fixed, and an orphaned duplicate line
  removed.

## [0.4.3] - 2026-09-07

### Changed

- Packaging and documentation polish only; no behavior change. The crates.io
  `description` and `categories` are rewritten (the binary now advertises
  itself as a CLI, and `command-line-utilities` is claimed), and an `exclude`
  list keeps development scaffolding — hunt scripts, hunt notes, spike
  examples, corpus manifests — out of the published tarball.
- Documentation corrections: the architecture doc's encoding-table provenance
  (ISO 32000-1 Annex D + Adobe AGL, never Ghostscript), the README scope
  section brought current with shipped features, the quick-start transcript
  matched to the real CLI output, and the changelog tail backfilled.
- CLI fixes: the multi-input error message's corrupted whitespace, and a
  single failed input no longer prints a meaningless second error line.
- The dependency lockfile no longer pins a yanked crate.

## [0.4.2] - 2026-09-02

### Added

- **Signature detection and warning.** A signed PDF's `/ByteRange` digest is
  invalidated by any re-serialization, and until now nothing at runtime said
  so. Picamatl now detects the signatures a run would invalidate and the CLI
  prints one stderr warning naming each of them and its object number.
  Detection covers:
  - the `/AcroForm` field tree, walked recursively through `/Kids` with
    `/FT` and `/V` inherited (ISO 32000-1 12.7.3.1: only leaf nodes must
    carry `/FT`), depth-guarded against cyclic `/Kids`;
  - merged field/widget nodes that carry `/ByteRange` themselves;
  - the catalog's `/Perms` `/UR3` (Reader-extended usage rights — the
    Reader-extended IRS form profile) and `/DocMDP` (certification) entries,
    whose values *are* the signature dictionaries per Table 258.

  Every reference along the way is resolved: `/AcroForm`, `/Fields`, `/FT`,
  `/V`, `/Kids`, `/Perms` and the `/Perms` entries may each be indirect.
  Findings are deduplicated by object id, so one signature reached through
  both `/AcroForm` and `/Perms` is reported once. An unsigned placeholder
  field — no `/V`, or a `/V` that is not a signature dictionary — is
  deliberately *not* reported: there is no digest to invalidate.

  New `--on-signature warn|silence` flag
  (`OptimizeOptions::on_signature`, `SignatureHandling::Warn` default /
  `Silence` for pre-filtering pipelines). `Silence` skips the walk entirely
  and costs nothing.

- **`optimize_with_report`**, returning `(Vec<u8>, OptimizeReport)` — the
  same bytes `optimize_with_options` returns, plus the `SignatureInfo`
  findings and whether the run rewrote the document or handed the input
  back. The library performs no I/O of its own: the `picamatl` binary owns
  the warning text. New public items: `OptimizeReport`, `SignatureInfo`,
  `SignatureKind`.

### Changed

- README signature paragraph updated to describe the runtime warning.
- `SignatureHandling` is now `#[non_exhaustive]`, matching
  `OptimizeOptions`, so a future variant is not a breaking change.
- The signature warning is emitted only when picamatl actually returned a
  rewritten document. A declined or non-shrinking run hands back the input
  byte for byte, invalidates nothing, and now says nothing — it used to
  print the warning and "wrote a byte-for-byte copy" on the same run.

## [0.4.1] - 2026-08-27

### Added

- **Batch output options.** `<INPUT>` now accepts a directory (its
  non-recursive `*.pdf` entries, sorted) in addition to a single path. Two new
  flags control where outputs go: `--output-dir <DIR>` (same file names, into
  one folder; created on demand) and `--suffix <SUFFIX>` (rename the stem, e.g.
  `report.pdf` → `report-min.pdf`, beside the input unless `--output-dir` is
  also given; combinable). Inputs are never consumed or overwritten by the
  batch forms. Batch semantics: one failing file does not lose the rest, but
  the run exits nonzero with a `N of M input(s) failed` summary.
- **First crates.io release** — `cargo install picamatl` (package 2.6 MiB
  after untracking 24 benchmark-output PDFs that predated the corpus
  ignore rule).

### Changed

- **Renamed `amatl` → `picamatl`** — every release through 0.4.0 built as
  `amatl`; the crates.io name was taken, and *pica-* (tiny) sharpens what the
  tool does to āmatl (fig-bark codex paper) anyway. Crate, binary, library
  and repository all move together. There is no `amatl` on crates.io, so no
  upgrade path is broken.
- README install section now shows only real channels.

## [0.4.0] - 2026-08-26

The "compression hunt" release: ~96 commits since 0.3.1, a wave of new
opt-in passes, several default-on behavioral changes, and a large correctness
sweep. README's "Measured results" and `docs/ARCHITECTURE.md` document the
pipeline in full.

### Added

- **`--figure-dpi <DPI>`** (opt-in quality knob, default off). A cheap
  pixel-statistic heuristic (background ∈ [0.25, 0.75) && edges ≥ 0.10, at
  full resolution across RGB/CMYK/Flate decode routes) classifies chart/
  diagram images carrying rendered text, and gives them this higher target DPI
  instead of `--target-dpi` — so zooming into figures stays legible while
  photographic content keeps compressing. Requires a positive `--target-dpi`.
  On NASA TM-20210010291: `--figure-dpi 195` → 5,404,689 B (67.8% saved) vs
  4,443,883 B defaults; 7 pages measurably closer to original, 0 worse.
  Designed as a quality knob, not a consent tier. Heuristic-only for now —
  the OCR/adaptive-text-height variant is documented as deliberately deferred
  (see the 2026-08-25 investigation).
- **`--flatten-forms`** (opt-in, off by default). Flattens interactive forms:
  burns each widget's appearance stream into the page content stream, then
  removes `/AcroForm`, the field tree, XFA packets, and `/Widget` annotations.
  Declines (13 documented rules in `docs/FORMS-PLAN.md`) unless every field's
  value is provably preserved — including dynamic XFA (`/NeedsRendering true`)
  and any value with no appearance to burn. On the IRS W-2: 1,392,531 → 250,229 B
  with all 11 pages pixel-identical; flips the file from a GS "win" to an
  picamatl win.
- **`--strip-private-data`** (opt-in). Removes every `/PieceInfo` page-piece
  dictionary (Illustrator/InDesign keep editable copies there). On
  `cmyk-jpeg.pdf`: 261,453 B — 96% of picamatl's output — removed, pixel-identical.
- **`--strip-metadata`** (opt-in). Drops every `/Metadata` (XMP) packet;
  breaks PDF/A and PDF/UA identification. adobe-spec: 134 packets, 860 KB.
- **`--strip-hinting`** (opt-in). Now covers Type1C (CFF) programs too, not
  just TrueType — strips Type2 hint operators + Private DICT hinting params,
  outline-verified glyph by glyph.
- **`--collapse-gray-images`** (opt-in). Lossless RGB→Gray collapse for
  images whose RGB channels are identical.
- **`--recompress-bitonal-images`** (opt-in). CCITT G4 recompression for
  bitonal scans (lossless, decode-back verified).
- **`--convert-type1`** (opt-in). Type1 → Type1C (CFF) conversion, planned per
  `/FontDescriptor` (not per font dict), union-merging sibling font variants;
  plus the dvips 3-arg flex other-subr protocol fix.
- **Type1C subset union merge** (under `subset_fonts`): lossless merge of
  same-family Type1C subsets sharing a descriptor (semantic Private compare).
- **Progressive JPEG support** in the Huffman-table re-optimizer — the last
  outright decline in the entropy-recode path is closed (baseline + extended +
  progressive).
- **JPXDecode → DCTDecode conversion** (opt-in, under `--allow-lossy`), via a
  pure-Rust JPEG2000 decoder; header/decode failure leaves the stream untouched.
- **RGB→Gray collapse** (opt-in).
- **`docs/ARCHITECTURE.md`** — the design map (invariants, pipeline order,
  source layout, consent ladder, test guide).

### Changed

- **Font subsetting is now ON by default** (was opt-in since 0.2.1). Covers
  Type0/CIDFontType2 Identity-H/V and nonsymbolic simple TrueType; never
  rewrites content-stream text bytes; text extraction stays bit-identical.
- **Object-stream packing is ON by default** (was off through 0.3.0).
  Lossless, but puts a PDF 1.5 floor on output (pre-Acrobat-6 readers can't
  open ObjStm). Opt out with `--no-pack-object-streams`.
- **Deflate backend switched to zlib-rs** (pure Rust) — better ratios at the
  same level 9; a `--deflate-backend zopfli` option adds an exhaustive-search
  pass for the final re-deflate + xref (≈30× CPU for ~1% more).
- **The signature guard on the entropy-level passes is gone.** It protected
  nothing (picamatl re-serializes the whole doc, so `/ByteRange` digests were
  already invalid); removing it is worth ~507 KB on the Reader-extended IRS
  form. README now states plainly: optimizing a signed PDF invalidates its
  signature.
- **CMYK/YCCK JPEGs now take a dedicated end-to-end path** — decode to raw
  CMYK samples, resample all four channels with Lanczos3, re-encode as YCCK
  with the Adobe APP14 marker written by libjpeg. Fixes a real corruption bug
  (the old fallback wrote 3 RGB channels under an unchanged `/DeviceCMYK`
  `/ColorSpace`), and requires a dict/payload component-count cross-check.
- **lopdf 0.42 → 0.44 with `default-features = false`** — drops the
  chrono/jiff/time datetime backends and lopdf's rayon; output byte-identical,
  binary ~71 KB smaller.
- **Real literals never change value** — the f32-rounding drift (LibreOffice
  `/MediaBox [0 0 595.91998 841.91998]` → `595.92 841.92`) is fixed by splicing
  the input's own literals back into the saved file (value-keyed). 27/28 pages
  of wiki-pdf rendered differently before; 0/28 now.

### Fixed

- Every full-raster decode now caps against a 256 MiB ceiling *before*
  allocation; unreadable headers and overflowing geometry declines instead of
  panicking (decompression-bomb / OOM hardening, pinned by tests).
- Decoded-payload stream dedup must key on the dictionary too (a cross-zlib
  false merge was possible).
- zopfli re-deflates every ObjStm, not just a lone one.
- ObjStm object count capped at 65535 — lopdf's xref stream truncates
  type-2 indices to u16 (upstream issue reported).
- Stale xref trailer keys (`DecodeParms`, `Filter`, `Prev`, `XRefStm`,
  `Length`) stripped from the trailer.
- Type3 `d0`/`d1` glyph-metric operators no longer abort the whole font plan.
- Form flattening declines pages whose content ends inside an open text
  object, and honors the `/DR`-dependent appearance rule.
- Clippy 1.98's `chunks_exact_to_as_chunks` lint satisfied at 4 sites (CI
  was red on every push until this landed).
- JPEG Huffman re-optimization skips nothing: progressive scan models added.
- `--convert-type1` flex handling accepts the dvips 3-arg protocol.
- Masked-base lossy reach for `--allow-lossy` settles in one pass (one-pass
  idempotence for masked Flate pairs restored).

## [0.3.1] - 2026-08-22

Three lossless serialization wins, no consent flag involved: on the 58-page
NASA reference at otherwise-default settings, 4,958,148 → 4,655,752 B
(−302,396 B, −6.10%), byte-identical on a second pass.

### Added

- **Final re-deflate pass** (on by default). Every stream whose `/Filter` is
  exactly `FlateDecode` is inflated and re-deflated at level 9, kept only when
  strictly smaller *and* byte-identical on inflate-back. Reaches streams a
  producer's weaker deflate left behind, which `doc.compress()` never touched.
  Declined wholesale for encrypted, PDF/A-declaring and signed documents.
- **Compressed cross-reference stream.** lopdf hardcodes
  `XRefStreamFilter::None` and synthesizes the xref object during save, so no
  document-level pass could reach it; it is now deflated in both save paths
  (NASA packed 17,486 → 5,804 B). Any structural mismatch — including a
  classic cross-reference table — passes the file through untouched.

### Changed

- **Default flip:** `pack_object_streams` now defaults to `true` (it was
  `false` through 0.3.0). Lossless, measured −163,394 B (3.29%) on NASA. The
  cost is a **PDF 1.5 floor** — a reader older than Acrobat 6 cannot open an
  `ObjStm` file at all. Opt out with `--no-pack-object-streams`.

### Note

The serialization-time passes are not counted as "work" by `try_optimize`'s
early return: a document where nothing semantic was planned still comes back
byte-for-byte identical.

## [0.3.0] - 2026-08-22

### Added

- **Consent-gated lossy Flate→JPEG re-encode** behind the new default-off
  `--allow-lossy` / `OptimizeOptions::allow_lossy_reencode`. Unmasked 8-bit
  DeviceGray/DeviceRGB/ICCBased(N=1,3) Flate images may change encoding class
  to `/DCTDecode`; Indexed, CMYK, non-8-bit and `/SMask`-carrying bases never
  are. Two review-driven guards ship with it: `looks_like_line_art` declines
  thin-line vector-style content (which JPEG mottles for marginal gain), and
  a Flate candidate the never-larger guard would reject now disqualifies the
  JPEG candidate too — consent to re-encode is not consent to re-litigate a
  resampling decision the lossless path already rejected.
- **Requantization reach extensions (Phase 6).** Two JPEG classes the Phase 5
  pipeline never reached now take the dimension-preserving requantization:
  bases whose `/SMask` is shared by several images (P-M1 — the refcount
  fail-safe now blocks resizing only, the one transform that can misalign
  other consumers), and payloads at or below the DPI threshold, normalized in
  place instead of left at scanner grade forever (P-M2).

### Fixed

- **Requantization is now exactly idempotent.** A candidate whose JPEG
  quantization tables are byte-identical to the source's is declined: same
  tables mean the payload is already at the configured quality, so a
  re-encode is pure generation loss. The 5% rule alone did not converge once
  the pipeline started re-reading its own q78 output.

## [0.2.2] - 2026-08-22

### Added

- **Soft-mask-coupled downsampling (Phase 5 D-M1/D-M2/D-M3)** — the masked-image
  work that the README's headline numbers rest on. An over-resolution base and
  its eligible `/SMask` are resampled to the *same* target geometry and
  replaced together or not at all, for JPEG bases (D-M2) and Flate bases
  (D-M3); a masked JPEG pair at or below the threshold instead takes the
  dimension-preserving requantization, which never resizes and never touches
  the mask stream (D-M1). Eligibility is narrow by design — plain 8-bit
  DeviceGray mask, no `/Matte`, not a stencil — and a mask shared by more than
  one base is never resized. On the NASA reference this took the tool from
  11.51 MB to 5.55 MB, byte-stable across passes.
- **The command-line binary.** The crate now ships a CLI over the same library
  API: positional input, `-o/--output` (default `<input>.optimized.pdf`, never
  overwriting without `--force`), and flags mapping to every `OptimizeOptions`
  builder. Defaults — including the boolean states printed under `--help` —
  are read from `OptimizeOptions::default()` at runtime, so CLI and library
  cannot drift.

## [0.2.1] - 2026-08-22

### Added

- **Opt-in lossless bitonal → CCITT G4 recompression (Phase 3 B-M1)**, via the
  `fax` crate. Covers CCITT-stored sources (G4 `/K -1`, EOL-framed G3) and
  1-bit Flate images including `/ImageMask` streams. Pixels are never
  resampled, `/BlackIs1` polarity is normalized at the sample level, and a
  stream is replaced only when the G4 payload is strictly smaller *and* an
  in-process decode-back reproduces the source bits exactly.
- **Opt-in Type0/CIDFontType2 (Identity-H/V) font subsetting (Phase 3 C-M1)**,
  via `subsetter`. The `/CIDToGIDMap`-stream technique replaces only
  `/FontFile2` and `/CIDToGIDMap`, so content-stream text bytes are never
  rewritten and `/W`, `/DW` and `/ToUnicode` stay untouched — text extraction
  is bit-identical pre/post. Glyph discovery walks pages, form XObjects
  (recursive, cycle-guarded), appearance streams, tiling patterns and Type3
  char procs; any parse doubt disables subsetting for that font or the whole
  document.

### Fixed

- **`optimize(optimize(x))` is now byte-stable on real files.** Duplicate-object
  merging ran exactly one generation per call, so merges that only became
  possible after remapping were left to the next call. The non-stream and
  stream dedup passes now alternate to a fixpoint inside a single call.
- **Page-tree nodes are never deduplicated**, even when byte-identical. Two
  blank pages with identical dicts were being collapsed into one object,
  putting the same id in `/Kids` twice — which changes what GoTo destinations
  resolve to and broke lopdf's page-reordering pass. Page identity is
  load-bearing.

## [0.2.0] - 2026-08-21

### Added

- **FlateDecode image downsampling, on by default.** Over-resolution
  Flate-compressed rasters (screenshots, exported bitmaps, lossless scans) are
  downsampled *in the same format* — decoded including PNG predictors 10–15,
  resized with Lanczos3, re-deflated at maximum compression. `/ColorSpace` is
  never touched, so no JPEG-class artifacts enter that content class. Scope:
  8-bit DeviceRGB/DeviceGray/ICCBased(N=1,3) without `/SMask`, `/Mask` or
  `/Decode`. Opt out with `downsample_flate_images = false`, which restores
  0.1.x behavior exactly.
- Decompression-bomb guard on the Flate path: a hard 256 MiB raw-pixel cap
  enforced *during* inflation, plus an exact decoded-length check.

### Fixed

- PNG predictor inversion is performed in-crate (spec-correct, including the
  Avg filter) rather than by lopdf's `Stream::decompressed_content`, whose Avg
  implementation produces incorrect pixels and which returns partial data on
  corrupt zlib streams.

## [0.1.0] - 2026-08-21

### Added

- Initial release: CTM-aware effective-DPI downsampling of over-resolution
  `DCTDecode` images via mozjpeg, duplicate object/stream deduplication,
  optional accessibility-tree stripping, optional PDF 1.5 object-stream
  packing, and the hard fail-safe contract — never larger, never corrupt,
  original bytes on any failure.

---

Releases through 0.4.0 were published under the crate name `amatl`; see the
0.4.1 entry. 0.1.0 and 0.2.0 predate this repository's release tags, so they
have no compare links below.

[Unreleased]: https://github.com/xcheljd/picamatl/compare/v0.4.4...HEAD
[0.4.4]: https://github.com/xcheljd/picamatl/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/xcheljd/picamatl/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/xcheljd/picamatl/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/xcheljd/picamatl/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/xcheljd/picamatl/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/xcheljd/picamatl/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/xcheljd/picamatl/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/xcheljd/picamatl/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/xcheljd/picamatl/releases/tag/v0.2.1
