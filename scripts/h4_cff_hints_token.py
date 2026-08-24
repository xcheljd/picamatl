#!/usr/bin/env python3
"""hunt4 item 12b: token-level CFF hint strip — the measurement that models what
a Rust implementation would actually do.

Decompile each Type2 charstring to its operator/operand program, drop the hint
operators (hstem/vstem/hstemhm/vstemhm/hintmask/cntrmask) and their operands,
re-fold the leading width operand, recompile. No outline re-encoding, no subr
removal: coordinates stay byte-for-byte the original relative deltas.
"""
import io
import sys
import zlib

import pikepdf
from fontTools.cffLib import CFFFontSet
from fontTools.misc.psCharStrings import T2CharString
from fontTools.pens.recordingPen import RecordingPen

HINTS = {"hstem", "vstem", "hstemhm", "vstemhm", "hintmask", "cntrmask"}


class _Fake:
    recalcBBoxes = False
    isTTF = False


def strip_program(prog, nominal_width, has_width):
    """Drop hint ops. Returns a new program, preserving the width prefix."""
    out = []
    stack = []
    width = None
    first = True
    i = 0
    while i < len(prog):
        tok = prog[i]
        if isinstance(tok, (int, float)):
            stack.append(tok)
            i += 1
            continue
        op = tok
        if op in ("hintmask", "cntrmask"):
            # implicit vstem: the stack holds stem pairs; the mask byte string
            # follows as the next program token.
            if first and len(stack) % 2 == 1:
                width = stack.pop(0)
            first = False
            stack = []
            i += 2  # skip the mask operand
            continue
        if op in HINTS:
            if first and len(stack) % 2 == 1:
                width = stack.pop(0)
            first = False
            stack = []
            i += 1
            continue
        # non-hint operator: it keeps its operands
        if first:
            first = False
            n = _nargs(op)
            if n is not None and len(stack) == n + 1:
                width = stack.pop(0)
        out.extend(stack)
        out.append(op)
        stack = []
        i += 1
    out.extend(stack)
    if width is not None:
        out.insert(0, width)
    return out


def _nargs(op):
    return {
        "rmoveto": 2,
        "hmoveto": 1,
        "vmoveto": 1,
        "endchar": 0,
    }.get(op)


def rewrite(data):
    cff = CFFFontSet()
    cff.decompile(io.BytesIO(data), None)
    td = cff[cff.fontNames[0]]
    cs = td.CharStrings
    priv = td.Private
    gsubrs = cff.GlobalSubrs
    subrs = getattr(priv, "Subrs", [])
    for n in cs.keys():
        c = cs[n]
        c.decompile()
        prog = strip_program(list(c.program), None, None)
        nc = T2CharString(program=prog, private=priv, globalSubrs=gsubrs)
        nc.private = priv
        cs[n] = nc
    # subrs may still be referenced by callsubr in the surviving program
    if subrs:
        for s in subrs:
            s.decompile()
            s.program = strip_program(list(s.program), None, None)
    buf = io.BytesIO()
    cff.compile(buf, _Fake())
    return buf.getvalue()


def _trace(data):
    cff = CFFFontSet()
    cff.decompile(io.BytesIO(data), None)
    cs = cff[cff.fontNames[0]].CharStrings
    out = {}
    for n in cs.keys():
        pen = RecordingPen()
        cs[n].draw(pen)
        out[n] = (cs[n].width, pen.value)
    return out


def verify(orig, stripped):
    """Count glyphs whose outline or width changed. Must be 0."""
    a, b = _trace(orig), _trace(stripped)
    if set(a) != set(b):
        return len(set(a) ^ set(b))
    return sum(1 for n in a if a[n] != b[n])


for path in sys.argv[1:]:
    pdf = pikepdf.open(path)
    seen = set()
    cur = new = n_bad = 0
    n_ok = n_fail = 0
    for o in pdf.objects:
        if not isinstance(o, pikepdf.Dictionary) or str(o.get("/Type")) != "/FontDescriptor":
            continue
        ff = o.get("/FontFile3")
        if ff is None or str(ff.get("/Subtype", "")) != "/Type1C" or ff.objgen in seen:
            continue
        seen.add(ff.objgen)
        raw = len(ff.read_raw_bytes())
        try:
            d = bytes(ff.read_bytes())
            stripped = rewrite(d)
            bad = verify(d, stripped)
            if bad:
                print(f"   OUTLINE MISMATCH {bad} -- font declined")
                n_bad += bad
                continue
            s = len(zlib.compress(stripped, 9))
        except Exception as e:  # noqa: BLE001
            n_fail += 1
            print("   fail", e)
            continue
        n_ok += 1
        cur += raw
        new += s
    print(
        f"{path.split('/')[-1]:26s} Type1C progs {n_ok} (fail {n_fail}) "
        f"cur {cur} token-hint-stripped z9 {new} SAVE {cur - new} "
        f"| outline-mismatched glyphs {n_bad}"
    )
