#!/usr/bin/env python3
"""hunt5 driver: regenerate the corpus outputs and print the size delta against
the round-4 baseline in target/scratch/h4.

Usage: python3 scripts/h5_run.py [--suffix def] [-- picamatl flags...]
"""
import os
import subprocess
import sys

FILES = ["adobe-spec", "arxiv-attention", "irs-1040gi", "nist-ssdf"]
BIN = "./target/release/picamatl"
OUT = "target/scratch/h5"

argv = sys.argv[1:]
suffix = "def"
if argv and argv[0] == "--suffix":
    suffix = argv[1]
    argv = argv[2:]
if argv and argv[0] == "--":
    argv = argv[1:]

os.makedirs(OUT, exist_ok=True)
total_old = total_new = 0
for f in FILES:
    dst = f"{OUT}/{f}.{suffix}.pdf"
    subprocess.run([BIN, *argv, f"corpus/{f}.pdf", "-o", dst], check=True, capture_output=True)
    base = f"target/scratch/h4/{f}.def.pdf"
    old = os.path.getsize(base) if os.path.exists(base) else 0
    new = os.path.getsize(dst)
    total_old += old
    total_new += new
    print(f"{f:<20} h4 {old:>9}  now {new:>9}  delta {new - old:>9}")
print(f"{'TOTAL':<20} h4 {total_old:>9}  now {total_new:>9}  delta {total_new - total_old:>9}")
