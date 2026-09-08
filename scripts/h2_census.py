#!/usr/bin/env python3
"""hunt2 census: where are the bytes in a PDF? Group streams by role.

Usage: python3 scripts/h2_census.py file.pdf [...]
"""
import collections, sys
import pikepdf


def role(obj):
    d = obj
    st = d.get("/Subtype")
    t = d.get("/Type")
    if st is not None:
        s = str(st)
        if s == "/Image":
            f = d.get("/Filter")
            return f"image {f}"
        if s in ("/Type1C", "/CIDFontType0C", "/OpenType"):
            return f"font {s}"
        if s == "/XML":
            return "metadata"
        if s == "/Form":
            return "form xobject"
        if s == "/ObjStm":
            return "objstm"
        if s == "/XRef":
            return "xref"
        return f"other {s}"
    if t is not None:
        return f"type {t}"
    if "/Length1" in d:
        return "font truetype"
    return "content/other"


def main():
    for path in sys.argv[1:]:
        pdf = pikepdf.open(path)
        agg = collections.Counter()
        cnt = collections.Counter()
        total = 0
        for obj in pdf.objects:
            if not isinstance(obj, pikepdf.Stream):
                continue
            raw = len(obj.read_raw_bytes())
            r = role(obj)
            agg[r] += raw
            cnt[r] += 1
            total += raw
        import os
        fs = os.path.getsize(path)
        print(f"== {path} ({fs} bytes; streams {total} = {total*100/fs:.1f}%)")
        for r, b in agg.most_common(12):
            print(f"   {r:28s} {cnt[r]:5d} objs {b:10d} ({b*100/fs:.1f}%)")


if __name__ == "__main__":
    main()
