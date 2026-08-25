# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`--strip-private-data` (`OptimizeOptions::strip_private_data`, off by
  default)** — removes every `/PieceInfo` page-piece dictionary, where an
  authoring application keeps its own private copy of the artwork beside the
  flattened page that draws it (ISO 32000-1 14.5). Illustrator's
  `AIPrivateData` is the extreme case: on `corpus-expanded/cmyk-jpeg.pdf` it is
  **261,453 B — 96% of amatl's entire output for that file**, taking it from
  67.4% of input to 3.3%; `corpus/arxiv-attention.pdf` sheds a further
  239,955 B. Pixel-identical (verified page by page at 100 dpi on all three
  affected corpus files); what it costs is round-trip editability in the
  producing application, hence opt-in, same posture as `--strip-metadata`.

### Changed

- **The signature guard on the three entropy-level passes is gone.**
  `redeflate_flate_streams`, `reoptimize_jpeg_streams` and
  `minify_content_streams` used to decline any document carrying a `/Sig`
  dictionary, a `/ByteRange`, or an AcroForm `/SigFlags`. That guard protected
  nothing: amatl re-serializes the entire document on every run, so every file
  offset moves and an offset-pinned digest is already invalid in the output —
  font subsetting, image downsampling, ObjStm packing and metadata stripping
  were never gated on signatures in the first place. Verified on
  `corpus-expanded/irs-w2.pdf`, a Reader-extended IRS form: the `/UR3`
  signature's `/ByteRange` in amatl's *previous* output already pointed past
  EOF. Removing the incoherent guard is worth **507,344 B (28% of that file's
  output)**, almost all of it the 1.5 MB XFA attachment that shipped with the
  producer's weak deflate. The README now states plainly that optimizing a
  signed PDF invalidates its signature.

### Fixed

