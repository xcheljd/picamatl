#!/usr/bin/env python3
"""hunt4 item 10: how much would *semantic* (outline-equivalent) Type1C family
merging actually recover?

For each BaseFont family with >1 embedded /FontFile3 (Subtype /Type1C):
  1. decompile every charstring to an absolute outline (pen trace) + width,
     which is subr-factoring- and hint-independent;
  2. report shared-glyph agreement (outline-equal? width-equal? bytes-equal?);
  3. if outline-compatible, BUILD the union font with fontTools (charstrings
     re-encoded from the outlines, no local subrs) and measure the real zlib-9
     stream size of the merged program against the group's current streams.
"""
import collections
import io
import re
import sys
import zlib

import pikepdf
from fontTools.cffLib import CFFFontSet
from fontTools.pens.recordingPen import RecordingPen
from fontTools.pens.t2CharStringPen import T2CharStringPen

TAG = re.compile(r"^/[A-Z]{6}\+")


def famname(n):
    return TAG.sub("/", str(n))


def load_cff(data):
    cff = CFFFontSet()
    cff.decompile(io.BytesIO(data), None)
    return cff


def outlines(cff):
    """name -> (width, tuple-of-pen-ops) for every glyph."""
    td = cff[cff.fontNames[0]]
    cs = td.CharStrings
    out = {}
    for name in cs.keys():
        pen = RecordingPen()
        c = cs[name]
        c.draw(pen)
        w = c.width
        out[name] = (
            w,
            tuple((op, tuple(tuple(p) for p in args)) for op, args in pen.value),
        )
    return out


def rawcs(cff):
    """Raw charstring bytes. Must be read BEFORE any draw()/decompile, which
    clears .bytecode."""
    td = cff[cff.fontNames[0]]
    cs = td.CharStrings
    return {n: bytes(cs[n].bytecode or b"") for n in cs.keys()}


def contours(ops):
    """Split a pen trace into closed contours, each a list of segments
    (kind, endpoint, control-points-relative-to-nothing) starting after moveTo."""
    out, cur, start = [], None, None
    for op, args in ops:
        if op == "moveTo":
            if cur is not None:
                out.append((start, cur))
            start, cur = args[0], []
        elif op in ("lineTo", "curveTo", "qCurveTo"):
            cur.append((op, args))
        elif op == "closePath" or op == "endPath":
            if cur is not None:
                out.append((start, cur))
            cur, start = None, None
    if cur is not None:
        out.append((start, cur))
    return out


def _pts(start, segs):
    """Absolute point stream of a contour: on-curve start then every arg."""
    p = [start]
    for _, args in segs:
        p.extend(args)
    return p


def contour_close(c0, c1, tol):
    """Equal within tol, allowing any rotation of the contour's start point."""
    s0, g0 = c0
    s1, g1 = c1
    if len(g0) != len(g1):
        return False
    kinds0 = [k for k, _ in g0]
    n = len(g0)
    for r in range(n):
        rot = g1[r:] + g1[:r]
        if [k for k, _ in rot] != kinds0:
            continue
        # start point of the rotated contour is the endpoint of segment r-1
        news = g1[r - 1][1][-1] if r else s1
        a, b = _pts(s0, g0), _pts(news, rot)
        if len(a) != len(b):
            continue
        if all(
            len(x) == len(y) and all(abs(u - v) <= tol for u, v in zip(x, y))
            for x, y in zip(a, b)
        ):
            return True
    return False


def close(a, b, tol):
    """Outline+width equality within `tol` font units, rotation-insensitive."""
    if a[0] != b[0]:
        return False
    c0, c1 = contours(a[1]), contours(b[1])
    if len(c0) != len(c1):
        return False
    used = [False] * len(c1)
    for x in c0:
        for i, y in enumerate(c1):
            if not used[i] and contour_close(x, y, tol):
                used[i] = True
                break
        else:
            return False
    return True


