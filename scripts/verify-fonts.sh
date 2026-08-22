#!/usr/bin/env bash
# verify-fonts.sh — dev-only external verification of the font-subsetting
# path (Phase 3 C-M1). Never shipped, never a CI gate; same posture as
# bench-vs-gs.sh.
#
# What it does:
#   1. Runs the ignored `emit_font_verification_pdfs` test, which writes a
#      pre-subset and post-subset PDF pair to target/font-verify/.
#   2. Renders both through Ghostscript's nullpage device — a rendering pass
#      that exercises the subset font program end to end and fails on font
#      program errors.
#   3. When poppler's pdftotext is available, extracts text from both and
#      diffs it: the CIDToGIDMap-stream technique keeps content streams and
#      /ToUnicode untouched, so extraction must be byte-identical.
#
# Usage:
#   scripts/verify-fonts.sh
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
out_dir="$repo_root/target/font-verify"

echo "== emit pre/post PDFs =="
cargo test --manifest-path "$repo_root/Cargo.toml" \
    --lib emit_font_verification_pdfs -- --ignored --nocapture

pre="$out_dir/pre.pdf"
post="$out_dir/post.pdf"

echo "== ghostscript render check =="
for f in "$pre" "$post"; do
    gs -dNOPAUSE -dBATCH -dQUIET -sDEVICE=nullpage "$f"
    echo "gs render OK: $f"
done

if command -v pdftotext >/dev/null 2>&1; then
    echo "== pdftotext comparison =="
    pdftotext "$pre" "$out_dir/pre.txt"
    pdftotext "$post" "$out_dir/post.txt"
    diff "$out_dir/pre.txt" "$out_dir/post.txt"
    echo "pdftotext output identical"
else
    echo "pdftotext not found; text comparison skipped"
fi

printf 'pre:  %8d bytes\npost: %8d bytes\n' "$(stat -c %s "$pre")" "$(stat -c %s "$post")"
