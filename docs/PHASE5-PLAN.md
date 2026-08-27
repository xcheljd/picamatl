# Phase 5 Plan — Transparency-Aware Compression (closing the Ghostscript gap)

Status: DRAFT for review
Date: 2026-08-22
Baseline evidence: NASA TM-20210010291 (16,804,107 B) head-to-head vs Ghostscript 10.07.1

## Measured problem

picamatl 0.2.1 (strip, no packing): 11,511,039 B (68%).
Ghostscript pdfwrite (mirrored 130dpi/q~0.4): 3,722,562 B (22%).

Payload census of the ORIGINAL nasa.pdf:

| class | streams | bytes | % of file |
| --- | --- | --- | --- |
| JPEG with /SMask | 74 | 9,156,940 | 54.5% |
| Flate img with /SMask | 37 | 2,141,762 | 12.7% |
| JPEG without mask | 32 | 4,621,808 | 27.5% |
| content/other streams | ~186 | ~358,766 | 2.1% |
| font programs | 13 | ~347,034 | 2.1% |

`plan_image` (src/lib.rs:512) returns None whenever `/SMask` or `/Mask` is
present: 67.2% of the file is untouchable today. GS downsamples masked images
by transforming base+mask as a unit (verified: original has 198 images above
200 ppi; gs output has zero above 200 ppi, all masked pages survive rendering).

## Resolved decision set

- Contract stays: no encoding-class change without explicit consent; bit-exact
  guarantee applies to pixels AFTER the user-approved transform. Downsampling
  is already lossy-by-design (opt-out exists); masks follow their base image.
- Mask/image unit rule: any operation on a masked image MUST apply the same
  geometry (same scale factor, same resample kernel) to base and mask, or be a
  dimension-preserving transform (JPEG requantization). Never one without the other.
- Fail-safe skip remains the default posture: unresolvable /SMask references,
  mask not DeviceGray 8bpc, /Matte present (premultiplied color — skip until
  explicitly supported), stencil masks (/ImageMask true as /Mask), and any
  parse doubt leave the pair untouched.
- Never-larger guard applies per stream, unchanged. Decode-back verification
  extends to masks: after any mask transform, decode-back must match the
  planned mask samples exactly.
- Idempotence: the fixpoint dedup loop (dec757e) must stay green; new passes
  integrate into the existing single-call pipeline ordering (dedup → plan →
  apply → prune).

## D-M1 — SMask-aware JPEG requantization (dimension-preserving)

Scope: JPEG (DCTDecode) images with /SMask where the mask is a plain
DeviceGray 8bpc JPEG/Flate stream and /Matte is absent.

Transform: decode base JPEG (mozjpeg decompressor), re-encode at
OptimizeOptions::jpeg_quality WITHOUT resizing. Dimensions unchanged ⇒ mask
alignment untouched by construction. Replace only if strictly smaller.

Why first: zero geometric risk, attacks 9.16 MB on the reference corpus.
Estimated saving at q78 on already-q75-ish NASA scans: modest per-image (~10-
20%), but it also unlocks...

**D-M1 SHIPPED** (0.2.1-dev, 2026-08-22): soft-mask-aware JPEG requantization
in `src/lib.rs` (`plan_replacement` → `plan_dct_requant`, `eligible_smask`).
Eligibility is exactly as scoped: `/SMask` resolving to a plain 8-bit
DeviceGray image stream, no `/Matte` anywhere in the pair, no `/ImageMask`
stencil; `/Mask` remains a hard skip; FlateDecode bases deferred to D-M3. The
base is decoded at its own dimensions and re-encoded at
`OptimizeOptions::jpeg_quality`, replaced only when strictly smaller and after
a decode-back pixel verification (geometry + channel count + MAD ceiling).
Since the transform never resizes, the effective-DPI margin of the resize
pipeline is intentionally not applied to masked JPEGs — mask alignment is
preserved by construction.

