#!/usr/bin/env python3
"""Render two PDFs through Ghostscript and compare every page pixel for pixel.

Used to prove `--flatten-forms` changes no ink: the original is rendered WITH
annotations (what a viewer shows), the flattened output has no annotations left
to render, so any difference is a flattening defect.

    scripts/forms-verify.py original.pdf flattened.pdf [dpi]
"""
import subprocess, sys, tempfile, os, glob

def render(pdf, out_dir, dpi):
    subprocess.run(
        ["gs", "-q", "-dNOPAUSE", "-dBATCH", "-sDEVICE=pnggray", f"-r{dpi}",
         "-dTextAlphaBits=1", "-dGraphicsAlphaBits=1",
         "-sOutputFile=" + os.path.join(out_dir, "p%03d.png"), pdf],
        check=True)
    return sorted(glob.glob(os.path.join(out_dir, "*.png")))

def main():
    a, b = sys.argv[1], sys.argv[2]
    dpi = int(sys.argv[3]) if len(sys.argv) > 3 else 100
    from PIL import Image
    with tempfile.TemporaryDirectory() as ta, tempfile.TemporaryDirectory() as tb:
        pa, pb = render(a, ta, dpi), render(b, tb, dpi)
        if len(pa) != len(pb):
            print(f"FAIL page count {len(pa)} != {len(pb)}"); return 1
        worst = 0.0
        bad = 0
        for i, (fa, fb) in enumerate(zip(pa, pb)):
            ia, ib = Image.open(fa).convert("L"), Image.open(fb).convert("L")
            if ia.size != ib.size:
                print(f"FAIL page {i+1} size {ia.size} != {ib.size}"); return 1
            da = ia.tobytes(); db = ib.tobytes()
            diff = sum(1 for x, y in zip(da, db) if abs(x - y) > 8)
            frac = diff / len(da)
            worst = max(worst, frac)
            if diff:
                bad += 1
                print(f"  page {i+1}: {diff} px differ > 8/255 ({100*frac:.4f}%)")
        print(f"{len(pa)} pages, {bad} with any difference, worst {100*worst:.4f}% of pixels")
        return 0 if worst == 0 else 2

sys.exit(main())
