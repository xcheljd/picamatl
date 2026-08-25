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
#   AMATL_NO_FLATTEN=1 scripts/bench-full.sh   # kitchen sink minus --flatten-forms
#
# Requires: gs, python3 (for the matrix printer).
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
bin="${AMATL_BIN:-$repo_root/target/release/amatl}"
out="$repo_root/target/scratch/matrix"
mkdir -p "$out/lossless" "$out/lossy" "$out/kitchen" "$out/gs"

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

# The kitchen sink is "every opt-in flag", which now includes form flattening.
# `AMATL_NO_FLATTEN=1` reproduces the pre-0.3.2 kitchen sink for a with/without
# comparison on form-heavy inputs.
flatten_flag="--flatten-forms"
[ -n "${AMATL_NO_FLATTEN:-}" ] && flatten_flag="--no-flatten-forms"

echo "== amatl lossless (defaults) =="
for f in "${files[@]}"; do
  name=$(basename "$f" .pdf)
  "$bin" -o "$out/lossless/$name.pdf" "$f" >/dev/null 2>&1
  echo "  $name: $?"
done

echo "== amatl lossy (no form flattening) =="
for f in "${files[@]}"; do
  name=$(basename "$f" .pdf)
  "$bin" --allow-lossy --strip-metadata --strip-private-data --convert-type1 \
    --strip-hinting --recompress-bitonal-images --collapse-gray-images \
    --deflate-backend zopfli -o "$out/lossy/$name.pdf" "$f" >/dev/null 2>&1
  echo "  $name: $?"
done

echo "== amatl kitchen sink ($flatten_flag) =="
for f in "${files[@]}"; do
  name=$(basename "$f" .pdf)
  "$bin" --allow-lossy --strip-accessibility --strip-metadata --strip-private-data \
    --convert-type1 --strip-hinting --recompress-bitonal-images --collapse-gray-images \
    "$flatten_flag" \
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

def kb(n):
    return f"{n/1024:.0f} KB" if n < 1024*1024 else f"{n/1024/1024:.1f} MB"

def fmt(label, path, i):
    try:
        s = os.path.getsize(path)
        pct = 100*s/i
        note = " (grew)" if s > i else ""
        return f"  {label}: {kb(s)} ({pct:.1f}% of original){note}"
    except FileNotFoundError:
        return f"  {label}: FAILED (no output)"

totals = {}
wins = {"amatl": 0, "gs": 0}
for f in files:
    i = os.path.getsize(f)
    name = os.path.splitext(os.path.basename(f))[0]
    print(f"{name}: {kb(i)} in")
    for label, sub in [("lossless", "lossless"), ("lossy", "lossy"), ("kitchen", "kitchen")]:
        line = fmt(label, os.path.join(out, sub, name + ".pdf"), i)
        print(line)
        p = os.path.join(out, sub, name + ".pdf")
        if os.path.exists(p):
            totals[sub] = totals.get(sub, 0) + os.path.getsize(p)
    gpath = os.path.join(out, "gs", name + ".pdf")
    gline = fmt("gs", gpath, i)
    print(gline)
    if os.path.exists(gpath):
        g = os.path.getsize(gpath)
        totals["gs"] = totals.get("gs", 0) + g
    kpath = os.path.join(out, "kitchen", name + ".pdf")
    if os.path.exists(kpath) and os.path.exists(gpath):
        w = "AMATL" if os.path.getsize(kpath) < g else "gs"
        wins[w.lower()] += 1
        print(f"  winner: {w}")
    print()

ti = sum(os.path.getsize(f) for f in files)
print("TOTALS:")
for sub in ("lossless", "lossy", "kitchen", "gs"):
    if sub in totals:
        print(f"  {sub}: {kb(totals[sub])} ({100*totals[sub]/ti:.1f}%)")
print(f"  head-to-head (kitchen vs gs): AMATL {wins['amatl']}, gs {wins['gs']}")
PYEOF
