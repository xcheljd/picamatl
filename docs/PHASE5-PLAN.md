# Phase 5 Plan — Transparency-Aware Compression (closing the Ghostscript gap)

Status: DRAFT for review
Date: 2026-08-22
Baseline evidence: NASA TM-20210010291 (16,804,107 B) head-to-head vs Ghostscript 10.07.1

## Measured problem

amatl 0.2.1 (strip, no packing): 11,511,039 B (68%).
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
9.16 MB masked-JPEG class to fall to roughly 1.5-2.5 MB. amatl total lands
around 4-6 MB vs gs 3.7 MB — competitive band reached.

## D-M3 — Flate masked-image handling

Same unit rule for the 37 Flate+SMask images (2.14 MB): downsample in place
(existing Flate path extended to carry the mask along) or leave if already
≤130 dpi. Lower priority: smaller pool, existing path does most work once
eligibility opens.

## D-M4 — Indexed/palette and low-bpc images (stretch)

Currently skipped (src/lib.rs:633 M1 scope note). GS re-encodes these too.
Palette-preserving downsample requires nearest-palette mapping — nontrivial
color science, do only after D-M1..3 land and re-measure.

## Explicitly out of scope (unchanged commitments)

- Symbol-mode JBIG2, perceptual quantization tricks: permanently out.
- Default-on lossy re-encode of lossless images: separate future milestone,
  requires its own consent surface (`allow_lossy_reencode`) and vetting spike.
- PDF/A, encrypted docs: skipped as today.

## Gates (every milestone)

cargo build && cargo test && cargo test --doc && cargo clippy --all-targets
-- -D warnings, real exit codes; conventional commits; push pre-approved;
CHANGELOG Unreleased entry per milestone; version stays 0.2.1 until release
call.
