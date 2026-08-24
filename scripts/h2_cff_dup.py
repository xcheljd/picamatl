#!/usr/bin/env python3
"""hunt2: do embedded CFF (/Type1C) programs differ only in their Name INDEX
subset tag? Group by (bytes with the leading Name INDEX blanked)."""
import collections, sys
import pikepdf


def mask_name_index(d):
    """Blank the CFF Name INDEX contents (offSize-agnostic). None if unparsable."""
    if len(d) < 4:
        return None
    hdr = d[2]
    off = hdr
    if off + 3 > len(d):
        return None
    count = int.from_bytes(d[off:off + 2], "big")
    if count == 0:
        return d
    osz = d[off + 2]
    if osz not in (1, 2, 3, 4):
        return None
    base = off + 3
    offs = []
    for i in range(count + 1):
        p = base + i * osz
        offs.append(int.from_bytes(d[p:p + osz], "big"))
    data_start = base + (count + 1) * osz - 1
    end = data_start + offs[-1]
    if end > len(d):
        return None
    # keep structure/length, blank the string bytes
    return d[:data_start + offs[0]] + b"?" * (offs[-1] - offs[0]) + d[end:]


for path in sys.argv[1:]:
    pdf = pikepdf.open(path)
    groups = collections.defaultdict(list)
    exact = collections.defaultdict(int)
    for o in pdf.objects:
        if isinstance(o, pikepdf.Stream) and str(o.get("/Subtype")) in ("/Type1C", "/CIDFontType0C"):
            d = o.read_bytes()
            m = mask_name_index(d)
            groups[m if m else d].append(len(o.read_raw_bytes()))
            exact[d] += 1
    dup_raw = sum(sum(v[1:]) for v in groups.values() if len(v) > 1)
    exact_dup = sum(c - 1 for c in exact.values() if c > 1)
    print(f"{path}: {sum(len(v) for v in groups.values())} CFF progs, "
          f"masked-dup groups {sum(1 for v in groups.values() if len(v)>1)}, "
          f"redundant raw bytes {dup_raw}, exact dups {exact_dup}")
