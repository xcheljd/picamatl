# Content-stream re-deflate on PDF/A-labelled files (measure-first notes)

Question: page content streams in PDF/A-labelled files are left exactly as
they arrived, because `minify_content_streams` and `redeflate_flate_streams`
both decline when `pdfa` is true. What does lifting each gate buy on the five
PDF/A files, and can either part change something PDF/A cares about?

Base: local main `7636820` (reference binary
`workspace/bin/picamatl-7636820`). Validator: veraPDF 1.30.4 (greenfield,
run-only, portable JRE), each file in the flavour it claims: usgs-bul1000,
usgs-bul1100, usgs-pp150 = PDF/A-1b; usgs-pp250, nbs-circ539v1 = PDF/A-2b.

Builds (local, uncommitted for B and C):

- A: re-deflate lifted for page content streams only (every stream a page
  names in `/Contents`); every other stream of a PDF/A file still declined.
- B: A plus the `minify_content_streams` gate lifted (the whole minifier).
- C: the `redeflate_flate_streams` gate lifted for every Flate stream.

Lanes: `def` (defaults) and `all` (every opt-in except `--strip-metadata`, so
the PDF/A label is kept: `--strip-accessibility --strip-private-data
--convert-type1 --strip-hinting --recompress-bitonal-images --allow-lossy
--collapse-gray-images --flatten-forms`).

## Sizes (bytes)

defaults:

- nbs-circ539v1: original 6,422,795; main 6,422,795 (passthrough); A
  6,422,795 (passthrough); B 6,378,712; C 6,422,795
- usgs-bul1000: original 4,262,675; main 3,254,163; A 3,249,493 (-4,670);
  B 3,236,752 (-12,741 vs A); C 3,249,488 (-5 vs A)
- usgs-bul1100: original 8,138,904; main 5,002,466; A 4,978,672 (-23,794);
  B 4,978,361 (-311 vs A); C 4,978,658 (-14 vs A)
- usgs-pp150: original 24,914,333; main 17,498,218; A 17,372,395
  (-125,823); B 17,371,651 (-744 vs A); C 17,372,381 (-14 vs A)
- usgs-pp250: original 10,287,328; main 10,107,174; A 10,092,860
  (-14,314); B 10,092,738 (-122 vs A); C 10,092,860 (0 vs A)

all options, label kept: the four USGS files are byte-for-byte the same sizes
as in defaults; nbs-circ539v1: main 6,390,259; A 6,378,734 (-11,525); B
6,378,712 (-22 vs A); C 6,378,728 (-6 vs A).

Totals vs main: A -168,601 B (defaults), -180,126 B (all options). B adds
13,940 B over A in the all-options lane, where every file is rewritten either
way. In defaults B adds 58,001 B, but 44,061 B of that is nbs-circ539v1 being
rewritten at all: nothing else counts as work there (main and A both pass it
through), and a minified page counts as work. C adds 33 B (defaults) / 39 B
(all) over A: no Flate stream other than page content has real headroom in
these files.

The lead's Python probe (zlib-9 per content stream) estimated about
-197,900 B. The shipped number is lower because picamatl's own deflate
backend is what runs, nbs-circ539v1 is not rewritten in defaults, and each
stream keeps its stored bytes unless the re-deflate is strictly smaller.

The nbs-circ539v1 defaults passthrough is the existing contract: the
re-deflate is a serialization-time pass and never counts as work on its own
(`serialization_wins_do_not_rewrite_an_otherwise_unchanged_file`), for
PDF/A and non-PDF/A files alike.

## veraPDF

45 runs: the five originals and main/A/B/C x def/all. All 45 are compliant
with 0 failed rules (PDF/A-1b: 129 rules passed; PDF/A-2b: 144 rules
passed). The PDF/A-1 outputs keep the classic xref table and unfiltered
catalog XMP from main.

## What `minify_content_streams` changes

1. Page content: the stream (or every element of a `/Contents` array,
   joined with a newline) is strictly parsed, re-emitted by lopdf (one
   operator per line, single spaces between operands) and the original
   number literals are spliced back in their shortest decimal-exact form
   (`1.0` -> `1`, `-0.36` -> `-.36`, `8.10` -> `8.1`). Comments and extra
   whitespace go; lopdf re-spells literal strings (`(\(A\) )` ->
   `((A) )`, same bytes). A `/Contents` array is merged into ONE new stream
   when every element is referenced only by that page; the page's
   `/Contents` then points at the new stream and the old elements are
   pruned. `/Filter` becomes `/FlateDecode` or is removed, `/DecodeParms` is
   removed. Declined per page: strict-parse failure, any inline image (`BI`),
   a re-parse that differs by value, or a result that is not strictly
   smaller than the re-deflated original.
2. Form XObjects: the same re-emit, in place (same object id, same dict
   apart from `/Filter` / `/DecodeParms` / `/Length`).

Census, A vs B, all-options outputs (`minify-census.py`, pikepdf, pages
matched by index):

- usgs-bul1000: 55/55 pages re-emitted, every one a 2-element `/Contents`
  array merged into one stream; 9,666 numbers re-spelled, 4,297 of them
  reals that became integers (`1.0` -> `1`); strings re-spelled on 51 pages.
- usgs-bul1100: 57/124 pages re-emitted; 13,341 numbers re-spelled, none
  real -> integer; 19,184 marked-content operators before and after.
- usgs-pp150: 67/171 pages; 39,481 numbers re-spelled, none real ->
  integer; 34,788 marked-content operators before and after (OCR
  `/Suspect <</Conf 0>> BDC ... EMC`).
- usgs-pp250: 1/149 pages, a 2-element array merged; 2 reals -> integers.
- nbs-circ539v1: 5/104 pages; 10 numbers re-spelled.

