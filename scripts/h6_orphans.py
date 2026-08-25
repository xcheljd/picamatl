#!/usr/bin/env python3
"""h6_orphans.py — bytes held by objects unreachable from the trailer.

Walks every object reachable from /Root, /Info and /Encrypt, then reports the
stored byte cost of what is left over (objects a viewer can never see).
"""
import sys, os, pikepdf

def orphan_bytes(path):
    pdf = pikepdf.open(path)
    seen = set()
    stack = []
    for key in ("/Root", "/Info", "/Encrypt"):
        if key in pdf.trailer:
            stack.append(pdf.trailer[key])
    while stack:
        o = stack.pop()
        try:
            og = o.objgen
        except Exception:
            og = (0, 0)
        if og != (0, 0):
            if og in seen:
                continue
            seen.add(og)
        if isinstance(o, pikepdf.Array):
            stack.extend(list(o))
        elif isinstance(o, (pikepdf.Dictionary, pikepdf.Stream)):
            stack.extend(list(o.values()))
    total = orph = n = 0
    for o in pdf.objects:
        if not isinstance(o, pikepdf.Stream):
            continue
        try:
            b = len(o.read_raw_bytes())
        except Exception:
            b = 0
        total += b
        if o.objgen not in seen:
            orph += b
            n += 1
    return os.path.getsize(path), total, orph, n

if __name__ == "__main__":
    go = gs = 0
    for p in sys.argv[1:]:
        try:
            f, t, o, n = orphan_bytes(p)
        except Exception as e:
            print(f"{os.path.basename(p)}: FAILED {e}")
            continue
        gs += f; go += o
        if o:
            print(f"{os.path.basename(p):<24} file={f:>10,}  orphan streams={o:>9,} ({100*o/f:4.1f}%) n={n}")
    print(f"{'TOTAL':<24} files={gs:>10,}  orphan streams={go:>9,} ({100*go/gs:4.1f}%)")
