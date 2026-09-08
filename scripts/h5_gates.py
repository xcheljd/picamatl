#!/usr/bin/env python3
"""hunt5 gates for one file: render comparison against a reference output,
pdftotext equality, second-pass idempotence, and (optionally) a Ghostscript
nullpage interpretation.

Usage:
  python3 scripts/h5_gates.py <ref.pdf> <new.pdf> [--gs] [--dpi 72]
                              [--pass2-flags "--strip-hinting ..."]

The render check reports per-page hash equality *and*, when hashes differ,
the fraction of differing pixels and the maximum channel delta -- hint
stripping legitimately changes rasterization, so identity is the gate for the
lossless passes and magnitude is the report for the opt-in one.
"""
import hashlib
import os
import subprocess
import sys
import tempfile

ref, new = sys.argv[1], sys.argv[2]
use_gs = "--gs" in sys.argv
dpi = "72"
if "--dpi" in sys.argv:
    dpi = sys.argv[sys.argv.index("--dpi") + 1]
pass2_flags = []
if "--pass2-flags" in sys.argv:
    pass2_flags = sys.argv[sys.argv.index("--pass2-flags") + 1].split()


def render(path, out_prefix):
    subprocess.run(
        ["pdftoppm", "-r", dpi, "-png", path, out_prefix],
        check=True,
        capture_output=True,
    )
    d = os.path.dirname(out_prefix)
    base = os.path.basename(out_prefix)
    return [
        os.path.join(d, f) for f in sorted(os.listdir(d)) if f.startswith(base)
    ]


def digest(paths):
    return [hashlib.sha1(open(p, "rb").read()).hexdigest() for p in paths]


def text(path):
    return subprocess.run(
        ["pdftotext", path, "-"], check=True, capture_output=True
    ).stdout


with tempfile.TemporaryDirectory() as td:
    pa = render(ref, os.path.join(td, "a"))
    pb = render(new, os.path.join(td, "b"))
    a, b = digest(pa), digest(pb)
    print(f"render: {len(a)} vs {len(b)} pages, identical = {a == b}")
    if a != b:
        try:
            from PIL import Image, ImageChops

            worst_frac = worst_max = 0.0
            differing = 0
            for x, y, px, py in zip(a, b, pa, pb):
                if x == y:
                    continue
                differing += 1
                ia = Image.open(px).convert("L")
                ib = Image.open(py).convert("L")
                diff = ImageChops.difference(ia, ib)
                hist = diff.histogram()
                n = sum(hist)
                frac = 1.0 - hist[0] / n
                worst_frac = max(worst_frac, frac)
                worst_max = max(worst_max, max(i for i, c in enumerate(hist) if c))
            print(
                f"  {differing}/{len(a)} pages differ; worst page "
                f"{worst_frac * 100:.3f}% of pixels, max channel delta {worst_max:.0f}"
            )
        except ImportError:
            print("  (install Pillow for a magnitude report)")
    print(f"pdftotext identical = {text(ref) == text(new)}")

    # Second pass: running picamatl again must not change the bytes.
    p2 = os.path.join(td, "pass2.pdf")
    subprocess.run(
        ["./target/release/picamatl", *pass2_flags, new, "-o", p2],
        check=True,
        capture_output=True,
    )
    same = open(p2, "rb").read() == open(new, "rb").read()
    print(f"pass-2 idempotent = {same} ({os.path.getsize(p2)} vs {os.path.getsize(new)})")

    if use_gs:
        r = subprocess.run(
            [
                "gs",
                "-dBATCH",
                "-dNOPAUSE",
                "-dSAFER",
                "-sDEVICE=nullpage",
                new,
            ],
            capture_output=True,
        )
        errs = [
            l
            for l in r.stderr.decode("utf8", "replace").splitlines()
            if l.strip() and "GPL Ghostscript" not in l and "Copyright" not in l
        ]
        print(f"gs nullpage rc={r.returncode} noise={len(errs)}")
        for l in errs[:10]:
            print("   ", l)
