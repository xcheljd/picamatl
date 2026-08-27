#!/usr/bin/env python3
"""hunt2 verify: render-identity (pdftoppm sha256), pass-2 idempotence, optional gs.

Usage: python3 scripts/h2_verify.py in.pdf out.pdf [pages] [--gs]
"""
import hashlib, os, subprocess, sys, tempfile


def render(pdf, first, last, d):
    subprocess.run(["pdftoppm", "-r", "72", "-f", str(first), "-l", str(last), "-png", pdf,
                    os.path.join(d, "p")], check=True)
    return [hashlib.sha256(open(os.path.join(d, f), "rb").read()).hexdigest()
            for f in sorted(os.listdir(d))]


def main():
    src, out = sys.argv[1], sys.argv[2]
    pages = sys.argv[3] if len(sys.argv) > 3 else "1-8"
    first, last = (int(x) for x in pages.split("-"))
    with tempfile.TemporaryDirectory() as a, tempfile.TemporaryDirectory() as b:
        ha, hb = render(src, first, last, a), render(out, first, last, b)
    print("render-identity:", "OK" if ha == hb else f"MISMATCH {ha} != {hb}")

    p2 = out + ".pass2"
    subprocess.run(["./target/release/picamatl", "-o", p2, out], stdout=subprocess.DEVNULL, check=True)
    same = open(p2, "rb").read() == open(out, "rb").read()
    print(f"idempotence: {'OK' if same else 'DIFFERS'} ({os.path.getsize(out)} -> {os.path.getsize(p2)})")

    if "--gs" in sys.argv:
        r = subprocess.run(["gs", "-o", "/dev/null", "-sDEVICE=nullpage", out],
                           capture_output=True, text=True)
        print("gs:", "OK" if r.returncode == 0 else f"FAIL {r.stderr[-500:]}")


if __name__ == "__main__":
    main()
