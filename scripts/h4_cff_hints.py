#!/usr/bin/env python3
"""hunt4 item 12: what would an opt-in *CFF* hint strip (the Type1C analogue of
--strip-hinting) recover, on top of what already ships?

For every /FontFile3 /Type1C program in a picamatl output: re-encode each
charstring from its outline with no hint operators (h/vstem, hintmask,
cntrmask, hstemhm, vstemhm), drop the Private DICT hint keys (BlueValues,
StdHW, StemSnap*, ...) and all subrs, then measure the zlib-9 stream size
against the current stored stream.
"""
import io
import re
import sys
import zlib

import pikepdf
from fontTools.cffLib import CFFFontSet
from fontTools.pens.recordingPen import RecordingPen
from fontTools.pens.t2CharStringPen import T2CharStringPen

HINT_KEYS = (
    "BlueValues BlueScale BlueShift BlueFuzz OtherBlues FamilyBlues "
    "FamilyOtherBlues StdHW StdVW StemSnapH StemSnapV ForceBold "
    "LanguageGroup ExpansionFactor"
).split()
TAG = re.compile(r"^/[A-Z]{6}\+")


class _Fake:
    recalcBBoxes = False
    isTTF = False


def strip(data, drop_hints=True):
    cff = CFFFontSet()
    cff.decompile(io.BytesIO(data), None)
    td = cff[cff.fontNames[0]]
    cs = td.CharStrings
    traces = {}
    for n in cs.keys():
        pen = RecordingPen()
        cs[n].draw(pen)
        traces[n] = (cs[n].width, pen.value)
    priv = td.Private
    if hasattr(priv, "Subrs"):
        del priv.Subrs
    priv.rawDict.pop("Subrs", None)
    if drop_hints:
        for k in HINT_KEYS:
            if hasattr(priv, k):
                delattr(priv, k)
            priv.rawDict.pop(k, None)
    for n, (w, ops) in traces.items():
        pen = T2CharStringPen(w, None)
        for op, args in ops:
            getattr(pen, op)(*[tuple(p) for p in args])
        c = pen.getCharString(private=priv, globalSubrs=[])
        c.private = priv
        cs[n] = c
    buf = io.BytesIO()
    cff.compile(buf, _Fake())
    return buf.getvalue()


for path in sys.argv[1:]:
    pdf = pikepdf.open(path)
    seen = set()
    cur = new = keep = 0
    n_ok = n_fail = 0
    for o in pdf.objects:
        if not isinstance(o, pikepdf.Dictionary) or str(o.get("/Type")) != "/FontDescriptor":
            continue
        ff = o.get("/FontFile3")
        if ff is None or str(ff.get("/Subtype", "")) != "/Type1C" or ff.objgen in seen:
            continue
        seen.add(ff.objgen)
        raw = len(ff.read_raw_bytes())
        d = bytes(ff.read_bytes())
        try:
            s = len(zlib.compress(strip(d, True), 9))
            k = len(zlib.compress(strip(d, False), 9))
        except Exception:  # noqa: BLE001
            n_fail += 1
            continue
        n_ok += 1
        cur += raw
        new += s
        keep += k
    print(
        f"{path.split('/')[-1]:26s} Type1C progs {n_ok} (fail {n_fail}) "
        f"cur {cur} | reencode-keep-Private {keep} (save {cur - keep}) "
        f"| +drop-Private-hints {new} (save {cur - new})"
    )
