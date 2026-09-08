#!/usr/bin/env python3
"""hunt5 config matrix: run one picamatl binary over the corpus in each flag
configuration and print a size table.

Usage:
  python3 scripts/h5_matrix.py <binary> <out-dir-suffix>

Run it once with the round-4 binary and once with the round-5 one, then diff
the two tables to attribute each row.
"""
import os
import subprocess
import sys

FILES = ["adobe-spec", "arxiv-attention", "irs-1040gi", "nist-ssdf"]
ROWS = [
    ("default", []),
    ("strip-hinting", ["--strip-hinting"]),
    ("convert-type1", ["--convert-type1"]),
    ("hinting+type1", ["--strip-hinting", "--convert-type1"]),
    (
        "all-lossless+opt-in",
        [
            "--strip-hinting",
            "--convert-type1",
            "--recompress-bitonal-images",
            "--collapse-gray-images",
        ],
    ),
]

binary = sys.argv[1]
suffix = sys.argv[2]
out = f"target/scratch/h5/matrix-{suffix}"
os.makedirs(out, exist_ok=True)

print(f"{'config':<22}" + "".join(f"{f:>18}" for f in FILES) + f"{'TOTAL':>12}")
for name, flags in ROWS:
    sizes = []
    for f in FILES:
        dst = f"{out}/{f}.{name}.pdf"
        subprocess.run(
            [binary, *flags, f"corpus/{f}.pdf", "-o", dst], check=True, capture_output=True
        )
        sizes.append(os.path.getsize(dst))
    print(f"{name:<22}" + "".join(f"{s:>18}" for s in sizes) + f"{sum(sizes):>12}")