**D-M2 SHIPPED** (0.2.1-dev, 2026-08-22): SMask-coupled downsampling
(`plan_smask_pair_downsample`, `plan_mask_resample`,
`plan_dct_resize_verified`). Over-resolution masked JPEG pairs are downsampled
as an ATOMIC unit: base (Lanczos3 → JPEG q78) and mask (Triangle → plain
FlateDecode gray) land at identical target geometry and are replaced together
or not at all. Combined 5% minimum-savings guard; decode-back on both sides
(exact for the lossless mask). Shared-mask fail-safe: a mask referenced by
more than one image is never resized (dedup merges byte-identical masks before
planning, making sharing reachable in practice — NASA page-33 repro). Measured
on the reference corpus: picamatl 11.51 MB (D-M1) → 6.03 MB vs Ghostscript's
3.72 MB; idempotent across repeated passes.

## D-M2 — SMask-coupled downsampling (the big one)

Scope: same eligibility as D-M1 plus over-resolution pairs (effective DPI >
target × margin, computed on the BASE image's CTM as today).

Transform: decode base + mask; resample BOTH to the same target geometry with
the same kernel (base: appropriate channel count; mask: bilinear on gray,
then threshold-free — keep 8bpc soft edge); re-encode base as JPEG q78, pack
mask as Flate (or keep its existing filter class). Replace both streams only
if combined strictly smaller. /SMask dict updated with new /Width//Height;
base dict unchanged except /Length.

Tests mirror A-M1/B-M1 batteries: corruption degradation (truncated mask input
⇒ exact original bytes for BOTH streams), never-larger, idempotence, alpha
round-trip (composite base+mask over known background before/after, compare
within JPEG tolerance), /Matte and stencil-mask skip cases.

Fixture: extend tests/generate_fixture.rs with a fifth page embedding an
RGB JPEG + 8bpc DeviceGray /SMask at high effective DPI. Benchmark continuity
noted in CHANGELOG.

Estimated effect on NASA corpus: masked JPEGs are scan-like photos at 200-300
ppi; downsampling to 130 dpi ≈ 2.3x linear reduction ≈ ~5x area. Expect the
9.16 MB masked-JPEG class to fall to roughly 1.5-2.5 MB. picamatl total lands
around 4-6 MB vs gs 3.7 MB — competitive band reached.

## D-M3 — Flate masked-image handling

Same unit rule for the 37 Flate+SMask images (2.14 MB): downsample in place
(existing Flate path extended to carry the mask along) or leave if already
≤130 dpi. Lower priority: smaller pool, existing path does most work once
eligibility opens.

**D-M3 SHIPPED** (0.2.1-dev, 2026-08-22): SMask-coupled Flate-base
downsampling (`plan_flate_smask_pair_downsample`). Over-resolution FlateDecode
bases with an eligible `/SMask` now take the same atomic coupled downsample as
D-M2: base through the format-preserving Flate→Flate path (`plan_flate` —
same `/ColorSpace`, predictor handling unchanged), mask through
`plan_mask_resample`, both to identical target geometry, replaced together or
not at all via the shared `Replacement.smask` mechanism. The combined
never-larger/5% guard uses the exact D-M2 arithmetic (incl.
`MASK_DICT_OVERHEAD`); the shared-mask refcount fail-safe and all D-M1 skip
rules carry over; the pair honors `downsample_flate_images`. Under-resolution
pairs are untouched — no requantization analogue exists for lossless payloads
without a lossy re-encode consent surface. Measured on the reference corpus:
picamatl 6.03 MB (D-M2) → 5,547,684 B (5.55 MB, 33.0% of the 16,804,107 B
original) vs Ghostscript's 3.72 MB; byte-stable on a second pass.

## D-M4 — Indexed/palette and low-bpc images (stretch)

Currently skipped (src/lib.rs:633 M1 scope note). GS re-encodes these too.
Palette-preserving downsample requires nearest-palette mapping — nontrivial
color science, do only after D-M1..3 land and re-measure.

**D-M4 CLOSED WITHOUT IMPLEMENTATION** (2026-08-22 re-measurement): the plan's
own gate ("do only after D-M1..3 land and re-measure") was applied and the
measured pool is empty. Census of every real corpus on hand — the 16.8 MB NASA
report (original AND picamatl-optimized), both watch-repair one-pagers, the picamatl
fixture, and Ghostscript's own outputs — found ZERO `/Indexed` images and
zero non-8-bit-per-component images. The palette images seen in earlier
`pdfimages -list` output were Ghostscript's *output* conversions (26 gray→
indexed streams totaling ~15 KB), never inputs. Building nearest-palette
resampling for a class that does not occur in any observed document would be
unmeasured complexity; if a palette-heavy corpus ever shows up, reopen with
that corpus as the benchmark.

