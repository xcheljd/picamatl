# Pre-push documentation refresh (2026-11-17)

Every measured number in README.md, docs/ARCHITECTURE.md's "Reproducing
benchmarks" and the new CHANGELOG lines was re-run on 2026-11-17 against
`main` at `4e85845` (release build, `cargo build --release`, binary sha256
`0543423c…6302e5`), Ghostscript 10.08.0, poppler pdftoppm/pdfimages, on a
16-core machine. The README tables had dated from 2026-08-24 (0.4.0).

## Inputs

- The 16-file corpus (`corpus/`, `corpus-expanded/`), all matching
  `corpus.sha256` (`sha256sum -c corpus.sha256`: 16 OK).
- NASA TM-20210010291, 16,804,107 B (not in the repository; re-downloadable
  from ntrs.nasa.gov).
- `fixtures/sample.pdf` (four pages) and the 0.1.0 two-page fixture from git
  history: `git show 54872f9:fixtures/sample.pdf` (193,668 B).

## Commands

- `scripts/bench-full.sh` (all five lanes, every picamatl run exited 0).
  Its matrix printer crashed with `KeyError: 'amatl'` (a rename leftover);
  fixed in this commit and re-run over the same outputs.
- One `picamatl <flags> -o out/<lane>/<file>.pdf <file>` per lane and file:
  - lossless: no flags
  - `--allow-lossy`
  - `--strip-accessibility`; `--allow-lossy --strip-accessibility`
  - kitchen sink of the five/six-document tables (their footnote, which is
    NOT a lane of bench-full.sh): `--allow-lossy --strip-accessibility
    --strip-metadata --convert-type1 --strip-hinting
    --recompress-bitonal-images --collapse-gray-images --deflate-backend
    zopfli`
  - kitchen sink of the consolidated table (= bench-full.sh kitchen):
    the above plus `--strip-private-data --flatten-forms`
  - `--flatten-forms`, `--strip-metadata`, `--strip-private-data`,
    `--figure-dpi 195`, `--target-dpi 0`
- Ghostscript forced lossy: the exact `gs` command of bench-full.sh
  (`-dCompatibilityLevel=1.5`, 130 DPI, threshold 1.15, `/DCTEncode`,
  QFactor 0.4). Ghostscript `/ebook`: `gs -q -dNOPAUSE -dBATCH
  -sDEVICE=pdfwrite -dPDFSETTINGS=/ebook` (as in forms-vs-gs.sh).
- `scripts/bench-vs-gs.sh` and `scripts/bench-vs-gs.sh <two-page fixture>`.
- `scripts/forms-vs-gs.sh` (with a Python that has pikepdf on `PATH`).
- `scripts/forms-verify.py <original> <output> 150` for irs-w2 (defaults,
  `--flatten-forms`, consolidated kitchen sink, gs `/ebook`) and
  census-brief (defaults, `--flatten-forms`, kitchen sink); the same at
  150 dpi for wiki-pdf `--target-dpi 0`.
- Timings: three wall-clock runs each on a quiet machine, alternating lanes.

bench-full.sh's lossless and kitchen outputs are byte-identical to the
per-lane runs; its gs outputs are within 2 bytes of the separate gs runs.

## Results (main at 4e85845)

NASA: defaults 4,297,986 B (25.6%, 74.4% saved; 1.07-1.21 s), a second
pass byte-identical; `--allow-lossy` 3,254,078 (19.4%, -24.3% vs
defaults); consolidated kitchen sink 2,945,043 (17.5%); `--figure-dpi 195`
5,258,830 (31.3%); `--target-dpi 0` 15,587,943 (92.8%); gs `/ebook`
4,931,396 (29.3%); gs forced 3,691,333 (22.0%).

Five-document corpus, % of input (adobe-spec, arxiv-attention,
irs-1040gi, nist-ssdf, dummy, total):
- lossless 30.5, 65.8, 58.6, 75.2, 93.9 = 38.4
- `--allow-lossy` identical
- + `--strip-accessibility` 23.8, 65.8, 38.0, 69.9, 93.9 = 30.2
- kitchen (footnote) 18.9, 52.9, 31.4, 47.4, 88.6 = 24.0
- gs forced 48.8, 55.5, 72.2, 82.1, 114.1 = 53.6
- the 0.4.4 release binary, lossless: 31.9, 66.1, 62.2, 75.9, 93.9 = 40.0

