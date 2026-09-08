#!/usr/bin/env python3
"""hunt2: dump TrueType table sizes for every embedded FontFile2 in a PDF."""
import collections, struct, sys
import pikepdf


def tables(data):
    n = struct.unpack(">H", data[4:6])[0]
    out = []
    for i in range(n):
        off = 12 + 16 * i
        tag = data[off:off + 4].decode("latin1")
        toff, tlen = struct.unpack(">II", data[off + 8:off + 16])
        out.append((tag, tlen))
    return out


def main():
    grand = collections.Counter()
    for path in sys.argv[1:]:
        pdf = pikepdf.open(path)
        print(f"== {path}")
        for obj in pdf.objects:
            if not isinstance(obj, pikepdf.Stream) or "/Length1" not in obj:
                continue
            data = obj.read_bytes()
            raw = len(obj.read_raw_bytes())
            try:
                ts = tables(data)
            except Exception:
                continue
            s = " ".join(f"{t}:{l}" for t, l in sorted(ts, key=lambda x: -x[1]))
            print(f"  decoded {len(data):7d} raw {raw:7d}  {s}")
            for t, l in ts:
                grand[t] += l
    print("== grand totals by table")
    for t, l in grand.most_common():
        print(f"   {t} {l}")


if __name__ == "__main__":
    main()
