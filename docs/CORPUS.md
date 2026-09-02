# Benchmark corpus

Every document the benchmark tables in the README reference, with its public
source and SHA-256. Fetch them yourself, verify the hashes, and run
`scripts/bench-full.sh` — every number in the README is reproducible from
these files. None of the documents are committed to this repository.

The corpus deliberately spans the classes that stress different parts of the
pipeline: LaTeX-generated papers (arXiv), US Government forms (IRS), NIST
publications, a scanned form, Wikipedia's print renderer, CMYK-heavy
press-ready documents, and LiveCycle dynamic XFA forms.

## Files

| file | size (bytes) | SHA-256 (first 16) | source |
|---|---:|---|---|
| adobe-spec.pdf | 22,491,828 | `9de0ca9e8570d620` | Adobe PDF Reference 1.7 ("PDF32000.book"), distributed with the ISO 32000-1 documentation |
| arxiv-attention.pdf | 2,233,053 | `aa7a3201a45567fd` | [arXiv:1706.03762](https://arxiv.org/abs/1706.03762) — Attention Is All You Need |
| arxiv-diffusion.pdf | 10,267,274 | `aee5e07a802e8dfd` | [arXiv:2006.11239](https://arxiv.org/abs/2006.11239) — Denoising Diffusion Probabilistic Models |
| arxiv-gpt4.pdf | 5,245,564 | `c33a66dadca2388d` | [arXiv:2303.08774](https://arxiv.org/abs/2303.08774) — GPT-4 Technical Report |
| census-brief.pdf | 545,684 | `c8eaf0b676935ce9` | US Census Bureau, "Congressional Apportionment" brief (census.gov) |
| cmyk-jpeg.pdf | 374,080 | `659d6b19912f63db` | CMYK JPEG test document (Adobe Illustrator sample; also circulated in Mozilla pdf.js test discussions) |
| dummy.pdf | 13,264 | `3df79d34abbca993` | minimal LibreOffice Writer export, generated for this corpus |
| irs-1040gi.pdf | 4,434,643 | `482e9c487c608f1b` | IRS 2025 Form 1040 instructions (irs.gov) |
| irs-w2.pdf | 2,150,352 | `61eca7c81f16d396` | 2026 Form W-2 (irs.gov) |
| nist-sp800-63b.pdf | 1,480,377 | `ccfce7510a126793` | NIST SP 800-63B, Digital Identity Guidelines (doi.org/10.6028/NIST.SP.800-63b) |
| nist-ssdf.pdf | 739,891 | `617746e553a9e2da` | NIST SP 800-218, SSDF v1.1 (csrc.nist.gov) |
| pypdf-cmyk.pdf | 443,953 | `5a5f76a951e403a5` | pypdf project CMYK test document (github.com/py-pdf/pypdf test assets) |
| wiki-cmyk-topic.pdf | 544,864 | `a1523c924dd35f86` | "Offset printing", Wikipedia print-to-PDF render (CC BY-SA 4.0; article authors credited in the document) |
| wiki-pdf.pdf | 2,196,261 | `1e0a8117e12ca91f` | "Ada Lovelace - Wikipedia" print-to-PDF render (CC BY-SA 4.0) |
| xfa_filled_imm1344e.pdf | 3,023,968 | `8313c52ea97b4990` | IRCC IMM 1344 sponsorship application, filled dynamic XFA form |
| xfa_issue14315.pdf | 11,568 | `d039a40ea28384ba` | minimal dynamic XFA form from pikepdf issue #14315 discussion |

The NASA TM-20210010291 report used in the README headline is not part of
this corpus; it is separately downloadable from
[ntrs.nasa.gov](https://ntrs.nasa.gov/citations/20210010291).

## License situation, per file

- **US Government works** (irs-1040gi, irs-w2, census-brief, nist-sp800-63b,
  nist-ssdf): public domain as US Government works.
- **arXiv papers**: downloaded under the author-granted license that permits
  redistribution of the arXiv-hosted copy for research purposes; used here as
  benchmark *inputs*, not redistributed in this repository. If you fetch and
  re-host them, check each paper's license.
- **Wikipedia renders**: CC BY-SA 4.0 — the attribution is embedded in the
  documents themselves (Wikipedia's print footer).
- **cmyk-jpeg.pdf / pypdf-cmyk.pdf**: sample files from the pypdf/pdf.js
  ecosystems' public test discussions; included as inputs only.
- **xfa_filled_imm1344e.pdf**: a filled example of a public government form
  (IRCC), redact personal data before re-sharing yours.

## Fetch script

`scripts/fetch-corpus.sh` downloads each file and verifies its SHA-256
against the table above. The two XFA samples require manual download (form
portals, no stable direct URLs) — the script prints instructions for those.

## Reproducing the benchmark

```bash
scripts/fetch-corpus.sh      # downloads + verifies all inputs
scripts/bench-full.sh        # the four-lane matrix (lossless/lossy/kitchen/gs/gs-custom)
scripts/bench-vs-gs.sh       # fixture vs Ghostscript (works without the corpus)
```

Ghostscript 10.07.1 is the `gs` reference version in the comparisons; any
recent `gs` will differ by a few percent.