Where the remaining gap to Ghostscript actually lives (picamatl 5.55 MB output
census): FlateDecode images 2.54 MB (122 streams, already at 130 DPI,
lossless-by-contract) + JPEG 2.12 MB + fonts/structure 0.64 MB. GS's edge is
its conversion of lossless image payloads to JPEG — exactly the consent-gated
`allow_lossy_reencode` surface reserved as a separate future milestone.

## Explicitly out of scope (unchanged commitments)

- Symbol-mode JBIG2, perceptual quantization tricks: permanently out.
- **DEFAULT-ON** lossy re-encode of lossless images: still out. The
  consent-gated (default-OFF) variant moved to Phase 7 below (2026-08-22
  spike) — the "separate future milestone, requires its own consent surface
  (`allow_lossy_reencode`) and vetting spike" reserved here is exactly what
  Phase 7 implements; flipping any default remains out of scope.
- PDF/A, encrypted docs: skipped as today.

## Phase 6 addendum — requantization reach extensions (2026-08-22)

Byte-census probes on picamatl's own NASA output found two classes of
scanner-quality JPEG that the Phase 5 pipeline never reached. Both fixes are
dimension-preserving requantizations — the same transform D-M1 established,
with the same 5% minimum-savings guard, decode-back verification, and
fail-safe skips:

**P-M1 — Shared-mask requantization.** The shared-mask refcount guard is now
scoped to RESIZE intent only (`SmaskUse::Resize` vs `SmaskUse::Requant`): a
mask referenced by several images still blocks the coupled downsample, but no
longer blocks requantization, which never touches the mask stream and cannot
misalign any consumer. An earlier draft also skipped over-res shared-mask
pairs entirely on a "cementing" rationale; that reasoning was wrong (the
future downsample it protected is blocked by the same shared mask either way)
and stranded ~1 MB of real savings on the reference corpus.

**P-M2 — Under-threshold JPEG quality normalization.** DCTDecode payloads at
or below the DPI threshold were previously left at scanner quality forever;
they now take the same dimension-preserving requantization. FlateDecode bases
remain excluded (lossless contract), bitonal remains owned by the G4 pass.

Measured: picamatl 5,547,684 B (D-M3) → 4,958,148 B on the reference corpus
(70.5% of the 16,804,107 B original) vs Ghostscript's 3,722,562 B — gap now
1.33×. Byte-stable on second pass; all 74 mask/base pairs dimensionally
aligned; renders clean in Ghostscript.

## Phase 7 — consent-gated lossy re-encode (SPIKE, 2026-08-22)

Status: implemented behind a DEFAULT-OFF flag; **final ship decision pends
human review of the visual side-by-sides** in `target/spike/SUMMARY.md`.
Nothing converts without explicit consent.

Design:

- Consent surface: `OptimizeOptions::allow_lossy_reencode` (default `false`,
  builder `with_allow_lossy_reencode`), CLI `--allow-lossy`/`--no-allow-lossy`.
- Scope: UNMASKED FlateDecode images that pass the exact D-M3 decode gates
  (`decode_flate_image`, factored out of `plan_flate`): 8 bpc only, no
  `/Decode`, DeviceGray/DeviceRGB/ICCBased(N=1/3), decodable predictor
  layout, bomb cap + exact-length check. Indexed, CMYK, and 16-bit never
  convert. Masked (`/SMask`) Flate bases are EXCLUDED — coupled lossy
  conversion needs its own mask-alignment analysis (future work; scope line
  pinned by `lossy_reencode_masked_flate_base_is_excluded`).
