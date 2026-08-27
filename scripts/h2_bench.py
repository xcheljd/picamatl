#!/usr/bin/env python3
"""hunt2 bench: run picamatl over the corpus with given flags and print sizes.

Usage: python3 scripts/h2_bench.py TAG [flags...]
"""
import os, subprocess, sys

CORPUS = ["adobe-spec", "arxiv-attention", "irs-1040gi", "nist-ssdf", "dummy"]
OUT = "target/scratch/h2"


def main():
    tag = sys.argv[1] if len(sys.argv) > 1 else "def"
    flags = sys.argv[2:]
    os.makedirs(OUT, exist_ok=True)
    tot_a = tot_b = 0
    for f in CORPUS:
        src = f"corpus/{f}.pdf"
        dst = f"{OUT}/{f}-{tag}.pdf"
        subprocess.run(["./target/release/picamatl", *flags, "-o", dst, src],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        a = os.path.getsize(src)
        b = os.path.getsize(dst) if os.path.exists(dst) else 0
        tot_a += a
        tot_b += b
        print(f"{f:20s} {a:10d} -> {b:10d}  ({(b-a)*100/a:+.2f}%)")
    print(f"{'TOTAL':20s} {tot_a:10d} -> {tot_b:10d}  ({(tot_b-tot_a)*100/tot_a:+.2f}%)")


if __name__ == "__main__":
    main()