Every re-emitted page parses (pikepdf) to the same operators and operand
values as before. No file has a structure tree, `/StructParents`, MCIDs,
page `/Group` or page-level Form XObjects; page dictionary key sets are
unchanged.

### Could it touch something PDF/A cares about?

- Marked content and structure: operators and operands are preserved by
  value, so BMC/BDC/EMC and MCIDs survive; `/StructParents` lives in the page
  dict, which is untouched. A marked-content reference that names a content
  stream with `/Stm` gives that stream a second reference, so an array that
  contains it is not merged; Form XObjects keep their object id.
- Transparency: page `/Group` and ExtGState names are untouched.
- `/Contents` array shape: no PDF/A rule constrains it.
- Implementation limits (ISO 19005-1 6.1.12, ISO 19005-2 6.1.13): YES, in
  principle. A real with a zero fraction is re-spelled as an integer. PDF/A
  caps integers at 2^31-1, while PDF/A-2 and -3 allow reals up to about
  3.4e38, so a legal PDF/A-2 literal such as `3000000000.0` would become an
  out-of-range integer and a new validator failure. None of the five files
  has such a literal, but the minifier cannot be shown safe in general.

Decision (card rule: minification ships only if it adds more than 20 KB in
total AND cannot change a PDF/A-relevant property): NOT shipped. It adds
13.9 KB like-for-like and fails the second condition. Only the lossless
re-deflate of page content streams (A) ships; the minifier stays declined on
PDF/A files and is listed for the owner's decision with these numbers.

Scripts (pica-builder `workspace/reports/pdfac/`): `run-lanes.sh`,
`vera-run.sh`, `vera-summary.py`, `minify-census.py`, `string-diff.py`.

## Results: the shipped change (A), committed binary

Commit `7748746` (code + tests; `c1f9d9b` adds only docs), reference binary
`workspace/bin/picamatl-pdfac-c1f9d9b`. Its outputs are byte-identical to
the measurement build A on all 96 runs below.

32-file accuracy set (`workspace/acc-files.txt`), main `7636820` vs this
branch, three lanes (all 96 runs exit 0):

- defaults: 28 files byte-identical; usgs-pp150 -125,823 B, usgs-bul1100
  -23,794 B, usgs-pp250 -14,314 B, usgs-bul1000 -4,670 B. nbs-circ539v1 is
  a passthrough on both. Total 96,140,482 B -> 95,971,881 B (-168,601 B).
- all options, label kept: 27 files byte-identical; the same four plus
  nbs-circ539v1 -11,525 B. Total 86,672,826 B -> 86,492,700 B (-180,126 B).
- kitchen (all options with `--strip-metadata`, claim removed): all 32
  byte-identical, 85,364,280 B.

No output is larger than main's or than its input; a second pass of the
branch binary over each of its 96 outputs is byte-identical.

Changed outputs (9: four in defaults, five in all options), each against
main's output of the same lane:

- decoded page content: every page's `/Contents` shape and every stream's
  decoded bytes identical (pikepdf);
- renders: every page identical at 100 and 300 dpi (pdftoppm, sha256 per
  page); against the ORIGINAL at 100 dpi the branch differs on exactly the
  pages main does (bul1100 4, pp150 23, the rest 0);
- `pdftotext` identical; Ghostscript `nullpage` exit 0 with no messages.

Positive control: the bul1000 output with a 20 pt black square appended to
page 3's content is flagged on page 3 only, in the decoded content and at
both resolutions.

Stale trailer keys (`workspace/scripts/stale.py`): all 96 outputs clean.

veraPDF on the committed binary's outputs, flavour as claimed:

- usgs-bul1000 1b: original pass, main pass, branch pass (defaults and all)
- usgs-bul1100 1b: original pass, main pass, branch pass (defaults and all)
- usgs-pp150 1b: original pass, main pass, branch pass (defaults and all)
- usgs-pp250 2b: original pass, main pass, branch pass (defaults and all)
- nbs-circ539v1 2b: original pass, main pass, branch pass (defaults and all)

PDF/A-1b: 129 rules passed, 0 failed; PDF/A-2b: 144 passed, 0 failed, on
every file in every column.

## Fuzzing

`cargo +nightly fuzz build -O` (exit 0). Seeds: every `optimize`,
`optimize_all` and `optimize_nopack` seed was timed through the release CLI
(best of 3); 36 `optimize` seeds at 100 ms or more were moved to
`corpus/optimize.slow/` (none deleted), none in the other two. Twenty new
PDF/A-labelled seeds went into `optimize` and `optimize_nopack` (2.6 to
5.5 ms each): PDF/A-1 and PDF/A-2 claims on two small form fixtures with
their page content stored at zlib level 0, and synthetic 1-3 page files
shaped like the unit-test fixture (content at level 0, one page in two as
a 2-element `/Contents` array, a pair of identical images). Each one comes
out smaller from this branch than from main.

`MINUTES=10 ./campaign.sh`: optimize 188 runs, optimize_all 360,
optimize_nopack 21,297, ccitt 30,746; 0 oom/timeout/crash, no findings.
It exited 1 because this worktree had no corpus dirs for the nine small
targets (DID NOT RUN). Those nine then ran 10 minutes with the campaign's
flags from empty corpora: jpeghuff, type1, cff, cff_font, truetype,
deflate, content, lzw and jbig2, 205,628 to 55,680,280 runs each, no
findings. The pipeline targets get few runs in the campaign because the
sanitizer's first pass over 2,000+ seeds takes most of the 10 minutes, so
a focused 10-minute pass followed with 100 seeds per target (the 10 new
ones plus 90 random PDF/A-labelled corpus seeds): optimize 87,720 runs,
optimize_nopack 30,947, no findings.
