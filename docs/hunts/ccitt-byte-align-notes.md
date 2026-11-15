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
- K=0 and K>0 byte-aligned streams stay declined (none in the corpus).
