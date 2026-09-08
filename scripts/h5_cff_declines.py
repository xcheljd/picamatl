#!/usr/bin/env python3
"""hunt5: explain why a Type1C program declines the hint strip — report the
subroutine counts and whether any reachable subroutine contains a hint
operator (the precondition src/cffhint.rs enforces).

Usage: python3 scripts/h5_cff_declines.py <file.cff> ...
"""
import io
import sys

from fontTools.cffLib import CFFFontSet

HINTS = {"hstem", "vstem", "hstemhm", "vstemhm", "hintmask", "cntrmask"}

for path in sys.argv[1:]:
    data = open(path, "rb").read()
    cff = CFFFontSet()
    cff.decompile(io.BytesIO(data), None)
    td = cff[cff.fontNames[0]]
    subrs = list(getattr(td.Private, "Subrs", []))
    gsubrs = list(cff.GlobalSubrs)
    hinted = 0
    for s in subrs + gsubrs:
        s.decompile()
        if any(isinstance(t, str) and t in HINTS for t in s.program):
            hinted += 1
    name = path.split("/")[-1]
    print(
        f"{name:<32} glyphs {len(td.CharStrings):>4} lsubrs {len(subrs):>4} "
        f"gsubrs {len(gsubrs):>4} subrs-with-hints {hinted:>4} bytes {len(data)}"
    )
