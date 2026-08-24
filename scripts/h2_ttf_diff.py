#!/usr/bin/env python3
"""hunt2: group embedded TrueType programs by (decoded size) and diff near-identical ones."""
import collections, struct, sys
import pikepdf


def tabs(d):
    n = struct.unpack(">H", d[4:6])[0]
    r = {}
    for i in range(n):
        o = 12 + 16 * i
        tag = d[o:o + 4].decode("latin1")
        off, ln = struct.unpack(">II", d[o + 8:o + 16])
        r[tag] = (off, ln)
    return r


pdf = pikepdf.open(sys.argv[1])
by = collections.defaultdict(list)
for obj in pdf.objects:
    if isinstance(obj, pikepdf.Stream) and "/Length1" in obj:
        d = obj.read_bytes()
        if d[:4] not in (b"\x00\x01\x00\x00", b"true", b"ttcf"):
            continue
        by[len(d)].append(d)

for size, group in sorted(by.items()):
    if len(group) < 2:
        continue
    a = group[0]
    print(f"== size {size}, count {len(group)}")
    for b in group[1:]:
        if a == b:
            print("   byte-identical")
            continue
        ta = tabs(a)
        for tag, (off, ln) in sorted(ta.items()):
            if a[off:off + ln] != b[off:off + ln]:
                da = a[off:off + ln]
                db = b[off:off + ln]
                ndiff = sum(1 for x, y in zip(da, db) if x != y)
                print(f"   differs in {tag}: {ndiff}/{ln} bytes  {da[:48]!r} vs {db[:48]!r}")
