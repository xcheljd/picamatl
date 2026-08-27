# Phase 3 Implementation Plan — More Image Filters & Font Subsetting

Status: **planning document — no code yet.** Approved scope per
`talaria/docs/picamatl-roadmap.md` §7 ("More image filters" and "Font subsetting")
and its "Dependency philosophy — the two-axis rule" section.

Every API name, crate version, and behavior claim below was verified on
2026-08-21 against the actual `lopdf 0.42.0` build in this repo's lockfile
(rustdoc + an empirical probe test, since docs.rs was unreachable from this
session) and against crates.io via `cargo info` / `cargo tree`. Nothing here is
from memory alone; the few remaining unknowns were flagged inline and are now
resolved in [Decisions — RESOLVED](#decisions--resolved).

---

## Executive summary

Phase 3 adds two capability families on top of the existing
DCTDecode-downsampling core, both under the same non-negotiable contract:
`catch_unwind` boundary, never-larger output, never-corrupt output, any
per-object uncertainty → leave that object untouched.

1. **FlateDecode images (A).** Extend the existing effective-DPI pipeline to
   Flate-compressed raster images (the PNG-like class: screenshots, exported
   bitmaps, scanned pages saved lossless). lopdf 0.42 already does the hard
   part — `Stream::decompressed_content()` decompresses **and applies the
   `/DecodeParms` PNG predictor** (verified empirically with a Predictor-15/Up
   stream: it returns fully de-filtered pixel rows). M1 downsamples
   over-resolution Flate images *in the same format* (no JPEG conversion, so
   `/ColorSpace` is untouched and no artifact-class change); cross-format
   Flate→JPEG conversion is a later opt-in. Estimated effort: **3–5 days**.

2. **CCITT / JBIG2 (B).** Honest scoping: these are decode-side formats; the
   real opportunities are (a) re-encoding G3 and Flate-stored *bitonal* images
   to CCITT G4 — now feasible in pure Rust because `fax 0.3.0` (MIT, pdf-rs
   org) ships both a G3/G4 **decoder and encoder** — and (b) maybe lossless
   generic-region JBIG2 later. Lossy/symbol-mode JBIG2 (the Xerox
   digit-substitution hazard) and MRC segmentation are recommended
   **permanently out of scope**. Estimated effort: 1–2 day spike + **~1 week**
   for the G4 milestone. Ships after A and C-M1.

3. **Font subsetting (C).** Integrate `subsetter 0.2.6` (typst org; MIT OR
   Apache-2.0; rust-version 1.85 ≤ our 1.88 MSRV; exactly **one** transitive
   dep with `default-features = false`, verified via `cargo tree`). Two facts
   from subsetter's own docs dictate the whole design: it **removes the `cmap`
   table** (output is only usable as a CID font) and it **remaps glyph IDs**.
   M1 therefore targets Type0 / CIDFontType2 / Identity-H fonts only, and uses
   a `/CIDToGIDMap` stream (old CID → new GID) so **content-stream text bytes
   are never rewritten** — eliminating the entire "rewrote text wrong" bug
   class. Glyph-set discovery (a resource-aware content-stream walker) is the
   genuinely hard part and is planned honestly below. Opt-in flag first.
   Estimated effort: **2–3 weeks** for M1.

Recommended order: **A-M1 → C-M1 → B-M1**, then the M2s as interest warrants.

---

## Resolved decision set

All nine open questions are resolved (details and rationale in
[Decisions — RESOLVED](#decisions--resolved)). The compact outcomes an
implementer should internalize before reading further:

1. `downsample_flate_images` **defaults ON** (0.2.0, minor bump + changelog).
2. Flate→JPEG conversion **deferred wholesale to A-M2** — no dormant flag in M1.
3. `subset_fonts` ships **opt-in (default OFF)** at 0.3.0; flip to default-ON is
   the explicit target after a clean corpus soak.
4. **Commit an OFL font fixture** (Noto Sans Regular + OFL license text) under
   `fixtures/fonts/`.
5. **Skip font subsetting for PDF/A-declared documents** in C-M1 (detect
   `pdfaid:part` in XMP); revisit with `/CIDSet` regeneration in C-M2.
6. Predefined CJK CMaps (`UniGB-UCS2-H` etc.): **permanent skip** — affected
   fonts are left untouched, never broken.
7. Simple-font (non-Type0) subsetting: **out of scope** — documented in the
   README honest-limitations section.
8. Symbol-mode JBIG2: **permanent public commitment against it** in the README.
9. Dependencies **approved as listed**: `flate2` (direct), `subsetter 0.2.6`
   (`default-features = false`), `fax 0.3.0` (at B-M1, post-spike),
   `ttf-parser` (dev-only). `jbig2enc-rust` **not** approved.

---

## Sequencing recommendation

| Order | Milestone | Why this position |
| --- | --- | --- |
| 1 | **A-M1** — Flate downsample, same-format | Smallest delta: reuses `collect_placements`, the DPI threshold logic, and the replace-only-if-smaller guard verbatim. Widest applicability win per line of code. De-risks the "second filter" plumbing (filter classification, DecodeParms) that B also needs. |
| 2 | **C-M1** — Type0/CIDFontType2 subsetting, opt-in | The headline differentiator (the roadmap's "biggest win on text-heavy PDFs"). Independent of A's code paths, so it can start while A soaks. Opt-in flag keeps the blast radius at zero for existing callers. |
| 3 | **B-M1** — bitonal → CCITT G4 re-encode | Rides on A's filter-classification plumbing (it is "the 1-bit branch" of the same dispatch). Needs the `fax` crate vetting spike first. |
| 4+ | A-M2 (SMask pairs, Flate→JPEG opt-in), C-M2 (embedded CMaps / CJK verification), B-M2 (lossless JBIG2, only if a Rust encoder vets clean) | Each is optional and independently shippable. |

Explicitly deferred/rejected: simple-font (non-Type0) subsetting, predefined
CJK CMaps (would mean shipping Adobe mapping tables), lossy JBIG2, MRC,
bitonal downsampling. Rationale in the per-capability sections.

---

## A. FlateDecode images

### A.1 Architecture fit and the lopdf 0.42 API reality

**Verified API surface** (rustdoc of the locked `lopdf 0.42.0` + probe test):

- `Stream::decompressed_content(&self) -> Result<Vec<u8>>` — decompresses
  FlateDecode **and applies the `/DecodeParms` predictor**. Probe: a stream
  with `/Filter FlateDecode` + `/DecodeParms << /Predictor 15 /Colors 3
  /BitsPerComponent 8 /Columns 4 >>` whose payload was PNG-Up-filtered rows
  came back as fully de-filtered pixels (`[10,20,30,10,20,30,...]`), not raw
  filter rows. So we do **not** hand-roll predictor inversion.
- `lopdf::filters::png::decode_frame(content: &[u8], bytes_per_pixel: usize,
  pixels_per_row: usize) -> Result<Vec<u8>>` plus `decode_row` / `encode_row`
  and `FilterType { None, Sub, Up, Avg, Paeth }` are public — a standalone
  fallback and, importantly, `encode_row` gives us cheap Up-filtering on the
  **encode** side.
- `Stream::filters(&self) -> Result<Vec<Vec<u8>>>` returns the filter-name
  chain for classification.
- `Stream::set_content` sets bytes without touching the dict (the existing
  DCT path already relies on this).

**Implementation checks to do on day 1** (cheap, in-test): behavior when
`/Filter` is the array form `[/FlateDecode]` with `/DecodeParms` as an array;
whether TIFF Predictor 2 is applied or errors (if unsupported → that image is
skipped, fail-safe). The probe already confirmed the scalar-name form.

**Where it slots in.** `plan_replacement` (src/lib.rs:380) grows from a
DCT-only gate into a filter dispatch:

```
match classify_filter(doc, dict):
    DctOnly   -> existing JPEG path (unchanged)
    FlateOnly -> new flate path (M1)
    _         -> None (untouched)
```

The new flate path, all inside the existing rayon `plan_replacement` call so
parallelism and the `catch_unwind` boundary are inherited:

1. Same placement/DPI math as today (`collect_placements` unchanged — it is
   filter-agnostic already).
2. Eligibility gates (each failure → `return None`, image untouched):
   `/BitsPerComponent == 8`; `/ColorSpace` resolves to `DeviceRGB`,
   `DeviceGray`, or `ICCBased` with `/N` 1 or 3 (component count is all we
   need since M1 never changes the color space); no `/SMask`, `/Mask`,
   `/Decode`, or `/Interpolate`-relevant surprises (keep the existing SMask
   skip); `Indexed`, 1/2/4-bit, 16-bit, `Separation`, `Lab` → skip in M1.
3. **Decompression-bomb guard before decoding**: compute
   `expected = width * height * channels`; skip if over a cap (256 MiB of raw
   pixels, ~9.5k×9.5k RGB — far above anything legitimately placed on a page).
   After `decompressed_content()`, require `decoded.len() == expected`
   exactly; any mismatch → skip.
4. Resize raw pixels with the existing `image` crate + Lanczos3 (wrap the
   buffer as `RgbImage`/`GrayImage`; no new decode dependency).
5. Re-encode: apply PNG Up filtering per row via `filters::png::encode_row`,
   deflate at level 9, and also try plain (no-predictor) deflate; keep the
   smaller of the two. Write `/Filter FlateDecode`, correct
   `/DecodeParms << /Predictor 15 /Colors n /BitsPerComponent 8 /Columns w >>`
   (or remove DecodeParms for the no-predictor variant), update
   `/Width`/`/Height`. Compression via a direct `flate2` dependency —
   **zero new transitive deps** (flate2 is already in the tree under lopdf;
   `Stream::compress` is unsuitable because we must control the level, the
   predictor, and DecodeParms).
6. Existing guard verbatim: `if out.len() >= stream.content.len() { return
   None }` — plus the whole-document never-larger check in
   `optimize_with_options`.

**Failure degradation:** identical shape to the DCT path — every fallible step
is `?`/`ok()?` inside `plan_replacement`, so a corrupt zlib stream, a
predictor lopdf can't handle, or a length mismatch produces `None` (image
untouched), never a document failure. A panic anywhere is caught by the
existing `catch_unwind` boundary. No new failure semantics are introduced.

### A.2 Recompression decision

**M1 recommendation: downsample in place, stay Flate.** Rationale:

- No artifact-class change. Flate images are typically screenshots, charts,
  line art — exactly the content JPEG ruins with ringing. Same-format
  downsampling is the natural extension of the library's documented promise
  ("lossy only on over-resolution images") without a new perceptual risk.
- `/ColorSpace` untouched → ICCBased/DeviceGray/DeviceRGB all "just work."
- The never-larger guard makes it strictly safe: flat-color images that
  deflate to almost nothing simply won't shrink further and stay untouched.

**Re-flating not-over-resolution streams** (level-9 + Up predictor, byte-
guarded): cheap and safe, but touches many images for single-digit-percent
wins and costs CPU. Recommendation: include it in the same pass but **only for
images already being decoded anyway** — i.e., don't decode otherwise-untouched
images just to re-flate them. (Revisit as an explicit "recompress-all" option
if benchmarks justify it.)

**Flate→JPEG conversion** (the big win on photographs someone exported as
PNG): deferred to A-M2 behind `convert_flate_to_jpeg: bool = false`.
Guardrails when it ships: only when the mozjpeg encode at `jpeg_quality` is
≥25% smaller than the best re-flate, and only when a cheap photographic-ness
test passes (e.g. >4096 distinct colors in a downsampled probe — flat-color
graphics fail this). `/ColorSpace` can legally stay (DCTDecode + ICCBased N=3
is valid PDF), so only the artifact class changes — which is exactly why it's
opt-in.

### A.3 Options surface

Two new `OptimizeOptions` fields, both fine under the existing
`#[non_exhaustive]` + hand-written `Default` + `with_*` builder pattern —
adding fields is a **minor** release by construction (this is what the
non_exhaustive design was for):

```rust
/// Downsample over-resolution FlateDecode images in place (format preserved).
pub downsample_flate_images: bool,   // recommend: default TRUE (see Q1)
/// A-M2: convert photographic Flate images to JPEG. Artifact-class change.
pub convert_flate_to_jpeg: bool,     // default FALSE, opt-in, ships in M2
```

**Recommendation: `downsample_flate_images` defaults ON.** It is the same
documented behavior (over-resolution → target DPI) applied to a second filter,
not a new kind of lossiness; semver-wise `#[non_exhaustive]` protects the API
and 0.x allows the behavior delta with a changelog entry (ship as 0.2.0, not
0.1.x). Callers who need bit-stable output across upgrades can pin. **RESOLVED
as Q1: default ON** (see [Decisions — RESOLVED](#decisions--resolved)).

### A.4 Test strategy

- **`tests/generate_fixture.rs`**: add page 3 (over-resolution Flate RGB image
  with Predictor 15, drawn small → gets downsampled) and page 4
  (under-resolution Flate image → untouched). The generator writes filtered
  rows via `filters::png::encode_row` + flate2, exercising the DecodeParms
  path end to end. Regenerate `fixtures/sample.pdf`; the existing
  `real_file_shrinks_when_present` and `scripts/bench-vs-gs.sh` then cover
  Flate for free.
- **Unit tests** (mirroring the existing DCT suite):
  - `build_pdf_flate(px, draw_pts, predictor)` helper; downsample assertions
    per predictor ∈ {None, Sub, Up, Avg, Paeth via Predictor 15, and
    no-DecodeParms}.
  - Grayscale Flate stays 1-channel; ICCBased N=3 accepted; Indexed / 1-bit /
    16-bit / SMask'd images provably untouched (dims + bytes unchanged).
  - **Corrupt-stream degradation**: truncated zlib body, decoded-length
    mismatch (dict claims 100×100 but stream decodes to fewer bytes), and a
    predictor value lopdf rejects — each must return the original document
    bytes (assert byte-equality, the existing contract test shape).
  - **Byte-stability characterization**: `optimize(optimize(x)) ==
    optimize(x)` shape — second pass on the output must be a no-op or
    byte-identical (deflate is deterministic; this pins accidental churn).
- **Bench**: extend `bench-vs-gs.sh` comparison to a Flate-heavy synthetic.

### A.5 Effort & risks

**Effort: 3–5 focused days** including tests and fixture regeneration.

| Risk | Notes / mitigation |
| --- | --- |
| Predictor edge cases | lopdf handles per-row PNG filter switching (`decode_frame` is row-wise). TIFF Predictor 2 support unverified → day-1 test; unsupported ⇒ skip image. PDF has no Adam7 interlacing, so that class doesn't exist. |
| Color-space mismatches | M1 never rewrites `/ColorSpace`; component count is validated against decoded length before any resize. Exotic spaces are gated out. |
| SMask interplay | Existing skip retained. A-M2 must downsample image+SMask **as a pair** (same target dims) or not at all — half-resizing corrupts rendering. |
| Decompression bombs | Pre-decode size cap + exact length check (§A.1 step 3). |
| Behavior-change default | Q1; changelog + 0.2.0 version bump. |

---

## B. CCITT FaxDecode / JBIG2Decode

### B.1 Honest scoping — what is actually achievable

CCITT and JBIG2 streams in real PDFs are **scanner output**; nobody needs us
to *decode-and-display*, and "downsample a bitonal image" is a quality trap
(re-binarization after resampling eats thin strokes and hairlines —
recommended permanently out of scope). The real, honest opportunities:

1. **G3 → G4 re-encode.** Old fax-pipeline scans use G3 (1D/2D), which is
   materially worse than G4 on the same image (commonly 30–50% larger). This
   needs both a decoder and an **encoder**.
2. **Flate-stored bitonal → G4.** Many generators store 1-bit scans as
   FlateDecode; G4 is typically 2–5× smaller on scanned text. This is "the
   1-bit branch" of A's filter dispatch.
3. **JBIG2 (lossless generic region)** buys a further ~20–30% over G4 — B-M2
   at most.

**Ecosystem reality (verified via `cargo info`/`cargo search`, 2026-08-21):**

- **`fax` 0.3.0** — "Decoder and Encoder for CCITT Group 3 and 4" — MIT,
  rust-version 1.71, repo `github.com/pdf-rs/fax`. Pure Rust. This removes
  the historical "CCITT needs C" assumption entirely: **no Axis-A relaxation
  is needed for B-M1.** Vetting spike required (maturity, fuzz behavior,
  `EncodedByteAlign`/`EndOfBlock` parameter coverage) before committing.
- Rust JBIG2 **decoders** exist and are credible (`hayro-jbig2` 0.3.0 by
  LaurenzV, `pdfluent-jbig2`; both MIT/Apache). Note `jbig2dec` (the C
  decoder) is **Artifex AGPL — Axis B forbids it regardless**, but we don't
  need it.
- Rust JBIG2 **encoders** now exist but are young/unvetted: `jbig2enc-rust`
  0.5.3 (MIT OR Apache-2.0, default feature `symboldict` — i.e. symbol mode).
  The classic `jbig2enc` is C++ on leptonica — even though Apache-2.0, it is
  not a "narrow leaf" dep and is rejected under the roadmap's Axis-A rule.

**The symbol-mode red line.** Lossy/symbol-coded JBIG2 is the Xerox scandal
failure mode: visually plausible **character substitution** (6↔8) in scanned
documents. For a library whose entire brand is a hard fail-safe contract,
shipping that mode would be self-sabotage. Recommendation: **permanently out
of scope, stated in the README as a trust commitment** (it's also a
positioning win, like the accessibility angle). Lossless generic-region JBIG2
has no such hazard and remains a legitimate B-M2 candidate.

### B.2 Recommended phase split

- **Spike (1–2 days, before B-M1):** vet `fax` — round-trip fuzzing on
  synthetic bitonal images, parameter coverage (`/K`, `/Columns`, `/Rows`,
  `/BlackIs1`, `/EncodedByteAlign`), decode-vs-Ghostscript comparison on real
  scans.
- **B-M1 (~1 week):** in the A dispatch, add a `Bitonal` branch: 1-bit
  Flate images and CCITT G3 streams → decode (lopdf / `fax`) → re-encode G4
  (`fax`) → replace only if smaller. `/DecodeParms` written accordingly
  (`/K -1`, `/Columns`, `/BlackIs1` normalized). No resampling, ever.
- **B-M2 (optional, gated on encoder vetting):** lossless generic-region
  JBIG2 for the same bitonal class, behind an opt-in flag; and JBIG2→G4/JBIG2
  recompression using a Rust decoder. Do not schedule until a Rust encoder
  passes the same vetting bar as `fax`.
- **Out of scope, recommend permanently:** symbol/lossy JBIG2, bitonal
  downsampling, and MRC (below).

### B.3 Vetting spike results (2026-08-22) — PASS (partial scope; see note)

Checks run against `fax 0.3.0` (probes live in `tests/fax_spike.rs`, kept as a
permanent regression harness):

**Scope note (honesty vs §B.2's spike definition):** the original spike covered
G4 round-trip fuzzing, the panic battery, and byte economy — it did **not**
cover the §B.2 items "parameter coverage (`/K`, `/Columns`, `/Rows`,
`/BlackIs1`, `/EncodedByteAlign`)" or "decode-vs-Ghostscript comparison on real
scans". Those gaps are closed by the B-M1 pre-work probes: G3 (`/K` ≥ 0) decode
behavior and wide-row/degenerate-`/Columns` coverage land as additional spike
tests, and a PDF-embedded G4 stream is rendered through Ghostscript for foreign
-decoder interop; `/BlackIs1` polarity and `/EncodedByteAlign` handling are
resolved as B-M1 eligibility-gate tests.

| Check | Result | Detail |
| --- | --- | --- |
| License | PASS | MIT LICENSE file present in the crate |
| Unsafe | PASS | `#![deny(unsafe_code)]` crate-wide |
| MSRV | PASS | `rust-version = "1.71"` (fax's own Cargo.toml, verified from registry source) — well under our 1.88 floor |
| Transitive deps | PASS | none beyond std |
| G4 round-trip | PASS | pixel equality on noise / flat-run / document-like / tiny-8×8 / single-row patterns (all five row filters of synthetic content) |
| Panic safety | PASS | truncations at 5 offsets (incl. inside EOFB), every single-byte bit-flip across a valid stream, and 64 pure-garbage streams — zero panics, all fold to clean `None`/error |
| Byte economy | PASS (with one honest caveat) | document-like: G4 **11,694 B vs flate9 12,346 B — G4 wins** (10.6% vs 11.2% of raw); flat-runs: G4 330 vs flate9 280 (deflate's best case, all-white rows); noise: both expand, G4 more |

**Gate decision: PROCEED with B-M1.** The never-larger guard makes the
flat-runs caveat harmless — a stream is only ever replaced when G4 is strictly
smaller, and G4 wins on the realistic scanned-document pattern, which is the
entire target corpus for this milestone.

#### B.3.1 Gap-closing probes (2026-08-22) — PASS

Follow-up probes closing the scope-note gaps (`tests/fax_spike.rs` +
`tests/fax_pdf_interop.rs`), all against `fax 0.3.0` source read from the
registry tarball:

| Probe | Result | Detail |
| --- | --- | --- |
| G4 extended makeup (>2560) | PASS | W=5100 round-trips: solid-white ×64 rows, solid-black ×64, noise ×32, document-like ×64 — pixel equality; solid rows force the `while n >= 2560` encoder loop and the extended-makeup table entries both colors |
| G4 degenerate dims | PASS | 1×1 (both colors), W=1 alternating/solid ×8 rows, W=63 (non-byte-aligned) noise/solid ×16 — pixel equality |
| G3 1D decode (positive) | PASS | hand-framed EOL streams (codewords from fax's public tables, framing hand-built: initial EOL, per-line EOL, RTC = 6 EOLs) round-trip pixel-equal on document-like/flat/noise/solid at W=1728, solid at W=5100 (1D extended makeup), 8×8 |
| G3 panic battery | PASS | truncations at 5 offsets, single-byte inversion at every byte of a valid stream, 64 pure-garbage streams — zero panics |
| PDF embed round-trip | PASS | 203×131 (odd dims) G4 image written as `/CCITTFaxDecode` XObject (`/K -1`, `/BlackIs1 false`) via lopdf 0.42; reload hands back byte-identical stream content; fax decodes to pixel equality |
| Foreign decoder (Ghostscript) | PASS | gs 10.07.1 `-sDEVICE=pbmraw -r72` render of that PDF: **0 of 26,593 pixels differ** from the source bitmap |

**Decoder contract facts (from fax 0.3.0 source, binding for B-M1 design):**

- `decode_g3` is pure T.4 1D **and requires EOL framing**: the constructor
  consumes a mandatory initial EOL, and no tag bit is read after EOLs. PDF
  `/K > 0` (mixed 2D) streams are unsupported, and `/K == 0` streams written
  with `/EndOfLine false` (the PDF default) are misframed — probe
  `g3_missing_initial_eol_fails_safely` pins the failure mode (no panic, no
  false success). ⇒ B-M1 CCITT-source eligibility: `/K < 0` primary;
  `/K == 0` only with `/EndOfLine true`; `/K > 0` skip.
- `decode_g4(…, height=Some(h))` stops reading at `h` lines (trailing garbage
  is never examined) and pads missing rows as all-white after an early EOFB.
  ⇒ B-M1 must drive `Group4Decoder` directly and require a clean
  `DecodeStatus` accounting for every row — "got h rows" is not evidence of a
  clean stream.
- The G4 decoder has no fill-bit handling between rows ⇒ `/EncodedByteAlign
  true` on `/K < 0` must be an eligibility-gate skip. (G3 1D fill bits before
  EOL are handled.)
- No debug printing on active paths: `print_peek`/`print_remaining` exist but
  every call site is commented out; the `debug!` macro is gated behind the
  off-by-default `debug` feature.

### B.5 Status

**B-M1 SHIPPED** (0.3.0-dev, 2026-08-22): opt-in
`OptimizeOptions::recompress_bitonal_images` in `src/bitonal.rs` — both source
shapes (CCITT-stored and Flate-stored 1-bit, incl. `/ImageMask`), strict
decoders per the §B.3.1 contract facts, `/BlackIs1` normalization, never-larger
guard with parms-overhead accounting, and production decode-back verification.
14-test battery in `src/bitonal.rs` mirrors the A-M1 corruption/idempotence/
polarity posture. Deviations from the §B.2 sketch: CCITT-source eligibility is
`/K -1` primary plus `/K 0` only with `/EndOfLine true` (`/K > 0` and EOL-less
`/K 0` are fail-safe skips — fax decoder limits, §B.3.1); the bitonal pass runs
over all image XObjects rather than inside the placement-keyed A dispatch
(lossless ⇒ placement-independent). B-M2 (JBIG2) remains unscheduled.

### B.4 Risk register

| Risk | Assessment / recommendation |
| --- | --- |
| **MRC trap** (foreground/background layer segmentation, à la DjVu / high-end scanner pipelines) | A research project wearing a feature's clothes: segmentation quality determines legibility, failure modes are subtle, and the payoff duplicates what scanners already emit. **Avoid entirely; do not leave a placeholder option for it.** |
| Symbol-mode JBIG2 substitution | Out of scope permanently (see red line above). |
| `fax` crate maturity | It's the pdf-rs org's decoder, but the *encoder* path is less traveled. Spike + fuzz before commitment; every re-encode is verified by decode-back comparison in tests (bit-exact bitmap round-trip — lossless means we can assert equality). |
| `/BlackIs1` and photometric inversion bugs | Classic CCITT foot-gun (inverted scans). Normalize explicitly; round-trip tests include both polarities. |
| CCITT params lopdf passes through vs. interprets | lopdf does not decode CCITT for us (it's a pass-through filter for `decompressed_content` — confirm on day 1; if it errors on CCITT streams, read `stream.content` raw and drive `fax` ourselves, which is the plan anyway). |

---

## C. Font subsetting via `subsetter`

### C.6 Dependency audit (first — it shapes the whole design)

Verified 2026-08-21 (`cargo info subsetter`, `cargo add` + `cargo tree` in a
scratch change, then reverted):

- **subsetter 0.2.6** (github.com/typst/subsetter, typst org). License
  **MIT OR Apache-2.0**; `rust-version = 1.85` → fits our 1.88 MSRV with
  headroom, **no MSRV bump**. (`fax` is 1.71; flate2 is already in-tree.)
- Features: `default = [variable-fonts]`, which pulls `skrifa`, `write-fonts`,
  `kurbo` (Google fontations — pure Rust, for instancing variable fonts).
  With **`default-features = false`** the entire transitive footprint is
  **one crate: `rustc-hash 2.1.3`** (verified via `cargo tree`).
  **Recommendation: `default-features = false` for M1**; a variable font that
  subsetter then can't process becomes a skip (fail-safe), and we can add the
  feature later if real PDFs surface embedded variable fonts (rare — PDF
  producers overwhelmingly embed static instances).
- License files: the typst org dual-licenses with LICENSE-MIT/LICENSE-APACHE
  at repo root; **verify the files are present in the vendored crate tarball
  at integration time** (checklist item, standard for typst crates).
- API (from its rustdoc):
  `subset(data: &[u8], index: u32, mapper: &GlyphRemapper) -> Result<Vec<u8>, Error>`;
  `GlyphRemapper::{new, remap(old_gid) -> new_gid, get, num_gids,
  remapped_gids}`. `.notdef` (gid 0) always retained. CFF2 unsupported
  (→ skip such fonts).
- **The two facts that dictate our design, from subsetter's own docs:**
  1. *"You must write your fonts as a CID font. This is because we remove the
     `cmap` table."* → The output font cannot serve a simple (non-Type0)
     TrueType font dict, which resolves glyphs through `cmap`.
  2. Glyph IDs are **remapped** to a contiguous range; for CFF it converts
     SID-keyed → CID-keyed with an *identity GID→CID* mapping, discarding the
     original CID scheme.

### C.1 Integration point — where fonts live and what lopdf gives us

Object-graph reality: page `/Resources /Font` → font dicts.

- **Composite (M1 target):** `/Subtype /Type0`, `/Encoding /Identity-H` (or
  `Identity-V`), `/DescendantFonts [→ dict]` with `/Subtype /CIDFontType2`
  (TrueType outlines, `/CIDToGIDMap` either `/Identity` or a stream) or
  `/CIDFontType0` (CFF). `/FontDescriptor` hangs off the **descendant**;
  the font program is `/FontFile2` (TrueType) or `/FontFile3`
  (CFF: `Subtype` `Type1C`/`CIDFontType0C`/`OpenType`).
- **Simple fonts:** `/Subtype /TrueType | /Type1`, `/FirstChar`/`/Widths`,
  `/Encoding` (+`/Differences`), descriptor with `/FontFile2`/`/FontFile3`/
  `/FontFile`.
- Fonts are also referenced from **Form XObject** `/Resources`, **annotation
  appearance streams** (`/AP`), **tiling patterns** (PatternType 1), and
  **Type3** `/Resources` — the discovery walker must cover these (§C.2).

lopdf gives us: `Document::get_page_fonts(page_id)`, generic dict/stream
traversal (`get_object`, `as_dict`), and `Stream::decompressed_content()` for
the Flate-wrapped `FontFile2` bytes (`/Length1` is metadata we regenerate on
write). lopdf's `Encoding` enum / `extract_text` are **Unicode-oriented**
(ToUnicode-based, for text extraction) — they do not give code→GID mapping, so
glyph discovery is entirely ours to build. `extract_text` remains useful as a
pre/post verification oracle (§C.5).

### C.2 Glyph-set discovery — the hard part, planned honestly

`subsetter` subsets *given* glyph IDs; **discovering which GIDs a document
uses is our job**, and getting it wrong means invisible text. Design:

**Walker.** A document-wide sweep (shape parallel to `collect_placements`,
keyed by font `ObjectId` so shared fonts accumulate across pages):

1. Enumerate every content-bearing stream **with its own resource context**:
   page content streams; Form XObjects reachable from any `/Resources`
   (recursive, bounded depth like the existing `resolve` — cycles exist in the
   wild); annotation `/AP` streams (N/R/D, including state sub-dicts); tiling
   pattern streams; Type3 `CharProcs`.
2. In each stream, `Content::decode`, then track `Tf` (current font name →
   font ObjectId via that stream's resources) and the four show operators:
   `Tj`, `'`, `"` (string operand) and `TJ` (strings inside the array
   operand).
3. For an Identity-H/V font: show-string bytes are big-endian 2-byte CIDs.
   Map CID→GID via the descendant's `/CIDToGIDMap` (`/Identity` ⇒ GID = CID;
   stream form ⇒ 2-byte big-endian lookup table). Accumulate the GID set per
   font object.

**The fail-safe inversion — eligibility, not effort.** A font is subset only
when *everything* checked out; any doubt anywhere disqualifies:

- **Global rule (M1): if ANY content-bearing stream in the document fails to
  `Content::decode`, subset NOTHING.** (This also neutralizes the known
  lopdf caveat that inline images `BI…ID…EI` can confuse content parsing —
  such documents are simply left un-subset.)
- Per-font disqualifiers → skip that font only: `/Encoding` is anything but
  Identity-H/V (predefined CJK CMaps, embedded CMap streams → C-M2);
  descendant is not `CIDFontType2`; no embedded `/FontFile2` (not embedded =
  nothing to subset); CFF2; `/CIDToGIDMap` unresolvable; the font is
  referenced from a resource context we didn't walk.
- Font-program parse failure inside `subsetter` → `Err` → skip that font.

**What M1 never does: rewrite text.** Because subsetter remaps GIDs, the naive
plan rewrites every show string (CID = GID under Identity). Instead, M1
exploits `CIDFontType2`'s `/CIDToGIDMap` **stream** form:

> Keep every content stream byte-identical. Subset with
> `GlyphRemapper` over the used GIDs. Replace `/FontFile2` with the subset
> font, and replace `/CIDToGIDMap` with a stream mapping **old CID → new
> GID** (2 bytes per CID up to the max used CID; unused CIDs → 0/.notdef;
> Flate-compresses to almost nothing since it's mostly zeros).

Consequences: `/W`, `/DW`, `/ToUnicode` are all keyed by **CID**, which we
didn't change — they stay valid untouched. Text extraction is bit-identical
pre/post. The whole "rewrote the text wrong" failure class is structurally
impossible in M1. This is also precisely why **CIDFontType0 (CFF) is out of
M1**: its CID→glyph mapping lives inside the CFF charset, and subsetter
forces identity GID=CID on output, so CFF requires actual string rewriting →
C-M2/M3 at the earliest.

Remaining edits per subset font: new deterministic subset tag on `/BaseFont`
(6 uppercase letters + `+`, derived from a hash of the subset bytes — **no
randomness**, outputs stay reproducible; replace any existing tag), same tag
on the descendant and descriptor `/FontName`, regenerate `/Length1`. If the
descriptor has a `/CIDSet` (PDF/A artifact), see Q5. Replace only if
`new FontFile2 + new CIDToGIDMap stream < old FontFile2 [+ old map stream]`
(net-smaller guard), on top of the whole-document never-larger contract.

Composite-glyph closure (a glyph referencing component glyphs) is handled
inside subsetter (it powers Typst's PDF export, where this is table stakes) —
**pinned by a dedicated test** with a composite glyph (e.g. "é") in M1 rather
than trusted blindly.

### C.3 Fallback contract

Unchanged outer contract: `catch_unwind` around the whole pipeline;
never-larger, never-corrupt, original bytes on any failure. Extensions, in
order of blast radius: any content stream fails to parse → no font is touched
document-wide; any per-font doubt (encoding, subtype, CFF2, subsetter error,
net-not-smaller) → that font ships untouched; and fonts are processed
in the same plan-then-mutate shape as images (plan on `&Document`, apply
replacements only after every planned font succeeded).

### C.4 Options surface

```rust
/// Subset embedded Type0/CIDFontType2 fonts to the glyphs actually used.
pub subset_fonts: bool,   // default FALSE — opt-in (recommendation)
```

**Recommendation: opt-in for at least one release cycle.** Font surgery is
correctness-sensitive in a way image downsampling is not (a bad image is
visibly bad; a bad font is *silently* unreadable for someone else's reader).
Flip the default only after the flag has soaked against the NASA benchmark
corpus + personal corpus with the §C.5 verification green. Ships as 0.3.0
with the flag; the default flip would be its own minor release later (Q3).

### C.5 Verification strategy (no rasterizer dependency)

Layered, none of which adds a runtime dep:

1. **Byte-level invariant (M1's superpower):** every content stream is
   byte-identical pre/post — assert it directly in tests. `/W`, `/ToUnicode`
   untouched — assert dict equality minus the intentionally changed keys.
2. **Structural:** output loads in lopdf; the subset `FontFile2` parses under
   **`ttf-parser`** (pure Rust, MIT/Apache — **dev-dependency only**): for
   every used old-GID, the mapped new-GID exists, has an outline, and the
   outline (point sequence via `ttf_parser::OutlineBuilder`) matches the
   original glyph's — plus advance-width equality from `hmtx`.
3. **Semantic oracle:** `Document::extract_text` pre/post equality (ToUnicode
   is untouched, so extraction must be identical).
4. **Dev-only external tools** (exist locally, never shipped — same posture
   as `bench-vs-gs.sh`): new `scripts/verify-fonts.sh` running
   `gs -dNOPAUSE -dBATCH -sDEVICE=nullpage` on pre/post (error/warning-free
   render), and `pdftotext` diff when poppler is present. Optional
   pixel-compare via `-sDEVICE=png16m` for eyeballing, not CI-gating.
5. **Fixtures:** commit an OFL-licensed font (Q4; e.g. Noto Sans Regular,
   OFL text alongside) under `fixtures/fonts/`; `generate_fixture.rs` gains a
   text page — Type0/Identity-H/CIDFontType2, `/CIDToGIDMap /Identity`, CIDs
   composed via a `ttf-parser` cmap lookup in the generator (dev-only). Tests
   cover: multi-page shared font accumulation; glyphs used only inside a Form
   XObject / annotation AP (walker coverage); composite glyph closure;
   CIDToGIDMap-stream input variant; a font used by an unparseable stream →
   untouched; corrupt FontFile2 → untouched; already-subset font
   (`XXXXXX+`) → re-subset only when net smaller.

**CJK verification (C-M2 gate):** the roadmap explicitly asks to verify
CID-keyed/CJK coverage on real samples. Identity-H CJK (the common modern
case — e.g. Typst and Chromium output) is already in M1's supported shape;
predefined CMaps (`UniGB-UCS2-H` etc.) stay skipped (shipping Adobe's mapping
tables is megabytes of data for a shrinking legacy corpus — recommend
permanent skip, Q6). C-M2 adds **embedded** CMap stream parsing (the
`begincidrange` subset of the CMap grammar is small and regular, ~300 lines
with tests) and a real-sample CJK soak using the dev-only gs/pdftotext
harness.

### C.7 Effort estimate & milestone split

| Milestone | Scope | Estimate |
| --- | --- | --- |
| **C-M1** | Type0/CIDFontType2/Identity-H+V, CIDToGIDMap-stream technique, discovery walker (pages + forms + AP + patterns), opt-in flag, fixture font + full test/verification battery | **2–3 weeks** (the walker and its tests are most of it) |
| **C-M2** | Embedded CMap streams; CJK real-sample soak; consider `variable-fonts` feature; revisit CIDSet regeneration | **1–2 weeks** |
| **C-M3 (recommend: never / explicitly out of scope)** | Simple TrueType/Type1 fonts (requires Type0 conversion + full show-string rewriting since subsetter drops `cmap`); CFF/CIDFontType0 (requires string rewriting per §C.2) | Not scheduled — legacy generators already subset their simple fonts; reward does not justify the risk class (Q7) |

Note the deliberate inversion of the original prompt's milestone order: with
subsetter's cmap-removal constraint, **simple-TrueType-first is the *harder*
path**, not the easier one — Type0/Identity-H first is both safer (no text
rewriting) and covers the dominant modern corpus.

---

## Consolidated risk table

| # | Risk | Capability | Severity | Mitigation |
| --- | --- | --- | --- | --- |
| 1 | TIFF Predictor 2 / filter-array forms unhandled by lopdf | A | Low | Day-1 probe tests; unsupported ⇒ skip image |
| 2 | Decompression bombs on hostile Flate streams | A | Med | Pre-decode size cap + exact decoded-length check; catch_unwind |
| 3 | JPEG artifacts on graphics if Flate→JPEG mis-fires | A-M2 | Med | Opt-in flag + photographic-ness gate + ≥25% win threshold |
| 4 | SMask pair desync when downsampling | A-M2 | High | M1 keeps the SMask skip; M2 resizes pairs atomically or not at all |
| 5 | `fax` encoder immaturity | B | Med | Vetting spike; lossless ⇒ decode-back bit-equality asserted per re-encode |
| 6 | `/BlackIs1` polarity inversion | B | Med | Explicit normalization + both-polarity round-trip tests |
| 7 | Symbol-mode JBIG2 character substitution | B | **Critical** | Permanently out of scope; README trust statement |
| 8 | MRC segmentation rabbit hole | B | High (effort) | Explicitly avoided, no placeholder |
| 9 | Missed glyph-usage site ⇒ invisible text | C | **Critical** | Resource-aware walker over all stream classes; global "any parse failure ⇒ subset nothing"; per-font eligibility inversion; gs/pdftotext soak |
| 10 | GID remap breaking text | C | Critical → **eliminated in M1** | CIDToGIDMap-stream technique: content bytes never rewritten |
| 11 | subsetter drops a needed table / composite component | C | Med | ttf-parser outline+advance equality per used glyph; composite-closure test |
| 12 | PDF/A conformance silently broken (`/CIDSet`, `/Length1`) | C | Low-Med | Q5: skip subsetting when XMP declares pdfaid; regenerate Length1 always |
| 13 | Fonts in unwalked contexts (Type3 recursion, nested patterns) | C | Med | Bounded-depth recursion with cycle guard; unwalked context ⇒ font ineligible |
| 14 | Behavior-change default surprising pinned users | A | Low | 0.2.0 bump + changelog; Q1 |
| 15 | New-dep supply chain (subsetter, fax, flate2, ttf-parser dev) | All | Low | All MIT/Apache, pure Rust, narrow leaves; `default-features=false` keeps subsetter at 1 transitive dep; lockfile pins |

MSRV: no change — all candidate deps have `rust-version` ≤ 1.85 (subsetter
1.85, fax 1.71); crate stays at **1.88.0**.

---

## Decisions — RESOLVED

All nine questions below were delegated to and resolved on the stated
project lens: **best and widest overall compatibility to compete with
Ghostscript, with the best defaults for overall usage.** Each was re-examined
through that lens before finalizing. **No final decision differs from the
original recommendation.** The lens did sharpen three of them: it raises the
priority of A-M2's Flate→JPEG conversion (Q2, where Ghostscript's AutoFilter
converts by default), it makes default-ON the explicit end-state for font
subsetting after soak rather than an open question (Q3, Ghostscript subsets
by default), and it was weighed and rejected for predefined CJK CMaps (Q6,
where "skip" degrades gracefully — affected fonts are left untouched, so
compatibility is never broken, unlike Ghostscript-style bundling of Adobe
tables for a shrinking legacy corpus). Decided-by is noted per item.

1. **`downsample_flate_images` defaults ON** (0.2.0, changelog + minor bump).
   Lens: Ghostscript's presets downsample all raster classes by default;
   default-OFF would quietly ignore the second-most-common image class, and
   the fail-safe contract fully covers the change.
   *Decided: Xchel via delegation, 2026-08-22.*
2. **Flate→JPEG conversion deferred wholesale to A-M2** — no dormant flag in
   M1. Lens: this is where Ghostscript still wins on ratio (AutoFilter →
   DCTEncode), so A-M2 rises in priority — but a lean, benchmark-informed M1
   is the better default path than shipping an untested heuristic now.
   *Decided: Xchel via delegation, 2026-08-22.*
3. **`subset_fonts` ships opt-in (default OFF) at 0.3.0.** Lens: Ghostscript
   subsets by default, so **default-ON is the explicit target** once the NASA
   + personal corpora soak cleanly with the verification harness green; until
   then OFF is the best default for overall usage (never-corrupt beats ratio).
   *Decided: Xchel via delegation, 2026-08-22.*
4. **Commit the OFL font fixture** (~300–600 KB, Noto Sans Regular + OFL
   license text) under `fixtures/fonts/`. Lens: real subsetting tests against
   a real font are what make a compatibility claim credible; OFL is
   unambiguous for redistribution.
   *Decided: Xchel via delegation, 2026-08-22.*
5. **Skip font subsetting for PDF/A-declared documents in C-M1** (detect
   `pdfaid:part` in the XMP metadata stream); revisit with proper `/CIDSet`
   regeneration in C-M2. Lens: widest compatibility includes not silently
   breaking a document's declared conformance.
   *Decided: Xchel via delegation, 2026-08-22.*
6. **Predefined CJK CMaps (`UniGB-UCS2-H` etc.): permanent skip.** Lens
   examined and rejected bundling: skip degrades gracefully (those fonts are
   left untouched — output is always valid), while support means shipping
   megabytes of Adobe tables for a legacy corpus; embedded CMaps (C-M2) cover
   the parseable remainder.
   *Decided: Xchel via delegation, 2026-08-22.*
7. **Simple-font (non-Type0) subsetting: out of scope**, documented in the
   README's honest-limitations section. Lens: it would require Type0
   conversion + full text rewriting (subsetter removes `cmap`) — the exact
   bug class M1's design eliminates; skipped fonts remain untouched, so
   compatibility is preserved, not reduced.
   *Decided: Xchel via delegation, 2026-08-22.*
8. **Symbol-mode JBIG2: permanent public commitment against it** — add the
   "picamatl will never lossy-symbol-encode your scans" statement to the README
   alongside the accessibility angle. Lens: never silently corrupting glyphs
   (the Xerox hazard) is itself a compatibility/trust differentiator vs
   less-careful pipelines.
   *Decided: Xchel via delegation, 2026-08-22.*
9. **Dependencies approved as listed:** direct `flate2` (already transitive
   via lopdf — zero new code in tree), `subsetter 0.2.6` w/
   `default-features = false` (+`rustc-hash`), `fax 0.3.0` (at B-M1,
   post-spike), `ttf-parser` (dev-only). `jbig2enc-rust` explicitly **not**
   approved (unvetted, and its default feature is symbol mode). Lens: the
   minimal vetted set that unlocks the compatibility wins above.
   *Decided: Xchel via delegation, 2026-08-22.*
