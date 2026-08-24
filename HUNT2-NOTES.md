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
(TBD)

## Rankings
(TBD)
