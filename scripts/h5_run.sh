#!/bin/sh
# hunt5: regenerate the corpus outputs under target/scratch/h5 and print the
# size delta against the round-4 baseline in target/scratch/h4.
# Usage: scripts/h5_run.sh [picamatl flags...]
set -e
BIN=./target/release/picamatl
OUT=target/scratch/h5
SUF="${SUF:-def}"
mkdir -p "$OUT"
for f in adobe-spec arxiv-attention irs-1040gi nist-ssdf; do
  "$BIN" "$@" "corpus/$f.pdf" "$OUT/$f.$SUF.pdf" >/dev/null
  old=$(stat -c%s "target/scratch/h4/$f.def.pdf" 2>/dev/null || echo 0)
  new=$(stat -c%s "$OUT/$f.$SUF.pdf")
  printf '%-20s h4 %9s  now %9s  delta %9s\n' "$f" "$old" "$new" "$((new - old))"
done
