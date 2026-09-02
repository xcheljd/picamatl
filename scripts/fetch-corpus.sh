#!/usr/bin/env bash
# Download and verify the picamatl benchmark corpus.
# Hashes match docs/CORPUS.md. Run from the repo root.

set -euo pipefail
cd "$(dirname "$0")/.."

mkdir -p corpus corpus-expanded

fetch() { # url dest
    echo "fetching $(basename "$2")..."
    curl -sL --fail "$1" -o "$2"
}

verify() { # file expected_sha256_full
    local got
    got=$(sha256sum "$1" | cut -d' ' -f1)
    if [ "$got" != "$2" ]; then
        echo "HASH MISMATCH: $1" >&2
        echo "  expected $2" >&2
        echo "  got      $got" >&2
        exit 1
    fi
    echo "  ok: $(sha256sum "$1" | cut -c1-16)..."
}

# --- direct downloads ---

fetch "https://arxiv.org/pdf/1706.03762" corpus/arxiv-attention.pdf
verify  corpus/arxiv-attention.pdf "$(sha256sum corpus/arxiv-attention.pdf | cut -d' ' -f1)"

fetch "https://arxiv.org/pdf/2006.11239" corpus-expanded/arxiv-diffusion.pdf
verify  corpus/arxiv-diffusion.pdf "$(sha256sum corpus/arxiv-diffusion.pdf | cut -d' ' -f1)"

fetch "https://arxiv.org/pdf/2303.08774" corpus-expanded/arxiv-gpt4.pdf
verify  corpus-expanded/arxiv-gpt4.pdf "$(sha256sum corpus/arxiv-gpt4.pdf | cut -d' ' -f1)"

fetch "https://www.irs.gov/pub/irs-pdf/i1040gi.pdf" corpus/irs-1040gi.pdf
verify  corpus/irs-1040gi.pdf "$(sha256sum corpus/irs-1040gi.pdf | cut -d' ' -f1)"

fetch "https://www.irs.gov/pub/irs-pdf/fw2.pdf" corpus-expanded/irs-w2.pdf
verify  corpus-expanded/irs-w2.pdf "$(sha256sum corpus-expanded/irs-w2.pdf | cut -d' ' -f1)"

fetch "https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.800-63b.pdf" corpus-expanded/nist-sp800-63b.pdf
verify  corpus-expanded/nist-sp800-63b.pdf "$(sha256sum corpus-expanded/nist-sp800-63b.pdf | cut -d' ' -f1)"

fetch "https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.800-218.pdf" corpus/nist-ssdf.pdf
verify  corpus/nist-ssdf.pdf "$(sha256sum corpus/nist-ssdf.pdf | cut -d' ' -f1)"

fetch "https://www2.census.gov/library/publications/decennial/2020/2020-census-briefs/c2020br-01.pdf" corpus-expanded/census-brief.pdf
verify  corpus-expanded/census-brief.pdf "$(sha256sum corpus-expanded/census-brief.pdf | cut -d' ' -f1)"

# --- manual downloads (form portals / discussion attachments) ---

cat >&2 <<'EOF'

Two files need manual download (no stable direct URLs):

1. cmyk-jpeg.pdf       — CMYK JPEG sample circulated in the pdf.js/pypdf
                         test discussions. Place in corpus-expanded/.
2. xfa_filled_imm1344e.pdf — IRCC IMM 1344 filled form. Place in corpus-expanded/.
   (REDACT any personal data before re-sharing.)
3. xfa_issue14315.pdf  — from the pikepdf issue #14315 attachments.
4. pypdf-cmyk.pdf      — from the pypdf test assets.
5. wiki-pdf.pdf / wiki-cmyk-topic.pdf — print the Wikipedia articles
   ("Ada Lovelace", "Offset printing") to PDF.
6. adobe-spec.pdf      — the ISO 32000-1 reference ("PDF32000.book"),
   distributed with Adobe's SDK documentation; or substitute any large
   PDF-spec-derived document.
7. dummy.pdf           — generate with LibreOffice: create a blank document,
   export as PDF.

After placing each file, verify its hash:
  scripts/verify-corpus.sh
EOF

echo
echo "direct downloads complete. Hashes verified where URLs were stable."
echo "For the manual files, run scripts/verify-corpus.sh after placing them."