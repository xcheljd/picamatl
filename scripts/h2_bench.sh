#!/usr/bin/env bash
# hunt2 benchmark helper: run amatl over corpus with given flags, print sizes
set -u
out=${OUT:-target/scratch/h2}
tag=${TAG:-def}
mkdir -p "$out"
for f in adobe-spec arxiv-attention irs-1040gi nist-ssdf dummy; do
  ./target/release/amatl "$@" -o "$out/$f-$tag.pdf" "corpus/$f.pdf" >/dev/null 2>&1
  a=$(stat -c%s "corpus/$f.pdf"); b=$(stat -c%s "$out/$f-$tag.pdf" 2>/dev/null || echo 0)
  printf '%-20s %10d -> %10d  (%+.2f%%)\n' "$f" "$a" "$b" "$(echo "scale=4;($b-$a)*100/$a" | bc)"
done