Six-document corpus (arxiv-gpt4, census, cmyk-jpeg, irs-w2, nist-sp800,
wiki, total):
- lossless 37.9, 51.2, 78.1, 63.7, 58.0, 44.4 = 48.1
- `--allow-lossy` 31.3, 51.2, 78.1, 63.7, 57.6, 30.8 = 42.6
- + `--strip-accessibility` 31.3, 43.2, 78.1, 60.6, 53.1, 25.9 = 40.3
- kitchen (footnote) 16.6, 35.9, 67.4, 57.4, 39.6, 22.7 = 30.3
- gs forced 28.4, 60.0, 4.6, 8.8, 70.1, 205.3 = 63.1

Consolidated kitchen sink, all 16 files: 11,882,097 B of 56,196,624
(21.1%); gs forced 29,541,711 (52.6%). Smaller than gs on 14 of 16; the
two exceptions are the dynamic XFA forms. Against gs `/ebook` (22,797,444
B, 40.6%): lossless defaults (21,562,045 B, 38.4%) are smaller on 7 of 16,
the kitchen sink on 12 of 16 (not arxiv-attention 957,557 vs 216,743,
cmyk-jpeg 11,919 vs 8,398, or the two dynamic XFA forms). Lossless
defaults are smaller than gs forced on 9 of 16.

Forms: irs-w2 defaults 1,369,502; `--flatten-forms` 229,153; kitchen +
flatten 124,220; gs `/ebook` 183,761. Page renders vs the original at 150
dpi: defaults and flatten differ on page 1 only (0.54% of its pixels), all
inside the 300-ppi header JPEG that defaults downsample to 130 ppi (the
0.4.4 binary downsamples it too, so this is not new behaviour; the
README's old "0 of 11" is replaced); kitchen + flatten adds glyph-edge
differences on the same page from `--strip-hinting` (1.34%); gs `/ebook`
differs on 11 of 11 (up to 0.56%). census-brief: 9 of 9 pages identical in
all three lanes. `--flatten-forms` output is byte-identical to defaults on
14 of 16 files (12 without `/AcroForm`, 2 declined dynamic XFA).
xfa_filled: picamatl 343,055 B (88.7% saved, 9,298 data nodes), gs
`/ebook` 4,157 B (0 data nodes).

Other README numbers: sample.pdf 117,398 B (17.7%) vs gs 71,742 (10.8%);
two-page fixture 19,889 (10.3%) vs gs 43,725 (22.6%), image payload 18,885
vs 40,164 B at the same pixel dimensions; adobe-spec output carries 134 XMP
packets, 860,660 B, 12.5% of 6,860,106 (`--strip-metadata` saves 876,207);
`--strip-private-data` saves 261,445 of cmyk-jpeg's 292,284 (89%) and
239,817 on arxiv-attention; nist-sp800-63b `--figure-dpi 195` +8,322 B
(858,015 → 866,337), pages 1 and 26 change, MAD vs the original 0.178 →
0.070 and 0.106 → 0.046 (pdftoppm gray, 150 dpi); figure-dpi timing
(median of 3) irs-1040gi 4.15 → 4.27 s, adobe-spec 3.87 → 3.88 s;
wiki-pdf `--target-dpi 0` renders 0 of 28 pages differently.

## Findings worth knowing

- Ghostscript 10.08.0 does not reproduce 10.07.1's numbers on three files
  with the same command: NASA 3,691,333 (10.07.1: 3,054,642), wiki-pdf
  205.3% of input (37.1%), wiki-cmyk-topic 494.4% (65.8%). On the two
  Wikipedia files the gs-custom lane (PDF 1.7, `LeaveColorUnchanged`) gives
  the same sizes. Four NASA command variants (no CompatibilityLevel,
  `/ebook` + the forced settings, no QFactor dictionary, mono downsampling
  added) gave 3,691,329 / 4,040,314 / 2,530,750 / 3,691,383 B, none
  3,054,642. The README
  quotes 10.08.0 and footnotes 10.07.1.
- Ghostscript `/ebook` is not lossless on NASA: 60 non-JPEG images in the
  input, 48 in its output. The README no longer calls it "matched intent".
- The five-document table's irs-1040gi lossless cell (92.6%) did not match
  even the 0.4.4 release (62.2%); its 0.4.0 description "scanned imagery"
  was also wrong (two JPEGs, no CCITT). The current 58.6% is not JBIG2:
  `--no-jbig2-bitonal-images` gives the same bytes.
- The real-literal example was described as a LibreOffice file; it is the
  Chrome-printed (Skia) Wikipedia article.
- The image-downsampling planners take no PDF/A argument, so they run on
  PDF/A-labelled files; ARCHITECTURE.md had said every re-encoding pass
  declines on them. The docs now list the gated passes exactly.
