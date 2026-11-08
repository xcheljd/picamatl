#!/bin/sh
# Build the fuzz seed corpora from real PDFs: fonts, JPEGs, Flate streams and
# content streams are pulled out of each PDF, de-duplicated, and laid out as
# corpus/<target>/. Seeds are never committed (corpus/ is git-ignored).
#
#   ./seed.sh ../fixtures/sample.pdf ../fixtures/forms/*.pdf more/*.pdf
set -eu
[ $# -gt 0 ] || { echo "usage: $0 <pdf>..." >&2; exit 2; }
# Resolve the inputs against the caller's directory before moving into fuzz/.
pdfs=""
for f in "$@"; do pdfs="$pdfs $(realpath "$f")"; done
# shellcheck disable=SC2086 # word-splitting the resolved list is intended
set -- $pdfs
cd "$(dirname "$0")"

raw=$(mktemp -d)
trap 'rm -rf "$raw"' EXIT
cargo run --quiet --release --bin extract_seeds -- "$raw" "$@"

mkdir -p corpus/optimize corpus/optimize_all corpus/cff
for t in truetype cff_font jpeghuff content type1 deflate; do mkdir -p "corpus/$t"; done
for t in truetype cff_font jpeghuff content; do
    [ -d "$raw/$t" ] && cp "$raw/$t/"* "corpus/$t/"
done
# The CFF merge target takes whole CFF fonts too.
[ -d "$raw/cff_font" ] && cp "$raw/cff_font/"* corpus/cff/
# type1 and deflate read a leading option byte before the payload: seed both
# settings of its low bit.
for t in type1 deflate; do
    [ -d "$raw/$t" ] || continue
    for f in "$raw/$t/"*; do
        for flag in 0 1; do
            { printf "\\00$flag"; cat "$f"; } > "corpus/$t/$(basename "$f")-$flag"
        done
    done
done
cp ../fixtures/jpeg/*.jpg corpus/jpeghuff/ 2>/dev/null || true
for f in "$@"; do
    cp "$f" corpus/optimize/
    cp "$f" corpus/optimize_all/
done
for d in corpus/*; do echo "$(basename "$d"): $(ls "$d" | wc -l) seeds"; done
