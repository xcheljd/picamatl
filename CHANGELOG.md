# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
  amatl win.
- **`--strip-private-data`** (opt-in). Removes every `/PieceInfo` page-piece
  dictionary (Illustrator/InDesign keep editable copies there). On
  `cmyk-jpeg.pdf`: 261,453 B — 96% of amatl's output — removed, pixel-identical.
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
  nothing (amatl re-serializes the whole doc, so `/ByteRange` digests were
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
