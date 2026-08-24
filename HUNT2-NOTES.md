# HUNT2 — Compression Scout Notes (round 2)

Status: IN PROGRESS. Committed incrementally.
Method: pdffonts / pdfimages -list / pikepdf parsing (scripts_hunt2_fontdup.py). No ghostscript on adobe-spec.pdf.
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

Method: pikepdf XObject walk (`scripts_hunt2_imagecensus.py`, `scripts_hunt2_csprobe.py`, `scripts_hunt2_idxprobe.py`, `scripts_hunt2_arxiv_reflate.py`). Stored bytes = `len(read_raw_bytes())` (verified vs `/Length`). All FlateDecode images in adobe/nist are `[/Indexed [/ICCBased N=3] ...]`; arxiv's are plain `/DeviceRGB`.

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