- **One Type3 glyph disabled every font optimization in the document.**
  `lopdf`'s content parser does not know `d0`/`d1`, the glyph-metric operators
  that open a Type3 `/CharProcs` stream: it reads `d1` as the operator `d`
  plus a stray number that binds as the *first operand of the next operator*,
  and a char proc that ends right after `d1` fails to parse at all. Either
  outcome aborted the font-usage walk, and one abort discards every font plan
  in the file. `corpus-expanded/arxiv-gpt4.pdf` shipped 713 KB of font
  programs untouched because of a single 22-byte char proc. The metrics prefix
  is now split off before parsing — a leading run of numeric tokens at the
  operator's exact arity, nothing else — so the operator sequence the walker
  inspects is unchanged and every other parse failure still declines. Worth
  **693,711 B (41% of that file's output)**; 100/100 pages verified
  pixel-identical at 72 dpi.

### Fixed

- **CMYK/YCCK JPEGs were re-encoded to RGB under an unchanged `/DeviceCMYK`
  `/ColorSpace`.** `decode_jpeg_scaled` declined four-component payloads, but
  every caller then fell back to the general-purpose `image` decoder, which
  does *not* decline — it converts CMYK to RGB. The three resulting channels
  were written back into a stream whose dict still declared four, producing a
  corrupt page. Reproduced on `corpus-expanded/cmyk-jpeg.pdf`: rendered
  through Ghostscript, the old output differed from the original by up to 251
  levels across 5.8% of the page, confined exactly to the image bounding box.

  Four-component streams now take a dedicated end-to-end path — decode to raw
  CMYK samples with libjpeg's explicit `JCS_CMYK` output, resample all four
  channels with the same Lanczos3 kernel the RGB path uses, re-encode as YCCK
  with libjpeg writing the Adobe APP14 marker itself. Everything stays in raw
  stored-sample space, so amatl never parses APP14 and never inverts or
  reorders a channel: whatever ink convention the input used, the output uses
  the same one. Verification is per-channel (the shared MAD ceiling of 96
  cannot catch a four-channel swap — a C/K swap pools to 29), truncated
  streams are caught structurally rather than trusted to libjpeg's lenient
  grey-fill "repair", and any `/Decode` array declines the stream. A
  dict/payload cross-check (found by a byte-mutation sweep) additionally
  declines any `/DCTDecode` image whose declared colour-space component count
  disagrees with its frame header where four components are involved —
  including a frame header too damaged to read under a four-component colour
  space.

  This is not new consent surface: CMYK images take the same default-on
  downsample and requant paths RGB images already took, under the same
  quality, never-larger and 5%-minimum guards. The correctness fix costs size
  on the one affected corpus file (76.7% → 78.2% lossless, 65.9% → 67.4%
  kitchen sink); the old numbers were bought with the corruption above. RGB
  and grayscale output is byte-for-byte identical on every other corpus file.
  See [`docs/CMYK-JPEG.md`](docs/CMYK-JPEG.md).

### Added

- **JPEG Huffman-table re-optimization, on by default, strictly lossless.**
  Every raw `/DCTDecode` payload now has its Huffman tables rebuilt from its
  own symbol statistics — the pure-Rust equivalent of `jpegtran -optimize`.
  DCT coefficients are never decoded, dequantized, or re-quantized: only the
  entropy coding of an unchanged symbol sequence changes, and the pass
  verifies that by decoding its own output back and requiring the identical
  token sequence. Nothing else in the JPEG moves (`APPn`, `COM`, `DQT`,
  `SOF`, `DRI` and the `SOS` headers are copied verbatim, restart markers
  keep their MCU boundaries). Because it is bit-exact it needs no consent
  flag. The headroom is in JPEGs the image path passes through untouched —
  above all the `/Decode`-carrying CMYK and Separation payloads it declines to
  re-encode:
  irs-1040gi 4,158,663 → 4,104,994 (−53,669 B, −1.3%). Streams amatl
  re-encoded itself carry mozjpeg's already-optimal tables and decline.
  Scope is baseline/extended sequential Huffman frames at 8-bit precision;
  progressive, arithmetic-coded, and hierarchical frames decline and ship
  byte-identical.

- **`strip_metadata` (`--strip-metadata`), opt-in, off by default.** Removes
  every `/Metadata` (XMP) entry. Producers that stamp a full packet on each
  page and XObject spend a double-digit share of the file on data no viewer
  reads to render: adobe-spec carries 134 packets, 860 KB, 12% of its
  optimized output (6,289,645 with the flag vs 7,166,167 without). Visually
  lossless, but it discards provenance and breaks PDF/A and PDF/UA
  identification, hence opt-in.

### Changed

- **`strip_hinting` (`--strip-hinting`) now covers Type1C (CFF) fonts too.**
  The flag used to be TrueType-only. It now also removes the Type2 hint
  operators (`hstem`, `vstem`, `hstemhm`, `vstemhm`, `hintmask`, `cntrmask`)
  and their operands from every `/FontFile3` `/Subtype /Type1C` program —
  including ones the same run produced by union-merging fragments or
  converting Type1 — plus the Private DICT hinting parameters (`BlueValues`,
  `StemSnap*`, …) that describe nothing once the charstring hints are gone.
  Charstrings are rewritten token by token: coordinate deltas are spliced
  through in their original encoding, no outline is re-encoded and no
  subroutine is inlined or dropped, and each glyph's outline and advance are
  re-verified from the *emitted* bytes before the program is accepted. Glyph
  names, glyph order and advance widths never change, so the strip is inert to
  every font dictionary pointing at the program. Measured over 80 corpus
  Type1C programs: 77 stripped, 3 declined (they carry no hints), 2,393 glyphs
  outline-verified, 0 mismatches. With `--strip-hinting --convert-type1`:
  adobe-spec −22,419 B, irs-1040gi −7,263 B, arxiv-attention −6,167 B
  (−35,849 B total, of which −3,213 B is the Private DICT keys). Same consent
  as before — hinted rasterization at small sizes can change — but the flag's
  scope is wider, so re-read it if you had assumed TrueType-only.

- **Duplicate TrueType subsets now dedup.** After subsetting, several embeds
  of the same subset of the same font differ only in the six-letter `ABCDEF+`
  subset tag inside the `name` table and the `head.checkSumAdjustment` it
  perturbs. Those tags are now masked (and both checksums repaired), so the
  programs are byte-equal and the existing stream dedup shares one copy. The
  tag a viewer sees comes from `/BaseFont`, which amatl already rewrites from
  a content hash. arxiv-attention 1,553,042 → 1,475,800 (−5.0%).

- **Simple-TrueType font subsetting.** `subset_fonts` now also subsets
  nonsymbolic simple TrueType fonts (`/Encoding` WinAnsiEncoding or
  MacRomanEncoding, incl. explicit-base `/Differences`), not just
  Type0/CIDFontType2 Identity-H/V. Content-stream text bytes, `/Encoding`,
  `/Widths`, and `/ToUnicode` never change: the subset font (built by
  `subsetter`, which drops `cmap` by design) gets a freshly synthesized
  `cmap` replicating the original's subtables — restricted to retained
  glyphs, glyph ids remapped — so every viewer lookup path of ISO 32000-1
  9.6.6.4 ((3,1) via glyph name → Unicode, (1,0) via glyph name → Mac OS
  Roman code, any (3,0) present) resolves each used code to the same outline
  as before, verified by a parse-back round-trip at plan time. Encoding
  tables and the Adobe Glyph List subset were extracted programmatically
  from Ghostscript's authoritative resources and cross-checked against
  Python's cp1252/mac_roman codecs. Fail-safe posture unchanged: symbolic
  flags, absent `/Encoding` or `/Widths`, unknown glyph names, unsupported
  cmap formats (anything outside {0, 4, 6, 12}), shared descriptors/font
  files, or a used code the cmap paths cannot resolve leave that font
  untouched. On the NASA corpus all 11 embedded simple TrueType fonts
  subset (pool 327,544 → 208,985 B stored), −115,595 B whole-file, with all
  58 pages rendering bit-identical (pdftoppm sha256) to the non-subset
  output.

### Changed

- **`subset_fonts` defaults to ON** (was opt-in since 0.2.1). Subsetting is
  rendering-preserving and render-verified, text extraction stays
  bit-identical, and the structure tree is untouched, so it joins the
  lossless defaults. Opt out with `with_subset_fonts(false)` /
  `--no-subset-fonts`. Measured on NASA: −124,486 B vs `--no-subset-fonts`.
- **Deflate backend switched to `zlib-rs`** (`flate2` feature `zlib-rs`,
  pure Rust). Better ratios at the same level 9 across every re-deflated
  stream; the existing strictly-smaller + verified-roundtrip guard in
  `redeflate_flate_streams` still gates every replacement, so no stream can
  regress by construction. Measured on NASA: −82,722 B at defaults.
  Dependency floor unchanged (zlib-rs 0.6.7 declares rust-version 1.75,
  inside the 1.88 MSRV).
- NASA TM-20210010291 measured results (defaults): 16,804,107 →
  **4,448,544 bytes (73.5% saved)**, byte-stable on pass 2, vs 4,655,752 at
  0.3.1 — and 9.8% under Ghostscript `/ebook` at matched intent.
  `--allow-lossy` q78: 3,342,293 (19.9% of input).

- **Soft-masked FlateDecode bases reach JPEG under `--allow-lossy`.** A
  FlateDecode image carrying an eligible 8-bit DeviceGray `/SMask` is no longer
  excluded from the consent-gated Flate→JPEG conversion. It reaches it two ways,
  and every masked pair now settles in a SINGLE pass:

  - **At its own geometry** (the pair is not over-resolution, or the coupled
    downsample is unavailable): the conversion is **dimension-preserving** and
    rewrites the base stream only. The `/SMask` object keeps its bytes and its
    `/Width`/`/Height`, so base and mask stay aligned by construction, and a
    **shared** mask is safe for exactly the reason the P-M1 requant is safe for
    one — nothing about the mask changes.
  - **At the target geometry** (the pair is over-resolution and takes the D-M3
    coupled downsample): the downsample now carries a JPEG competitor computed
    at the SAME target pixels the mask is losslessly resampled to, and the
    smaller base candidate wins. This sits on the resize side of the P-M1 line
    and widens nothing: the mask is resampled either way, so the path stays
    behind the existing shared-mask refcount guard.

  All existing guards apply unchanged — the line-art content check on the
  decoded source pixels, decode-back MAD verification, the 5% minimum-savings
  rule (combined over the pair on the coupled path), and atomic base+mask
  replacement. So does no-compounding-losses: if the fully lossless pair
  candidate would itself be declined, no JPEG candidate is computed, and after
  a decline nothing lossy is retried. `--allow-lossy` is consent to re-encode,
  not consent to re-litigate a resampling decision the lossless path made.

  **One-pass idempotence.** Because the competitor lands the conversion inside
  the downsample, pass 2 sees a DCTDecode base at the target geometry rather
  than an at-target masked *Flate* base, so `optimize(optimize(x))` is
  byte-identical to `optimize(x)` under the flag. Without it the same total
  harvest arrived split across two passes.

  Alignment was re-validated for the resampling path (the prior compositing
  experiment only covered dimension-preserving conversion under an unmodified
  mask): on the NASA output all 74 masked pairs report base dims == mask dims,
  the mask streams are byte-identical to what the flag-off downsample produces,
  and on the three affected pages (23, 34, 43) the flag-off↔flag-on render
  error at edges (5.5 / 3.3 / 1.8 MAD) stays well under the resample error
  flag-off already accepts (14.5 / 11.1 / 3.6), i.e. the residual is JPEG
  quantization — which this flag consents to — and not misregistration.

  Measured on NASA at `--allow-lossy`, otherwise default, ONE pass:
  **4,155,393 → 3,506,379 B (−649,014 B)**, byte-stable on a second pass.
  Flag-off output is unchanged (4,655,752 B, byte-identical to 0.3.1).

## [0.3.1] - 2026-08-22

Three lossless serialization wins from the compression investigation. No pixel
is resampled, no encoding class moves, and no consent flag is involved. On the
58-page NASA reference (16,804,107 B) at otherwise-default settings, combined:
**4,958,148 → 4,655,752 B (−302,396 B, −6.10%)**, byte-identical on a second
pass, all 58 pages render clean under Ghostscript 10.07.1.

### Changed

- **BREAKING (default flip, 0.x minor):** `OptimizeOptions::pack_object_streams`
  now defaults to `true` (it was `false` through 0.3.0). Output is packed into PDF 1.5 `ObjStm` streams unless
  you opt out with `--no-pack-object-streams` (library:
  `.with_pack_object_streams(false)`). Lossless — same objects, same semantics,
  different serialization — and measured at **−163,394 B (3.29%)** on NASA,
  where the structure tree is intact and 2,180 of 2,497 objects are packable.
  **The cost is a PDF 1.5 floor:** a reader older than Acrobat 6 (2003) cannot
  open an `ObjStm` file *at all* — a hard failure, not a degradation. The
  previous struct doc claimed packing "buys only a couple of percentage points";
  that caveat described the post-`strip_accessibility` case (few objects left to
  pack) and is now stated as such.

### Added

- **Final re-deflate pass (on by default).** Every stream whose `/Filter` is
  exactly `FlateDecode` is inflated and re-deflated at zlib level 9, keeping the
  result only when it is strictly smaller AND inflates back byte-identically.
  `/Filter` and `/DecodeParms` are untouched, so predictors still apply to the
  same post-inflate bytes and every reader decodes exactly what it decoded
  before. This reaches streams that arrived with a producer's weaker deflate
  output — which `doc.compress()` never touched, by its own "only streams
  without a `/Filter`" rule — and gets a second look at amatl's own output.
  Measured **−128,645 B (2.59%)** on NASA. Declined wholesale for encrypted,
  PDF/A-declaring (`fonts::pdfa_blocked`), and signed (`/ByteRange`, `/Type
  /Sig`, AcroForm `/SigFlags`) documents. Runs after ALL planning, purely at
  serialization time. Zopfli remains out of scope (~50× compression time for
  roughly 233 KB more; a separate future flag).

- **Compressed cross-reference stream.** lopdf hardcodes
  `XRefStreamFilter::None` in `writer.rs::write_cross_reference_stream`, and the
  object never exists in `Document::objects` (the writer synthesizes it during
  save), so no `SaveOptions` knob and no document-level pass could reach it:
  amatl shipped a raw 7-bytes-per-entry xref stream. It is now deflated in both
  save paths — NASA packed **17,486 → 5,804 B**, unpacked **17,479 → 7,122 B**.
  Patching the serialized bytes is sound because the xref stream is the last
  object in the file and `startxref` (plus its own xref entry) records its
  *start* offset, which does not move. Every structural assumption is verified
  against the bytes actually present, and any mismatch — including a classic
  cross-reference table — passes the file through untouched. `/W [1 4 2]` is
  left as lopdf emits it; narrowing it was explicitly out of scope.

### Note

The serialization-time passes (packing, re-deflate, xref compression) are not
counted as "work" by `try_optimize`'s early return, unchanged from how packing
has always behaved: a document where nothing semantic was planned still comes
back byte-for-byte identical, preserving the "declined everything ⇒ your exact
bytes" property. Pinned by
`serialization_wins_do_not_rewrite_an_otherwise_unchanged_file`.

[Unreleased]: compare from 0.3.0.

## [0.3.0] - 2026-08-22

### Added

- **Consent-gated lossy Flate→JPEG re-encode (Phase 7 spike) — SHIPPED BEHIND
  A DEFAULT-OFF FLAG, pending human review of the visual side-by-sides**
  (`target/spike/SUMMARY.md`). New `OptimizeOptions::allow_lossy_reencode`
  (default `false`, builder `with_allow_lossy_reencode`; CLI `--allow-lossy` /
  `--no-allow-lossy`): when — and only when — enabled, unmasked 8-bit
  DeviceGray/DeviceRGB/ICCBased(N=1/3) FlateDecode images may change encoding
  class to `/DCTDecode`. Over-resolution images get a JPEG candidate at the
  same target geometry as the format-preserving downsample and the smaller
  payload wins; at/below-threshold images are re-encoded at their own
  geometry, replaced only on ≥5% savings plus decode-back verification (the
  D-M1 MAD ceiling). Dict updates go through a new `DictUpdate::FlateToJpeg`
  (scalar `/Filter /DCTDecode`, stale `/DecodeParms` dropped; `/ColorSpace`
  and `/BitsPerComponent` already match — channel count is preserved).
  Indexed/CMYK/non-8-bit images and `/SMask`-carrying bases are never
  converted; corrupt payloads return exact original bytes. Measured (q78):
  NASA 4,958,148 → 4,380,087 B (gap to Ghostscript 1.33× → 1.18×), fixture
  sample 116,752 → 32,593 B; byte-stable on repeat passes.

  Post-review hardening (both defects found in the visual side-by-sides):

  - **Line-art content guard.** `looks_like_line_art` declines the lossy
    conversion for thin-line vector-style images — dominant flat background
    ≥ 75% of pixels, top-8 quantized colors ≥ 90%, sharp-edge density ≤ 8% —
    measured in one pass over the decoded SOURCE samples (never resized ones,
    where resampling has already blurred what the metrics key on). The
    5%-savings rule is no protection here: a JPEG of line art beats a mediocre
    deflate almost every time, which is exactly how the NASA p12 plug profiles
    (objs 59–61) acquired background mottling, muddied dash-dot lines and
    hairline color shift. Measured on the review corpus: p12 line art
    bg 0.909–0.930 / palette 0.946–0.956 / edges 0.061–0.065 → DECLINED;
    CFD velocity fields (36, 46) bg 0.201/0.413 → still convert; 3D PSD
    surfaces (248–255) bg 0.522–0.538 → still convert; synthetic noise
    stripes bg 0.001–0.008 → still convert. The guard removes only the JPEG
    candidate, so over-resolution line art still takes the lossless
    downsample and lands byte-identical to the flag-off output.
  - **No compounding losses.** When the JPEG candidate competes against the
    format-preserving downsample, a Flate candidate that the never-larger
    guard would decline now disqualifies the JPEG candidate too. Consent to
    re-encode is not consent to re-litigate a resampling decision the lossless
    path rejected — previously the NASA p7 TKE banners (objs 22–29) took both
    losses at once (resolution + DCT) because the JPEG win hid a downsample
    that had grown the stream. Those streams now keep their original bytes.

  Post-fix measurements (q78): NASA 16,804,107 → 4,457,789 B (was 4,380,087
  before the guards; flag-off 4,958,148), fixture sample unchanged at
  32,593 B; both still byte-stable on a second `--allow-lossy` pass.

- **Requantization reach extensions (Phase 6).** Two classes of
  scanner-quality JPEG the Phase 5 pipeline never reached now take the
  dimension-preserving requantization: masked images whose `/SMask` is shared
  by several bases (the refcount fail-safe now blocks only resizing, which is
  the only transform that can misalign other consumers — P-M1), and
  unmasked/masked DCTDecode payloads at or below the DPI threshold, which are
  quality-normalized in place instead of being left at scanner grade forever
  (P-M2). Same guards as D-M1 throughout: strict-smaller + 5% minimum savings,
  decode-back verification, untouched mask streams, byte-stable repeat passes.
  Reference corpus: 5.55 MB → 4.96 MB (70.5% reduction from the original
  16.8 MB).

### Fixed

- **The decoded-payload stream dedup no longer over-merges.**
  `dedup_decoded_streams` keyed only on the inflated bytes, so two streams
  with the same payload but *different dictionaries* collapsed into one and
  the lowest-id dictionary silently won. On the 756-page Adobe PDF 1.7 spec
  this merged five image pairs that must stay distinct: two with transposed
  dimensions (65x66 vs 66x65 over identical index bytes) and three sharing
  index bytes under different `/Indexed` palette objects — which recolored
  the overprint illustration on page 752, among others. The key is now
  (decoded bytes, dictionary minus `/Length`); `/Length` stays out because
  differing stored lengths are exactly what this pass exists to collapse.
  Measured cost: +1,248 B on the Adobe spec, 0 B on the IRS, arXiv, and NIST
  corpora. With downsampling disabled, all 21 previously-differing spec pages
  now render byte-identical to the original at 72 dpi.

- **`--deflate-backend zopfli` re-deflates every `ObjStm`.** Capping objects
  per stream at 65,535 made large files emit more than one object stream, and
  the zopfli patch declined outright on anything but a lone `ObjStm` — so the
  payloads stayed zlib (Adobe spec 6,461,220 -> 6,918,902 B; IRS 3,805,245 ->
  4,090,288 B). Every `ObjStm` is now patched, strictly last-to-first so each
  patch only shifts bytes after the streams still to come; every existing
  guard is retained per stream and applied independently. Adobe spec
  6,454,219 B, IRS 3,803,479 B — both below the old single-stream figures.

- **Requantization is now exactly idempotent.** `plan_dct_requant` declines a
  candidate whose JPEG quantization tables are byte-identical to the
  source's: same tables mean the payload is already at the configured
  quality, so a re-encode is pure generation-loss churn. The 5% rule alone
  did not converge once the lossy spike started producing our own q78
  payloads — mozjpeg's trellis re-shaves 5–10% per pass on graphics-heavy
  content it encoded itself (NASA repro: second pass shrank 4,380,087 →
  4,371,056 before the guard). Shipped D-M1/P-M2 corpus outputs are
  unchanged.

### Changed

- **`SmaskUse` eligibility split.** `eligible_smask` now takes a usage intent;
  the shared-mask refcount guard applies to resize-intent lookups only.

[0.3.0]: nothing yet.

## [0.2.2] - 2026-08-22

[0.2.2]: nothing yet.

### Added

- **SMask-coupled Flate-base downsampling (Phase 5 D-M3).** Over-resolution
  FlateDecode images with an eligible `/SMask` are now downsampled with the
  same atomic pair mechanics as D-M2: the base goes through the existing
  format-preserving Flate→Flate path (same `/ColorSpace`, predictor handling
  unchanged), the mask through the same resample-to-Flate path, both to the
  SAME target geometry — replaced together or not at all. The combined
  never-larger/5% minimum-savings guard, the shared-mask fail-safe, and every
  D-M1 skip rule (`/Matte`, stencils, ineligible mask shapes, corrupt
  streams) apply unchanged, and the pair honors `downsample_flate_images`.
  Pairs already at/below the DPI threshold are left byte-for-byte untouched —
  there is no requantization analogue for lossless Flate payloads (that would
  be a lossy re-encode, which still has no consent surface). On the 16.8 MB
  NASA reference report this takes amatl from 6.03 MB (D-M2) to 5.55 MB
  (5,547,684 B) vs Ghostscript's 3.72 MB, byte-stable across passes.

- **SMask-coupled downsampling (Phase 5 D-M2).** Over-resolution JPEG images
  with an eligible `/SMask` are now downsampled as an atomic pair: base
  (Lanczos3 → JPEG at `jpeg_quality`) and mask (Triangle → plain FlateDecode
  8-bit DeviceGray) are resampled to the SAME target geometry and replaced
  together — never one side alone. The combined candidate must beat the pair's
  original size by the full 5% minimum, and both halves pass decode-back
  verification (exact for the lossless mask). A mask referenced by more than
  one image is never resized (fail-safe: dedup merges byte-identical masks
  before planning, so sharing is reachable in practice), and all D-M1 skip
  rules (`/Matte`, stencils, ineligible mask shapes, corrupt streams) carry
  over. On the 16.8 MB NASA reference report this takes amatl from 11.51 MB
  (D-M1) to 6.03 MB vs Ghostscript's 3.72 MB, byte-stable across passes.

- **SMask-aware JPEG requantization (Phase 5 D-M1).** JPEG (`DCTDecode`)
  images carrying an `/SMask` soft mask are no longer unconditionally skipped:
  when the mask resolves to a plain 8-bit DeviceGray image stream with no
  `/Matte` (and is not an `/ImageMask` stencil), the base JPEG is decoded at
  its OWN dimensions and re-encoded at `OptimizeOptions::jpeg_quality` —
  never resized, so mask alignment is untouched by construction, and the
  `/SMask` stream itself is never modified. Every hard rule from the Phase 5
  contract is enforced: strict smaller-only replacement (never-larger guard),
  decode-back pixel verification of the re-encoded base before replacement,
  `/Mask` color-key/stencil images and all ineligible `/SMask` shapes
  (unresolvable reference, non-image object, non-DeviceGray color space,
  non-8-bit samples, `/Matte` anywhere in the pair, FlateDecode bases — those
  are covered by the D-M3 entry above) are left byte-for-byte untouched, and the panic-safe
  `catch_unwind` boundary is unchanged. Attacks the 9.16 MB masked-JPEG class
  on the reference corpus at zero geometric risk.

- **`amatl` CLI.** The crate now ships a command-line binary (`src/main.rs`)
  over the same library API: positional input, `-o/--output` (default
  `<input>.optimized.pdf`, never overwriting without `--force`), and flags
  mapping 1:1 to every `OptimizeOptions` builder
  (`--target-dpi`, `--jpeg-quality`, `--dpi-margin`, and `--flag` /
  `--no-flag` pairs for all five boolean options). All defaults — including
  the boolean on/off states printed under `--help` — are read from
  `OptimizeOptions::default()` at runtime so CLI and library cannot drift.
  Non-PDF input is rejected by `%PDF-` header sniffing; success reports
  input → output bytes, percent saved, and elapsed time; failures exit
  nonzero with a clean message (no panics on user-facing paths).

## [0.2.1] - 2026-08-22

[0.2.1]: nothing yet.

### Added

- **Opt-in lossless bitonal→G4 recompression (Phase 3 B-M1).**
  `OptimizeOptions::recompress_bitonal_images` (default `false`) and the
  `with_recompress_bitonal_images` builder losslessly re-encode 1-bit images
  to CCITT G4 via the `fax` 0.3.0 crate (vetted in docs/PHASE3-PLAN.md §B.3:
  MIT, `#![deny(unsafe_code)]`, zero transitive dependencies — promoted from
  dev-dependency to runtime dependency). Two source shapes: CCITT-stored
  (`/K -1` G4, and EOL-framed `/K 0` G3 1D with `/EndOfLine true`) and
  Flate-stored 1-bit images, including `/ImageMask` streams. Pixels are never
  resampled; `/BlackIs1` polarity is normalized at the sample level; a stream
  is replaced only when the G4 payload is strictly smaller (including
  `/DecodeParms` overhead) **and** an in-process decode-back pass reproduces
  the source samples bit-for-bit. All parameter doubt is a fail-safe skip:
  `/EncodedByteAlign true`, `/K > 0`, EOL-less `/K 0`, `/EndOfBlock false`,
  non-identity `/Decode`, geometry mismatches, and damaged streams (strict
  row-count + EOFB/RTC accounting — a short stream is never re-encoded as
  white-padded data).

- **Opt-in font subsetting (Phase 3 C-M1).**
  `OptimizeOptions::subset_fonts` (default `false`) and the
  `with_subset_fonts` builder subset embedded Type0/CIDFontType2
  (Identity-H/V) fonts to the glyphs the document actually shows, via
  `subsetter` 0.2.6 (`default-features = false`; one transitive dependency).
  The `/CIDToGIDMap`-stream technique replaces only `/FontFile2` and
  `/CIDToGIDMap` (plus a deterministic `TAG+` name): content-stream text
  bytes are **never rewritten**, and `/W`, `/DW`, and `/ToUnicode` stay
  untouched, so text extraction is bit-identical pre/post (pinned by tests
  and a Ghostscript/pdftotext harness in `scripts/verify-fonts.sh`).
- Resource-aware glyph discovery over all content-bearing stream classes:
  pages, form XObjects (recursive, cycle-guarded), annotation appearance
  streams, tiling patterns, and Type3 char procs. Any parse failure anywhere
  disables subsetting for the whole document; any per-font doubt (shared
  descendant/descriptor/font program, non-Identity encoding, CFF/CFF2,
  unresolvable map, glyphs out of range, not net-smaller) leaves that font
  untouched. PDF/A-declared (`pdfaid` XMP) and encrypted documents are
  skipped entirely.
- OFL-licensed fixture font (`fixtures/fonts/NotoSans-Regular.ttf` +
  `fixtures/fonts/OFL.txt`) powering the verification battery: per-glyph
  outline and advance equality via `ttf-parser` (dev-dependency only),
  composite-glyph closure, CIDToGIDMap-stream inputs, multi-page
  accumulation, and walker coverage of forms/appearances.

### Fixed

- **`optimize(optimize(x))` is now byte-stable on real-world files
  (idempotence).** Duplicate-object merging ran exactly one generation per
  call: merging duplicate leaf objects (e.g. repeated ColorSpace entries)
  remaps references, which can make their *parents* — image streams, then
  dicts referencing those — newly byte-identical, and that next generation of
  merges was left to the next `optimize` call. Repro: a 16 MB NASA scan where
  pass two removed 7 more objects (−3,725 bytes). The non-stream and stream
  dedup passes now alternate to a fixpoint inside a single call (each
  effective round strictly removes at least one object, so termination is
  guaranteed).
- **Page-tree nodes are never deduplicated, even when byte-identical.**
  Surfaced by the fixpoint change on the same NASA file: two blank pages with
  identical dicts (after their empty content streams and thumbnails merged)
  were collapsed into one object, putting the same id in `/Kids` twice —
  which changes what GoTo destinations resolve to, and breaks lopdf 0.42's
  `renumber_objects_with` page-reordering pass (it assumes distinct page ids
  and silently overwrote an unrelated scanned page, orphaning its 1.37 MB
  image subtree). Page identity is load-bearing; `/Type /Page` and
  `/Type /Pages` dicts are now excluded from dedup. The same hazard was
  latent in earlier releases for documents whose pages are byte-identical at
  load time.

### Notes

- Fonts amatl does not subset are always left byte-for-byte untouched:
  simple (non-Type0) fonts and predefined CJK CMaps are permanently out of
  scope; embedded CMap streams and CIDFontType0 (CFF) are future milestones.
- The default-ON flip for `subset_fonts` is planned only after a clean
  corpus soak (see docs/PHASE3-PLAN.md, decision #3).

## [0.2.0] - 2026-08-21

### Added

- **FlateDecode image downsampling, on by default.** Over-resolution
  Flate-compressed raster images (the PNG-like class: screenshots, exported
  bitmaps, lossless scans) are now downsampled to the effective target DPI *in
  the same format* — decoded (including PNG predictors 10–15), resized with
  Lanczos3, and re-deflated at maximum compression, choosing the smaller of a
  PNG Up-predictor and a plain no-predictor encoding. `/ColorSpace` is never
  touched, so no JPEG-class artifacts can be introduced. Scope: 8-bit
  DeviceRGB / DeviceGray / ICCBased (N = 1 or 3) images without `/SMask`,
  `/Mask`, or `/Decode`; everything else (Indexed, 1/2/4/16-bit, TIFF
  Predictor 2, array-form `/DecodeParms`) is deliberately left byte-for-byte
  untouched.
- `OptimizeOptions::downsample_flate_images` (default `true`) and the
  `with_downsample_flate_images` builder, for callers who need the 0.1.x
  behavior (`false` restores it exactly).
- Decompression-bomb guard on the Flate path: a hard 256 MiB raw-pixel cap
  enforced *during* inflation, plus an exact decoded-length check — any
  mismatch (truncated stream, lying dimensions) leaves the image untouched.

### Changed

- **Behavior change:** documents containing over-resolution Flate images now
  produce different (smaller) output than 0.1.x by default. Pin
  `downsample_flate_images = false` for bit-stable output across the upgrade.
- The committed test fixture (`fixtures/sample.pdf`) grew from two JPEG pages
  to four pages (two JPEG + two FlateDecode), exercising both pipelines.

### Fixed

- PNG predictor inversion is performed by amatl itself (spec-correct,
  including the Avg filter) instead of lopdf 0.42's
  `Stream::decompressed_content`, whose Avg-filter implementation produces
  incorrect pixels and which returns partial data on corrupt zlib streams.

## [0.1.0] - 2026-08-21

### Added

- Initial release: CTM-aware effective-DPI downsampling of over-resolution
  DCTDecode (JPEG) images via mozjpeg, duplicate object/stream deduplication,
  optional accessibility-tree stripping, optional PDF 1.5 object-stream
  packing, and the hard fail-safe contract (never larger, never corrupt,
  original bytes on any failure).
