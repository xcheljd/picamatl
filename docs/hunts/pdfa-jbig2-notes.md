# JBIG2 re-store on PDF/A-labelled files: validator check

Question: may the lossless CCITT -> JBIG2 re-store run on documents that
claim PDF/A without harming the claim? Checked with veraPDF 1.30.4
(greenfield CLI, run as an external checker only) on Temurin JRE 21.0.13.

## Which corpus files claim PDF/A

Read from each file's catalog `/Metadata` (decoded XMP):

- nbs-circ539v1.pdf  PDF/A-2B  (JBIG2 masks + JPX, no CCITT)
- usgs-bul1000.pdf   PDF/A-1B  (55 CCITT G4 page scans)
- usgs-bul1100.pdf   PDF/A-1B  (CCITT with /EncodedByteAlign, out of scope)
- usgs-pp150.pdf     PDF/A-1B  (JBIG2 + JPEG, no CCITT)
- usgs-pp250.pdf     PDF/A-2B  (JBIG2 + JPX, no CCITT)

The other 27 corpus files carry no PDF/A claim.

## Validator results, defaults

Flavour = the one each file claims. (a) original, (b) main e615168,
(c) main + the JBIG2 exemption.

- nbs-circ539v1 2b: (a) pass  (b) pass  (c) pass
- usgs-pp250    2b: (a) pass  (b) pass  (c) pass
- usgs-bul1000  1b: (a) pass  (b) FAIL 6.1.4-3 x1, 6.7.2-2 x1  (c) FAIL 6.1.4-3 x1, 6.7.2-2 x1
- usgs-bul1100  1b: (a) pass  (b) FAIL 6.1.4-3 x1, 6.7.2-2 x1  (c) same as (b), output byte-identical
- usgs-pp150    1b: (a) pass  (b) FAIL 6.1.4-3 x1, 6.7.2-2 x1  (c) same as (b), output byte-identical

Rules:

- ISO 19005-1:2005 6.1.4-3 "Xref streams shall not be used" — from the
  default `pack_object_streams` save path.
- ISO 19005-1:2005 6.7.2-2 "The Metadata object stream dictionary in the
  document's catalog shall not contain the Filter key" — `doc.compress()`
  deflates the (originally unfiltered) XMP stream. The decoded XMP is
  byte-identical to the original; only its encoding changes.

Both failures exist on main without this change: the JBIG2 exemption adds
no failed rule and raises no count. Because main's own output already fails
PDF/A-1b, the defaults comparison on PDF/A-1 files is "no new failures",
which is weaker than "stays compliant".

## Isolating the JBIG2 embedding

usgs-bul1000 with `--no-pack-object-streams` (classic xref table, header
stays %PDF-1.4): (b) and (c) both fail only 6.7.2-2 x1.

Then the (c) output with only its catalog XMP stream rewritten unfiltered
(probe, pikepdf; every other stream kept as written): PASS, 129/129 rules,
55 JBIG2 images. So veraPDF accepts JBIG2Decode image XObjects in a
PDF/A-1b file and raises nothing about the PDF version (PDF/A-1 is based on
PDF 1.4; the JBIG2 path raises pre-1.4 headers to 1.4, which bul1000
already has).

The same table, re-run on the final build (291fe8a + docs): identical to
the one above, rule for rule and count for count.

## Sizes, 32-file set, main e615168 vs this change

- defaults: 31 files byte-identical; usgs-bul1000 4,226,153 B ->
  3,225,145 B (-1,001,008 B, -23.7%). Total 98,615,606 B -> 97,614,598 B.
- kitchen (all options incl. `--strip-metadata`): 32 files byte-identical
  (the strip removes the claim, so JBIG2 already ran there). Total
  85,868,502 B.
- all options without `--strip-metadata`: 31 byte-identical; usgs-bul1000
  4,225,489 B -> 3,224,480 B (-1,001,009 B). Total 88,268,158 B ->
  87,267,149 B.

Changed files: second pass byte-identical; all 55 JBIG2 images decode
bit-identical through jbig2dec (padding bits masked); all 55 pages render
identical at 100 dpi against the original; gs nullpage clean; pdftotext
identical. The catalog XMP decodes byte-identical to the original.

## What /EncodedByteAlign CCITT still leaves (out of scope)

usgs-bul1100 (PDF/A-1b) stores 120 page images as CCITT G4 with
`/EncodedByteAlign true`, which the strict decoder declines. Probe: every
one re-stored as plain G4 with the same samples (Pillow/libtiff; 124/124
pages render identical, 251/251 bitonal images pixel-identical), then run
through this build: all 120 ship as JBIG2, 1,542,752 B smaller than the
byte-aligned G4 they replace in the default output (6,511,029 B).
