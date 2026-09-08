#!/usr/bin/env python3
"""hunt4: inspect one Type1C family group in detail — subr tables, Private DICTs,
and the actual outline delta for conflicting shared glyphs."""
import collections
import io
import re
import sys

import pikepdf
from fontTools.cffLib import CFFFontSet
from fontTools.pens.recordingPen import RecordingPen

TAG = re.compile(r"^/[A-Z]{6}\+")
path, want = sys.argv[1], sys.argv[2]

pdf = pikepdf.open(path)
groups = collections.defaultdict(list)
seen = set()
for o in pdf.objects:
    if not isinstance(o, pikepdf.Dictionary) or str(o.get("/Type")) != "/FontDescriptor":
        continue
    ff = o.get("/FontFile3")
    if ff is None or str(ff.get("/Subtype", "")) != "/Type1C" or ff.objgen in seen:
        continue
    seen.add(ff.objgen)
    groups[TAG.sub("/", str(o.get("/FontName", "/?")))].append(bytes(ff.read_bytes()))

members = groups["/" + want]
tds = []
for d in members:
    c = CFFFontSet()
    c.decompile(io.BytesIO(d), None)
    tds.append(c)

for i, c in enumerate(tds):
    td = c[c.fontNames[0]]
    p = td.Private
    print(
        f"frag{i}: glyphs {len(td.CharStrings)} localSubrs "
        f"{len(getattr(p, 'Subrs', []) or [])} globalSubrs {len(c.GlobalSubrs)} "
        f"nominalWidthX {getattr(p, 'nominalWidthX', None)} "
        f"defaultWidthX {getattr(p, 'defaultWidthX', None)} "
        f"FontMatrix {td.FontMatrix}"
    )
    subrs = getattr(p, "Subrs", []) or []
    print(f"        subr bytes {[len(s.bytecode or b'') for s in subrs][:8]}")

base = tds[0][tds[0].fontNames[0]].CharStrings
shown = 0
for i in range(1, len(tds)):
    cs = tds[i][tds[i].fontNames[0]].CharStrings
    for n in base.keys():
        if n not in cs:
            continue
        b0, b1 = bytes(base[n].bytecode or b""), bytes(cs[n].bytecode or b"")
        p0, p1 = RecordingPen(), RecordingPen()
        base[n].draw(p0)
        cs[n].draw(p1)
        if p0.value == p1.value and base[n].width == cs[n].width:
            continue
        if shown >= 3:
            break
        shown += 1
        print(f"\nCONFLICT frag0 vs frag{i} glyph {n}: bytes_equal={b0 == b1} "
              f"w {base[n].width} vs {cs[n].width} ops {len(p0.value)} vs {len(p1.value)}")
        for a, b in list(zip(p0.value, p1.value))[:6]:
            mark = "  " if a == b else "* "
            print(f"  {mark}{a}\n  {mark}{b}")
