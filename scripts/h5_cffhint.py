#!/usr/bin/env python3
"""hunt5: isolate the Type1C hint-strip contribution by running amatl twice
with the same flags, once with AMATL_NO_CFFHINT=1 (a temporary escape hatch
kept only while measuring).

Usage: python3 scripts/h5_cffhint.py [amatl flags...]
"""
import os
import subprocess
import sys

FILES = ["adobe-spec", "arxiv-attention", "irs-1040gi", "nist-ssdf"]
flags = sys.argv[1:] or ["--strip-hinting"]
tag = "".join(f.lstrip("-")[:3] for f in flags)
total = [0, 0]
for f in FILES:
    sizes = []
    for name, off in (("nocff", True), ("cff", False)):
        env = dict(os.environ)
        env.pop("AMATL_NO_CFFHINT", None)
        if off:
            env["AMATL_NO_CFFHINT"] = "1"
        dst = f"target/scratch/h5/{f}.{tag}.{name}.pdf"
        subprocess.run(
            ["./target/release/amatl", *flags, f"corpus/{f}.pdf", "-o", dst],
            check=True,
            capture_output=True,
            env=env,
        )
        sizes.append(os.path.getsize(dst))
    total[0] += sizes[0]
    total[1] += sizes[1]
    print(
        f"{f:<20} no-cff {sizes[0]:>9}  with-cff {sizes[1]:>9}  "
        f"CFF hint strip {sizes[1] - sizes[0]:>8}"
    )
print(
    f"{'TOTAL':<20} no-cff {total[0]:>9}  with-cff {total[1]:>9}  "
    f"CFF hint strip {total[1] - total[0]:>8}"
)
