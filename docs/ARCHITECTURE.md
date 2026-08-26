# amatl architecture

The design map for this codebase. README.md has the *why*; this has the *how*.
Hunt notes (`docs/hunts/huntN-notes.md`) have the *history* — each phase's
investigation, rejected alternatives, and measured tradeoffs.

## The one-line mental model

**Plan everything against an immutable document, then apply. Nothing ships
unless it is provably strictly-smaller, and anything we can't prove is left
byte-for-byte untouched.**

That posture — not any individual optimization — is the thing that makes amatl
amatl. Every feature below is a special case of it.

## Non-negotiable invariants (do not break these)

1. **Never-larger.** No stream is replaced unless the candidate is strictly
   smaller (`out.len() >= original` ⇒ decline). A higher target DPI produces a
   bigger candidate that either wins or is declined; nothing regresses.
2. **Fail-safe.** `optimize` never panics. A `catch_unwind` boundary around the
   whole pipeline turns any panic (deep in a decoder, the parser, the encoder)
   into the same graceful fallback as an ordinary error: **return the input
   bytes unchanged**.
3. **Default-inert & consent-tiered.** Defaults are lossless-only. Flags
   explicitly opt into loss (quality, stripping, encoding-class changes). A
   quality knob like `--figure-dpi` only ever makes output *larger and more
   faithful* — it is not a consent tier.
4. **Idempotence.** `optimize(optimize(x)) == optimize(x)` byte-for-byte.
   Pinned by tests; subtle breakers (masked Flate→JPEG conversions) are
   handled in the plan logic.
5. **Verify by decode-back.** Every re-encoded candidate is decoded back and
   checked against the exact pixels handed to the encoder (geometry + channel
   count + MAD ceiling). Cross-checked against independent decoders, never the
   same library that produced it.
6. **Untrusted-input safety.** Malformed/truncated/crafted bytes decline, never
   corrupt. The CMYK-color-space bug class (writing 3 channels under a 4-channel
   `/ColorSpace`) is guarded by explicit dict/payload consistency checks.

## Pipeline (src/lib.rs `try_optimize`)

Executed in this order; each pass reports whether it did work.

**Phase A — coalescing (pre-plan):**
- `dedup_streams` — merge byte-identical stream objects (repeated logos/product
  shots). Run *before* image planning so a repeated image is decoded, resized,
  and deduped once.
- `dedup_decoded_streams` — second dedup on *decoded* payloads; collapses
  embedded font programs that differ only in serialization.
- Form flattening (opt-in) — `forms::apply_flatten`, runs BEFORE all other
  planners so moved appearance streams get minified/re-deflated. A `None` plan
  is a decline (13 documented rules in `docs/FORMS-PLAN.md`).
- `minify_content_streams` — minify content streams (opt-in by default? no —
  always on; sloppy number literals, merged multi-stream pages).

**Phase B — plan against immutable doc:**
- Image placements: walk each page content stream tracking the CTM →
  `rendered_w/h_pts` per image XObject → **effective DPI** per axis
  (`px / rendered_inches`). This is the core idea: downsample only images that
  are genuinely over-resolved *for how they are drawn*.
- Font subsets: plan per `/FontDescriptor` (read-only). Type0/CIDFontType2
  Identity-H/V + simple TrueType; Type1C merges. Never rewrites content-stream
  text bytes.
- Bitonal → CCITT G4 (opt-in `--recompress-bitonal-images`).
- JPX→DCT conversion (opt-in `--allow-lossy`).
- Gray-collapse detection (`--collapse-gray-images`).

**Phase C — apply:**
- Each `Replacement` applied; every candidate that fails strictly-smaller is
  skipped (per-stream guard).
- Fonts applied; strips (accessibility / metadata / private-data / hinting)
  applied; then `dedup_objects` + `dedup_streams` again (the dedup passes are
  mutually-interfering, so they run both sides of the apply phase).

**Phase D — prune & serialize:**
- `doc.prune_objects()` — drop now-orphaned objects (struct elems, stripped
  metadata/private-data, unused font programs).
- `doc.compress()` — lopdf's stream compression.
- `redeflate_flate_streams` — re-deflate every Flate stream at max level via
  zlib-rs (or zopfli backend), strictly-smaller-guarded.
- `reoptimize_jpeg_streams` — rebuild Huffman tables on untouched JPEGs
  (jpegtran-equivalent), strictly lossless, verified by decode-back.
- `pack_object_streams` (default ON) → puts a PDF 1.5 floor on output;
  opt out for pre-Acrobat-6 readers.
- `renumber_objects`, `strip_stale_xref_trailer_keys`, compressed xref stream.
- The real-literal restoration pass (`src/reals.rs`) splices the input's own
  literals back into the saved file — f32-parsing otherwise loses precision on
  e.g. `/MediaBox [0 0 595.91998 841.91998]`.

## Source layout

