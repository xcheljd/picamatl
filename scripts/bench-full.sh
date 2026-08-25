#!/usr/bin/env bash
# bench-full.sh — repeatable full-corpus benchmark for amatl vs Ghostscript.
#
# Runs every corpus PDF (corpus/ + corpus-expanded/) through:
#   amatl lossless (defaults), amatl kitchen sink, Ghostscript forced lossy
# and prints a comparison matrix. Outputs land in target/scratch/matrix/.
#
# Usage:
#   scripts/bench-full.sh          # full corpus
#   scripts/bench-full.sh adobe-spec arxiv-gpt4   # named files (no extension)
#   AMATL_BIN=./target/release/amatl scripts/bench-full.sh
#
# Requires: gs, python3 (for the matrix printer).
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
bin="${AMATL_BIN:-$repo_root/target/release/amatl}"
out="$repo_root/target/scratch/matrix"
mkdir -p "$out/kitchen" "$out/gs"

# Build the file list: either named args or everything in corpus/ + corpus-expanded/
if [ "$#" -gt 0 ]; then
  files=()
  for n in "$@"; do
    [ -f "$repo_root/corpus/$n.pdf" ] && files+=("corpus/$n.pdf") && continue
    [ -f "$repo_root/corpus-expanded/$n.pdf" ] && files+=("corpus-expanded/$n.pdf") && continue
    echo "WARN: $n.pdf not found" >&2
  done
else
  files=("$repo_root"/corpus/*.pdf "$repo_root"/corpus-expanded/*.pdf)
fi

echo "== amatl kitchen sink =="
for f in "${files[@]}"; do
  name=$(basename "$f" .pdf)
  "$bin" --allow-lossy --strip-accessibility --strip-metadata --convert-type1 \
    --strip-hinting --recompress-bitonal-images --collapse-gray-images \
    --deflate-backend zopfli -o "$out/kitchen/$name.pdf" "$f" >/dev/null 2>&1
  echo "  $name: $?"
done

echo "== ghostscript forced lossy =="
for f in "${files[@]}"; do
  name=$(basename "$f" .pdf)
  gs -sDEVICE=pdfwrite -dCompatibilityLevel=1.5 -dNOPAUSE -dBATCH -dQUIET \
    -sOutputFile="$out/gs/$name.pdf" \
    -dDownsampleColorImages=true -dColorImageResolution=130 \
    -dColorImageDownsampleType=/Bicubic -dColorImageDownsampleThreshold=1.15 \
    -dDownsampleGrayImages=true -dGrayImageResolution=130 \
    -dGrayImageDownsampleType=/Bicubic -dGrayImageDownsampleThreshold=1.15 \
    -dAutoFilterColorImages=false -dAutoFilterGrayImages=false \
    -dColorImageFilter=/DCTEncode -dGrayImageFilter=/DCTEncode \
    -c '<< /ColorImageDict << /QFactor 0.4 /Blend 1 /HSamples [1 1 1 1] /VSamples [1 1 1 1] >> /GrayImageDict << /QFactor 0.4 /Blend 1 /HSamples [1 1 1 1] /VSamples [1 1 1 1] >> >> setdistillerparams' \
    -f "$f"
  echo "  $name: $?"
done

echo
echo "== matrix =="
python3 - "$repo_root" "$out" "${files[@]}" <<'PYEOF'
import os, sys
repo, out = sys.argv[1], sys.argv[2]
files = sys.argv[3:]
rows = []
for f in files:
    i = os.path.getsize(f)
    name = os.path.splitext(os.path.basename(f))[0]
    k = os.path.getsize(os.path.join(out, "kitchen", name + ".pdf"))
    g = os.path.getsize(os.path.join(out, "gs", name + ".pdf"))
    rows.append((name, i, k, g))
hdr = f"{'file':<24}{'input':>12}{'kitchen':>12}  {'gs':>12}"
print(hdr)
ti = tk = tg = 0
for name, i, k, g in rows:
    ti += i; tk += k; tg += g
    w = "AMATL" if k < g else "gs"
    print(f"{name:<24}{i:>10,}{k:>10,} {100*k/i:>4.1f}%{g:>10,} {100*g/i:>4.1f}%  {w}")
print(f"{'TOTAL':<24}{ti:>10,}{tk:>10,} {100*tk/ti:>4.1f}%{tg:>10,} {100*tg/ti:>4.1f}%  {'AMATL' if tk<tg else 'gs'}")
PYEOF