- Over-resolution: a JPEG candidate at the SAME target geometry competes with
  the format-preserving Flate downsample; the smaller payload wins under the
  existing never-larger guard (`plan_flate_to_jpeg`). With
  `downsample_flate_images` off, geometry is off-limits but the encoding
  class may still change in place (dimension-preserving re-encode).
- Under-threshold: dimension-preserving Flate→JPEG at `jpeg_quality`,
  replaced only on ≥5% savings AND a decode-back MAD verification (the D-M1
  ceiling) — `plan_flate_lossy_requant_replacement`.
- Dict update: new `DictUpdate::FlateToJpeg` (scalar `/Filter /DCTDecode`,
  stale `/DecodeParms` dropped; `/ColorSpace`/`/BitsPerComponent` untouched —
  channel count is preserved so they already describe the JPEG).
- Idempotence finding: the assumed "5% rule ⇒ byte-stable pass 2" was FALSE —
  mozjpeg trellis re-shaves 5-10%/pass on graphics-heavy JPEGs it encoded
  itself. Fixed exactly: `plan_dct_requant` declines candidates whose DQT
  quantization tables are byte-identical to the source's
  (`jpeg_quant_tables`). Hardens shipped D-M1/P-M2 too; corpus outputs
  unchanged.

Measurements (q78 unless noted): NASA 16,804,107 → flag-off 4,958,148 →
flag-on 4,380,087 B (gap to Ghostscript's 3,722,562 B narrows 1.33× → 1.18×);
q85 4,772,830 B. Fixture sample 116,752 → 32,593 B. 19/122 NASA Flate streams
converted (807 KB → 230 KB); byte-stable second passes; gs-clean rendering.
Corpus gaps: the talaria sample and watch-repair one-pagers were unreachable
from this session (sandbox), not skipped by choice.

Review flags from the side-by-sides (candidate no-ships): thin line art
(p12 profiles) picks up JPEG mottling; the p7 banner class compounds loss by
enabling a downsample the lossless path had declined; p39/p40 PSD surfaces
lose mesh detail at q78 (q85 markedly better). Photographic/CFD raster
content converts cleanly.

### Human-review outcome (2026-08-22) — both flagged defects closed

Xchel's review of the composites confirmed two must-fix classes; both are now
fixed in code, tuned and re-measured against the same corpus:

- **Line-art class declined by a content guard.** `looks_like_line_art`
  (background ≥ 0.75, top-8 palette ≥ 0.90, sharp-edge density ≤ 0.08,
  computed in one pass over the decoded SOURCE pixels) removes the JPEG
  candidate for thin-line vector-style content. Measured: p12 profiles
  0.909–0.930 / 0.946–0.956 / 0.061–0.065 → declined; CFD fields (36, 46)
  and PSD surfaces (248–255) and the sample noise stripes all fall far below
  the background threshold and still convert. Because only the JPEG candidate
  is removed, over-resolution line art still takes the lossless downsample:
  the flag-on output for that class equals the flag-off output.
- **Compounding-loss trap closed.** In the over-resolution competition, a
  Flate candidate the never-larger guard would decline now disqualifies the
  JPEG candidate as well. `--allow-lossy` is consent to re-encode, not to
  re-open a resampling decision the lossless path rejected. The p7 TKE
  banners (objs 22–29) consequently keep their original bytes.

Post-fix: NASA flag-on 4,457,789 B (from 4,380,087 B), fixture sample
unchanged at 32,593 B, both byte-stable on a second `--allow-lossy` pass.
Converted NASA streams: 8 (36, 46, 248–251, 254, 255) instead of 19.
Still open (NOT a defect, a default-quality question): the p39/p40 PSD mesh
softening at q78 — `--jpeg-quality 85` retains it and is the better companion
default if that class matters.

## Gates (every milestone)

cargo build && cargo test && cargo test --doc && cargo clippy --all-targets
-- -D warnings, real exit codes; conventional commits; push pre-approved;
CHANGELOG Unreleased entry per milestone; version stays 0.2.1 until release
call.