| File | Lines | What lives there |
|---|---|---|
| `src/lib.rs` | ~11K | The whole pipeline: `OptimizeOptions`, placement walk, plan/apply, all image & serialization passes, fail-safe boundary, the bulk of tests |
| `src/fonts.rs` | ~3.4K | Font subsetting + Type1→CFF conversion |
| `src/type1.rs` | ~2K | Type1 interpreter |
| `src/jpeghuff.rs` | ~1.8K | Lossless Huffman-table re-encoder |
| `src/cffhint.rs` | ~1K | Type2 hint stripping |
| `src/forms.rs` | ~1.2K | Form flattening |
| `src/cffmerge.rs` | ~0.8K | Type1C same-family union merges |
| `src/bitonal.rs` | ~0.8K | CCITT G4 recompression |
| `src/encodings.rs` | ~0.9K | Font encoding tables (extracted from Ghostscript resources) |
| `src/reals.rs` | ~1K | Real-literal splice-back |
| `src/truetype.rs` | ~0.7K | TrueType subset cmap synthesis |
| `src/main.rs` | ~0.4K | CLI (every flag maps 1:1 to an `OptimizeOptions` builder method) |

## Key mechanisms

- **Effective-DPI gate** (the core): `over_resolution = max(eff_w, eff_h) >
  target_dpi × dpi_margin && target < stored`. Evaluated on BOTH axes so a
  non-uniformly scaled image is caught even if one axis looks fine.
- **Masked-image handling** (Phase 5/6): D-M1 (dimension-preserving
  requant), D-M2 (coupled downsample of JPEG base + `/SMask` as an atomic
  pair), D-M3 (same for Flate base), P-M1 (shared-mask refcount guard — a
  shared mask is never resized). The mask always follows the base's target
  geometry.
- **`--figure-dpi`** (2026-08-26): heuristic text/chart-aware DPI. Classifier:
  `background ∈ [0.25, 0.75) && edges ≥ 0.10` on full-resolution decoded
  pixels (three decode routes: RGB JPEG / CMYK JPEG / Flate). Promotes figure-
  like images to a higher target so zooming stays legible. Known v1 boundary:
  busy-background charts (bg < 0.25) are not promoted — that band would need
  OCR, deliberately deferred.
- **CMYK/YCCK**: four-component JPEGs take a dedicated path — decode to raw
  CMYK samples (libjpeg `JCS_CMYK`), resample all four channels, re-encode as
  YCCK with APP14 written by libjpeg. Never converted to RGB; `/Decode` arrays
  decline. See `docs/CMYK-JPEG.md`.
- **Digital signatures**: re-serialization invalidates `/ByteRange` digests.
  Always has; the entropy passes no longer pretend otherwise by declining.
  Optimizing a signed PDF invalidates its signature — documented in README.

## Consent ladder (README calls these levels)

| Level | Flag | What it allows |
|---|---|---|
| Lossless defaults | — | dedup, re-deflate, Huffman re-table, subset fonts, ObjStm packing, JPEG/Flate downsample-at-target (encoding class preserved) |
| Quality knob | `--figure-dpi` | higher target for figure-like images (never lossier) |
| Lossy | `--allow-lossy` | Flate photos → JPEG (line-art auto-declined), JPX → DCT, CMYK requant |
| Strips | `--strip-*` | accessibility, metadata (breaks PDF/A+UA), private data, hinting |
| Form flatten | `--flatten-forms` | burn widget appearances, drop form layer (declines unless value provably preserved) |

## Reading the tests

The suite is the best documentation of the invariants. Key groups:
- `real_file_shrinks_when_present` + the corpus byte-stability gates — the
  never-larger contract on real files
- `redeflate_is_never_larger_and_idempotent`, `mutated_cmyk_streams_never_
  corrupt_or_escape` — the two scary failure classes
- The `figure_metrics_separate_charts_from_photos_and_line_art` /
  `line_art_metrics_*` tests — the classifier calibration

## Reproducing benchmarks

- `scripts/bench-full.sh` — four/five-lane matrix (lossless/lossy/kitchen/gs/
  gs-custom) over the corpus; prints per-file paragraphs.
- `scripts/bench-vs-gs.sh` — fixture vs Ghostscript.
- NASA TM-20210010291: input at `~/Labs/talaria/.extraction-tmp/nasa.pdf`
  (not in the repo; re-downloadable from ntrs.nasa.gov). Defaults →
  4,443,883 B (73.6%); `--figure-dpi 195` → 5,404,689 B (67.8%);
  `--target-dpi 0` → 15,654,875 B (93.2%, pixel-exact).

## Maintenance notes

- MSRV 1.88 (dependency-driven). CI runs 3-OS matrix; clippy `-D warnings`
  tracks latest stable (a new lint can break CI even on docs-only commits —
  see `9fec5e3`).
- `Cargo.lock` is gitignored (library crate); only `Cargo.toml` ships.
- mozjpeg (C) is the only native code; libjpeg-turbo compiled in. Build needs
  NASM. No native *runtime* deps.
- Dependencies all permissive (MIT/Apache/BSD/Zlib). AGPL is a hard no
  (jpegli-rs rejected 2026-08-25 for exactly this).
