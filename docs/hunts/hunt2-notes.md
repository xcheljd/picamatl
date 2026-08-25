# HUNT2 — Compression Scout Notes (round 2)

Status: IN PROGRESS. Committed incrementally.
Method: pdffonts / pdfimages -list / pikepdf parsing. No ghostscript on adobe-spec.pdf.
Corpus sizes: adobe-spec 22,491,828 · arxiv-attention 2,233,053 · irs-1040gi 4,434,643 · nist-ssdf 739,891.

## Font audit (pdffonts + pikepdf stream hashing)

amatl src has NO CFF-subsetting of already-CFF (/Type1C) input fonts (grep 'cff' → only Type1→Type1C conversion + CIDFontType0 note "requires show-string rewriting — C-M2/M3"). All corpus CFF embeds are producer-subsetted already, so CFF re-subsetting headroom is small.

Byte-exact duplicate font programs: ZERO in all 4 files (sha256 over decoded stream). Exact-dup dedup table = dead end.

Same-font multiple-subset embeds (grouped by BaseFont minus subset tag; savings = Σ−max heuristic, i.e. merged union subset ≈ largest member):

- adobe-spec: 39 font programs, 209,124B total (0.9% of file). Merge groups: MyriadPro-Semibold x3 (save 8,086), MyriadPro-Regular x3 (6,596), MinionPro-Regular x3 (3,299), MyriadPro-Bold x2 (1,159), Helvetica-Bold x2 (553). TOTAL est ≈ 19,693B. Fonts are NOT the story in this file.
- arxiv-attention: 34 programs, 758,107B (34% of file!). Big win: **ArialMT x5** (52,362×4 + 54,262 = 263,710B) and **Arial-BoldMT x5** (44,832×5 = 224,160B) — five separate embeds (tags SKOJEB/RZEDQD/IYXBUV/FZOJEB/TWYPSZ), near-identical sizes, none byte-equal (per-figure resaves). Est merge save ≈ **388,776B (~17% of file)**. Remaining 24 small CM/Nimbus Type1 subsets ≈ handled by --convert-type1.
- irs-1040gi: 73 programs, 842,089B. Merge groups: TimesNewRomanPSMT x2 raw (57,726), TimesLTStd-Roman x6 (29,497), HNLTStd-Bd x12 (26,216), HNLTStd-Roman x9 (18,540), TimesLTStd-Bold x6 (12,482), TimesLTStd-Italic x5 (10,229), HNLTStd-It x6 (8,458), +7 smaller groups. TOTAL est ≈ **171,721B (~4%)** across ~60 subset fragments of ~15 families.
- nist-ssdf: 10 programs, 629,345B (85% of file!), all singletons, no merge candidates. Producer subsets look fat (e.g. 104,604B CourierNewPSMT, 89,433B) — possible deeper re-subsetting win but needs glyph-usage analysis (not done).

