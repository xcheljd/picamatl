# Compression #10: /EncodedByteAlign CCITT (notes)

Branch `compress/ccitt-byte-align`, based on local main `7b8a288`.

## Phase 1: census (measure first)

`scripts/census-ccitt-parms.py` over the 32 files of the accuracy set
(`acc-files.txt`), image XObjects only, stored (encoded) bytes:

- adobe-spec.pdf: K=-1 EncodedByteAlign=true ImageMask, BlackIs1 false,
  EndOfBlock default: 4 images, 82,115 B (281 x 601 each).
- usgs-bul1100.pdf (PDF/A-1b): K=-1 EncodedByteAlign=true Image (DeviceGray),
  BlackIs1 false: 120 images, 5,704,952 B (about 2,200 x 3,560 each).
- No other file has a byte-aligned CCITT image. No K=0 or K>0 byte-aligned
  stream exists in the set (no K>=0 CCITT image at all).

Totals: 124 byte-aligned images, 5,787,067 B, all G4 (K=-1).
Non-aligned G4 for comparison: 4,020 images, 41,495,665 B in 11 files.

Stream shape (`tails`, all 124): every stream ends in an EOFB that starts on
a byte boundary (`00 10 01`) and is the last three bytes of the stream; no
trailing bytes. Blank rows are one byte each (`0x80`: V0 plus seven fill
bits).

Reachable saving: the earlier probe (Pillow/libtiff re-store as plain G4,
then the JBIG2 pass) measured 1,542,752 B on usgs-bul1100 and 61,046 B on
adobe-spec, 1,603,798 B together, over the 500 KB stop bar. The build below
re-measures it in-pipeline.

## Scope decisions

- Byte-aligned G4 (K<0) is decoded by an in-house T.6 decoder
  (`src/ccitt.rs`) that reads the fax crate's public code tables through its
  public `BitReader` trait and does its own reference-line coding and bit
  positioning. Non-aligned streams keep the existing fax `Group4Decoder`
  path, so their output is unchanged.
- Strictness: fill bits must be zero; exactly /Height rows; a byte-aligned
  EOFB right after them; nothing after the EOFB; no uncompressed
  (extension) mode; every coding step must move a0 strictly right (the
  T.4 constraints a conforming encoder obeys). Anything else is untouched.
  Valid but non-canonical sequences (a vertical code where pass mode was
  due) are decoded: T.4 defines their pixels. A unit test first assumed
  any flipped bit would be caught and found one that turned
  `Pass Pass VR1` into `Pass VR3` with identical pixels; the test now
  flips the recorded fill bits only.
- K=0 and K>0 byte-aligned streams stay declined (none in the corpus).

## Results (branch head, against main 7b8a288 built from ~/Labs/picamatl)

Decoder checks (all 124 byte-aligned images):
- picamatl's decode vs poppler (`pdfimages` of the original) and vs
  Ghostscript (each original stream alone on a w x h pt page at 72 dpi):
  0 differing pixels on all 124, padding masked, /Decode handled. Controls:
  one flipped pixel = 1 diff, one flipped padding bit = 0. pdfimages writes
  /ImageMask stencils as their raw sample bits (PBM 1 = sample 1) and
  DeviceGray images as 1 = black; gs paints both black where sample = 0.
- Continuous mode vs fax's strict path on all 4,020 non-aligned G4 images
  in the 32 files: 4,020 identical, 0 differ, 0 one-sided.
- jbig2dec on every JBIG2 stream in the new outputs (defaults and
  kitchen): 124 of 124 match picamatl's decode (controls pass).

Sizes, 32 files (main defaults total 97,756,865 B, as on the card):
- defaults: 96,140,482 B (-1,616,383 B); kitchen: 85,868,502 B ->
  84,252,108 B (-1,616,394 B). 30 files byte-identical in both lanes; the
  two changed files are the two with byte-aligned images. Second pass
  byte-identical on 32 of 32 in both lanes.
- usgs-bul1100 defaults 6,557,236 B -> 5,002,466 B (-23.71%), kitchen
  6,445,944 B -> 4,891,145 B (-24.12%).
- adobe-spec defaults 6,921,719 B -> 6,860,106 B (-0.89%), kitchen
  4,311,367 B -> 4,249,772 B (-1.43%).

Renders (pdftoppm, every page): new == main output on 124/124 (bul1100)
and 756/756 (adobe-spec) pages at 100 and 300 dpi. Against the original,
new differs only where main also differs: bul1100 pages 1, 2, 59, 60
(downsampled JPEGs); adobe-spec 19 pages (9 with downsampled images; the
other 10 are main's existing changes on pages 211, 311-315, 543-545, 548,
e.g. the 33 x 33 JPEG thumbnails of 311-315, max channel diff 3). Main
stores all 124 byte-aligned streams unchanged, so in their areas new ==
main == original. Text identical, gs nullpage clean, no new poppler
syntax errors.

veraPDF default outputs: usgs-bul1000, usgs-bul1100, usgs-pp150 (1b) and
usgs-pp250, nbs-circ539v1 (2b) all compliant, 0 failed rules.

Worst case (17 KB PDF declaring a 16384 x 16384 near-blank byte-aligned
image, one black pel per 64 lines): 11.35 s, 102 MB peak RSS; the same
pixels as continuous G4 already take 11.33 s on main, so the cost is the
JBIG2 search, not the decoder. Blank: 3.5 s (continuous on main 2.9 s).
268M x 1: 0.01 s, 70 MB. 16385 x 16384 (over the pixel cap): declined.

Fuzz: MINUTES=10 campaign, 12 targets incl. the new `ccitt` (72,691
runs); optimize 10,706 and optimize_all 12,632 runs on 100 seeds each
under 100 ms (30 byte-aligned one-image PDFs, 29 of which convert);
0 oom/timeout/crash, 0 findings.
