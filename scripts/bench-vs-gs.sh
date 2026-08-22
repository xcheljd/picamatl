#!/usr/bin/env bash
# bench-vs-gs.sh — compare amatl's output size against Ghostscript pdfwrite on
# the same input PDF.
#
# Usage:
#   scripts/bench-vs-gs.sh [input.pdf]
#
# Defaults to the committed fixture fixtures/sample.pdf. Requires
# `gs` (Ghostscript) and a working Rust toolchain.
#
# How it drives amatl: the library's real-file test harness. Setting
# AMATL_TEST_PDF makes `real_file_shrinks_when_present` optimize that file with
# a size-focused configuration (strip_accessibility=true, packing off unless
# AMATL_TEST_PACK=1) and AMATL_TEST_OUT saves the optimized bytes.
#
# What "equivalent settings" means: amatl's defaults downsample images whose
# effective on-page DPI exceeds 130 x 1.15 down to 130 DPI and re-encode as
# JPEG at libjpeg quality 78. The gs invocation mirrors that: DCTEncode with
# 130 DPI image resolution, a 1.15 downsample threshold, and QFactor 0.4
# (Ghostscript expresses JPEG quality as a DCT QFactor; ~0.4 is the closest
# documented analogue of libjpeg q~78 — this is an approximation, not an exact
# match). gs also rewrites fonts/structure, so totals are not apples-to-apples
# beyond the image payload; the point is a size sanity check.
#
# The fixture is Flate-heavy as of 0.2.0: pages 3-4 embed FlateDecode images
# (PNG Up-predictor rows), so this same run also benchmarks the Flate
# downsampling path — no separate input needed. gs's mirrored settings already
# downsample all raster classes, keeping the comparison apples-to-apples.
#
# Measured baseline (2026-08-21, 4-page fixtures/sample.pdf with Flate pages):
#   input:  662107 bytes
#   amatl:  123948 bytes (18% of input)
#   gs:      71742 bytes (11% of input; gs 10.07.1 — its forced DCTEncode
#            converts the Flate noise pages to JPEG, which amatl refuses by
#            design, so the gap overstates typical documents)
#
# Previous 2-page JPEG-only fixture (gs 10.07.1): 193668 -> amatl 27087 (13%),
# gs 43722 (22%).
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
in="${1:-$repo_root/fixtures/sample.pdf}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

amatl_out="$tmp/amatl.pdf"
gs_out="$tmp/gs.pdf"

echo "== amatl (via real-file test harness) =="
AMATL_TEST_PDF="$in" AMATL_TEST_OUT="$amatl_out" \
    cargo test --manifest-path "$repo_root/Cargo.toml" \
    --lib real_file_shrinks_when_present -- --nocapture

echo "== ghostscript pdfwrite =="
gs -sDEVICE=pdfwrite -dCompatibilityLevel=1.5 -dNOPAUSE -dBATCH -dQUIET \
    -sOutputFile="$gs_out" \
    -dDownsampleColorImages=true -dColorImageResolution=130 \
    -dColorImageDownsampleType=/Bicubic -dColorImageDownsampleThreshold=1.15 \
    -dDownsampleGrayImages=true -dGrayImageResolution=130 \
    -dGrayImageDownsampleType=/Bicubic -dGrayImageDownsampleThreshold=1.15 \
    -dAutoFilterColorImages=false -dAutoFilterGrayImages=false \
    -dColorImageFilter=/DCTEncode -dGrayImageFilter=/DCTEncode \
    -c '<< /ColorImageDict << /QFactor 0.4 /Blend 1 /HSamples [1 1 1 1] /VSamples [1 1 1 1] >> /GrayImageDict << /QFactor 0.4 /Blend 1 /HSamples [1 1 1 1] /VSamples [1 1 1 1] >> >> setdistillerparams' \
    -f "$in"

in_size=$(stat -c %s "$in")
amatl_size=$(stat -c %s "$amatl_out")
gs_size=$(stat -c %s "$gs_out")

echo
echo "== results =="
printf 'input:  %8d bytes  %s\n' "$in_size" "$in"
printf 'amatl:  %8d bytes  (%d%% of input)\n' "$amatl_size" $((amatl_size * 100 / in_size))
printf 'gs:     %8d bytes  (%d%% of input)\n' "$gs_size" $((gs_size * 100 / in_size))
