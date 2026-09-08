#!/usr/bin/env python3
"""hunt2: remaining headroom from merging several subsets of the SAME family
into one union subset. Groups embedded font programs by BaseFont minus its
subset tag; estimate = sum(raw) - max(raw) per group (union ≈ largest member).
"""
import collections, re, sys
import pikepdf

TAG = re.compile(r"^/[A-Z]{6}\+")


def base(name):
    s = str(name)
    return TAG.sub("/", s)


for path in sys.argv[1:]:
    pdf = pikepdf.open(path)
    groups = collections.defaultdict(list)
    seen = set()
    for o in pdf.objects:
        if not isinstance(o, pikepdf.Dictionary) or str(o.get("/Type")) != "/FontDescriptor":
            continue
        for k in ("/FontFile", "/FontFile2", "/FontFile3"):
            ff = o.get(k)
            if ff is None:
                continue
            key = ff.objgen
            n = base(o.get("/FontName", "/?"))
            if (n, key) in seen:
                continue
            seen.add((n, key))
            groups[n].append(len(ff.read_raw_bytes()))
    est = 0
    for n, sizes in sorted(groups.items(), key=lambda kv: -(sum(kv[1]) - max(kv[1]))):
        if len(sizes) > 1:
            g = sum(sizes) - max(sizes)
            est += g
            print(f"   {n:34s} x{len(sizes):<3d} raw {sum(sizes):8d}  merge est save {g}")
    print(f"{path}: family-merge headroom ~{est}")
