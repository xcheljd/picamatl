#!/usr/bin/env python3
"""hunt5 gates for one file: render identity against a reference output,
pdftotext equality, second-pass idempotence, and (optionally) a Ghostscript
nullpage interpretation.

Usage: python3 scripts/h5_gates.py <ref.pdf> <new.pdf> [--gs] [--dpi 72]
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


def render(path, out_prefix):
    subprocess.run(
        ["pdftoppm", "-r", dpi, "-png", path, out_prefix],
        check=True,
        capture_output=True,
    )
    d = os.path.dirname(out_prefix)
    base = os.path.basename(out_prefix)
    return [
        hashlib.sha1(open(os.path.join(d, f), "rb").read()).hexdigest()
        for f in sorted(os.listdir(d))
        if f.startswith(base)
    ]


def text(path):
    return subprocess.run(
        ["pdftotext", path, "-"], check=True, capture_output=True
    ).stdout


with tempfile.TemporaryDirectory() as td:
    a = render(ref, os.path.join(td, "a"))
    b = render(new, os.path.join(td, "b"))
    print(f"render: {len(a)} vs {len(b)} pages, identical = {a == b}")
    if a != b:
        for i, (x, y) in enumerate(zip(a, b)):
            if x != y:
                print(f"  first mismatch on page {i + 1}")
                break
    print(f"pdftotext identical = {text(ref) == text(new)}")

    # Second pass: running amatl again must not change the bytes.
    p2 = os.path.join(td, "pass2.pdf")
    subprocess.run(
        ["./target/release/amatl", *sys.argv[3:][:0], new, "-o", p2],
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