## Duplicate font programs
See above — exact dups: 0 everywhere; family-level multi-embed merge is the real lever (arxiv #1).

## Image census

Method: pikepdf XObject walk (one-off probe scripts, removed 2026-08-25 — superseded by the committed `scripts/h2_*` series). Stored bytes = `len(read_raw_bytes())` (verified vs `/Length`). All FlateDecode images in adobe/nist are `[/Indexed [/ICCBased N=3] ...]`; arxiv's are plain `/DeviceRGB`.

| file | images | total stored | by filter |
|---|---|---|---|
| adobe-spec | 172 | 662,083 B (2.9%) | DCT 128 = 405,789 B · Flate(indexed) 39 = 174,177 B · CCITTfax 4 = 82,115 B · none 1 = 2 B |
| arxiv-attention | 3 | 137,381 B (6.1%) | Flate(/DeviceRGB) 3 = 137,381 B |
| irs-1040gi | 0 | 0 | — (pure text/vector) |
| nist-ssdf | 6 | 20,170 B (2.7%) | Flate(indexed) 6 = 20,170 B |

Findings:

- **gray-in-RGB: ZERO everywhere.** arxiv's 3 DeviceRGB images have R≠G==B on real pixels (checked full decode). Adobe/nist Flate images are all Indexed-with-RGB-base (indices, not RGB samples) — nothing to convert to DeviceGray. Measured savings: **0 B**.
- **Indexed palette shrink:** adobe's 39 indexed images use 83–241 distinct indices each → 4bpc repack impossible except 13 solid-fill icons (uniq=1); measured zlib9 repack saves only **81 B** total. Dead end.
- **nist-ssdf indexed images:** all 6 use ≤3 distinct indices → 4bpc repack + zlib9 **measured save ≈ 3,797 B** (obj100/105/113: −1,162/−1,227/−1,040; obj26/28/30 solid fills: −123/−124/−121). Small but real.
- **arxiv reflate:** its 3 RGB images are already exactly zlib-level-9 optimal (`stored == plain_z9` byte-for-byte; PNG-prediction is *worse*, e.g. 122,622 vs 88,300). Dead end.
- DCT images (adobe, 405,789 B): lossy path already covers; no action in this audit. CCITT fax 82,115 B: bitonal dead end confirmed earlier.

## Rankings

All opportunities ranked by expected corpus savings:

| # | opportunity | files | est/measured savings | notes |
|---|---|---|---|---|
| 1 | Font family multi-subset merge (ArialMT x5 + Arial-BoldMT x5) | arxiv-attention | **≈ 388,776 B** (~17% of file) | from font audit; #1 lever in corpus |
| 2 | Font family merge (TimesLT/HNLT/TimesNR ~15 families × subset fragments) | irs-1040gi | **≈ 171,721 B** (~4%) | from font audit |
| 3 | Font family merge (MyriadPro/MinionPro/Helvetica groups) | adobe-spec | ≈ 19,693 B (<1%) | from font audit |
| 4 | Indexed-image 4bpc repack (uniq≤3, measured zlib9) | nist-ssdf | **3,797 B measured** | small; needs bpc+lookup rewrite |
| 5 | jpegtran DCT coefficient cleanup | corpus | 242 B | known, marginal |
| 6 | Payload-only dup variants | corpus | 61 B | known, marginal |

Corpus total if #1–#4 all land: ≈ 584 KB (~2.1% of 29.9 MB corpus); fonts alone (#1–#3) ≈ 580 KB.

Confirmed dead ends this round: gray-in-RGB conversion (0 B), adobe indexed repack (81 B), arxiv image reflate (0 B, already optimal), plus prior round's bitonal re-encode / ICC pruning / exact font-program dups.

---

# Implementation pass (same round, output-side measurements)

Everything below measures **amatl's output**, not the input, using
`scripts/h2_*.py` (bench, byte census, TTF table dump, TTF near-dup diff, CFF
dup, depth-reduction repack, family-merge headroom, verify harness). Gates for
every lossless claim: `pdftoppm` sha256 render-identity, pass-2 idempotence,
`gs -sDEVICE=nullpage`, plus `cargo test --release` / clippy / fmt.

## Where the bytes are in the *default output* (`scripts/h2_census.py`)

| file | biggest buckets (share of output) |
|---|---|
| adobe-spec 7.17 MB | content streams 48% · ObjStm 24% · **XMP metadata 12%** · DCT 4% · Type1C 2% |
| arxiv-attention 1.55 MB | **font programs 24%** · content 18% · Flate images 13% · 2,513 form XObjects 12% |
| irs-1040gi 4.26 MB | 2 DCT images 37% · content 27% · ObjStm 23% · TrueType 5% · Type1C 3% |
| nist-ssdf 561 KB | content 44% · **TrueType 32%** · Flate images 13% · ObjStm 9% |

The 12% XMP bucket in adobe-spec is invisible in an input-side image/font
audit and turned out to be the single largest addressable item in the corpus.

## Shipped

### 1. `name`-table subset-tag masking → duplicate TT subsets dedup (2eca424)

The "arxiv ArialMT ×5" group above does **not** need union subsetting to
collapse most of the way. After amatl's own subsetting, those programs are
byte-identical **except** the six-letter `ABCDEF+` tag inside the `name` table
and `head.checkSumAdjustment` (which the tag perturbs) — glyf/loca/cmap/hmtx
match exactly (`scripts/h2_ttf_diff.py`). Masking the tags to `AAAAAA` and
repairing both checksums makes them byte-equal, so the existing stream dedup
shares one program. The subset tag a viewer reads comes from `/BaseFont`,
which amatl already rewrites from a content hash.

arxiv-attention 1,553,042 → **1,475,800** (−77,242, −5.0%): 4 of 5 Arial-Bold
embeds and 3 of 5 ArialMT collapsed. irs −26, nist −53, adobe +1.

### 2. `--strip-metadata` (opt-in, off by default) (64b28fb)

adobe-spec carries **134 XMP packets, 860 KB, 12%** of its optimized output —
one per page/XObject, none consulted to render. Removing every `/Metadata`
entry (orphans then pruned): adobe-spec 7,166,167 → **6,289,645**
(−876,522, −12.2%); nist −1,637; irs −616. Render-identical, but discards
provenance and breaks PDF/A and PDF/UA identification, hence opt-in beside
`--strip-accessibility`.

## Recommended, not implemented

- **Same-family union subsetting.** Headroom *remaining after* the tag-masking
  dedup, Σ−max per family measured on the output
  (`scripts/h2_family_merge.py`): irs-1040gi **108,621 B (2.5%)**, adobe-spec
  18,009 (0.25%), arxiv-attention 13,983 (0.9%), nist 0. So the ranking-table
  #1 (arxiv, est 388 KB input-side) is now ~90% collected by the cheap fix,
  and the residual lever is #2 (irs). irs's fragments are CFF/Type1C with
  per-subset encodings, so merging means rewriting show-string codes — the
  C-M2/M3 work item, not a scout-sized change.
- **Lossy quality ladder (retry q85/q90 when the q78 MAD guard declines).** Not
  evaluated: no in-repo corpus image is declined (irs's two DCTs dominate but
  are already DCT), and the photo-heavy NASA file is outside sandbox-readable
  paths. Still the most promising lossy lever.

## Dead ends measured in this pass (<1%, dropped per brief)

- **CFF (`/Type1C`) dedup keyed on bytes-minus-Name-INDEX**
  (`scripts/h2_cff_dup.py`) — the CFF analogue of the shipped TT fix: irs
  8,259 B (0.19%), adobe 0, arxiv 0; exact CFF dups 0. Does not pay.
- **Lossless bit-depth reduction on the output** (`scripts/h2_depth.py`, real
  repack + re-deflate): nist **1,700 B (0.3%)**, adobe 117 B, arxiv/irs 0.
  (The input-side estimate of 3,797 B above shrinks once amatl's own
  downsampling has already run.) `/DeviceGray` images on an n-bit ladder:
  none. Confirms the image-census verdict with post-pipeline numbers.
- **Form-XObject dedup beyond exact:** arxiv has 2,513 form XObjects (median
  60 B raw, 180 KB total) and 2,513 **distinct** payloads — no redundancy.
- **`/PieceInfo`:** absent from the corpus.

## Font-table overhead observed (not acted on)

`scripts/h2_ttf_tables.py` over nist's 10 subsets: glyf 174,688 B, but hinting
(`prep` 37,538 + `fpgm` 20,197 + `cvt ` 19,586 = **77 KB decoded**) and `name`
12,926 B ride along. Dropping hinting is a real win on hinting-heavy files but
changes rasterization at small sizes — not lossless under the pixel-identity
contract; it would belong behind a lossy flag.

## Final table — input → output (bytes)

| file | input | default | + `--strip-metadata` | + all lossless opt-ins¹ | default + zopfli | `--strip-metadata` + zopfli |
|---|---|---|---|---|---|---|
| adobe-spec | 22,491,828 | 7,166,167 | 6,289,645 | 6,289,645 | ZA | MA |
| arxiv-attention | 2,233,053 | 1,475,800 | 1,475,800 | 1,268,302 | ZR | MR |
| irs-1040gi | 4,434,643 | 4,262,520 | 4,261,904 | 4,261,904 | ZI | MI |
| nist-ssdf | 739,891 | 561,239 | 559,602 | 559,602 | ZN | MN |
| dummy | 13,264 | 12,459 | 12,459 | 12,459 | ZD | MD |

¹ `--strip-metadata --convert-type1 --recompress-bitonal-images`.

New flag this round: `--strip-metadata` / `--no-strip-metadata` (library
`OptimizeOptions::with_strip_metadata`). No default behaviour changed except
the tag masking, which is unconditional and lossless.