def build_union(base_data, all_outlines):
    """Re-encode the union glyph set into base_data's CFF, dropping all subrs."""
    cff = load_cff(base_data)
    td = cff[cff.fontNames[0]]
    cs = td.CharStrings
    priv = td.Private
    if hasattr(priv, "Subrs"):
        del priv.Subrs
    priv.rawDict.pop("Subrs", None)
    order = list(cs.keys())
    for n in sorted(all_outlines):
        if n not in cs:
            order.append(n)
    for n in order:
        w, ops = all_outlines[n]
        pen = T2CharStringPen(w, None)
        for op, args in ops:
            getattr(pen, op)(*[tuple(p) for p in args])
        c = pen.getCharString(private=priv, globalSubrs=[])
        c.private = priv
        if n in cs.charStrings:
            cs[n] = c
        else:
            cs.charStringsIndex.append(c)
            cs.charStrings[n] = len(cs.charStringsIndex) - 1
    td.charset = order
    buf = io.BytesIO()

    class _Fake:
        recalcBBoxes = False
        isTTF = False

    cff.compile(buf, _Fake())
    return buf.getvalue()


def main(paths):
    grand = 0
    for path in paths:
        pdf = pikepdf.open(path)
        groups = collections.defaultdict(list)
        seen = set()
        for o in pdf.objects:
            if (
                not isinstance(o, pikepdf.Dictionary)
                or str(o.get("/Type")) != "/FontDescriptor"
            ):
                continue
            ff = o.get("/FontFile3")
            if ff is None or str(ff.get("/Subtype", "")) != "/Type1C":
                continue
            if ff.objgen in seen:
                continue
            seen.add(ff.objgen)
            groups[famname(o.get("/FontName", "/?"))].append(
                (ff.objgen, bytes(ff.read_bytes()), len(ff.read_raw_bytes()))
            )
        total = 0
        for fam, members in sorted(groups.items()):
            if len(members) < 2:
                continue
            cur_raw = sum(m[2] for m in members)
            try:
                cffs = [load_cff(m[1]) for m in members]
                raws = [rawcs(c) for c in cffs]
                outs = [outlines(c) for c in cffs]
            except Exception as e:  # noqa: BLE001
                print(f"  {path} {fam}: PARSE FAIL {e}")
                continue
            union = {}
            conflicts = widthconf = byteconf = maxdev = 0
            ok = True
            for o in outs:
                for n, v in o.items():
                    if n in union:
                        if union[n] != v:
                            if not close(union[n], v, TOL):
                                conflicts += 1
                                if union[n][0] != v[0]:
                                    widthconf += 1
                                ok = False
                            else:
                                maxdev = max(maxdev, 1)
                    else:
                        union[n] = v
            for i in range(1, len(raws)):
                for n in raws[0]:
                    if n in raws[i] and raws[0][n] != raws[i][n]:
                        byteconf += 1
            shared = sum(1 for n in union if sum(1 for o in outs if n in o) > 1)
            status = (
                f"OUTLINE-EQUAL(tol={TOL})"
                if ok
                else f"OUTLINE-CONFLICT({conflicts}, width-diff {widthconf})"
            )
            line = (
                f"  {path.split('/')[-1]:26s} {fam:24s} x{len(members)} "
                f"union {len(union)} shared {shared} bytes-diff {byteconf} -> {status}"
            )
            if not ok:
                print(line + f"  | cur raw {cur_raw}")
                continue
            merged = build_union(members[0][1], union)
            mz = len(zlib.compress(merged, 9))
            save = cur_raw - mz
            total += save
            print(line + f"  | cur raw {cur_raw}  merged z9 {mz}  SAVE {save}")
        print(f"{path}: semantic-merge measured save {total}")
        grand += total
    print(f"TOTAL {grand}")


TOL = float(sys.argv[1])
main(sys.argv[2:])
