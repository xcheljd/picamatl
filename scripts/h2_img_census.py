#!/usr/bin/env python3
"""hunt2: Flate image census — colorspace, bpc, and lossless reduction headroom
(RGB with R==G==B -> Gray; 8-bit gray with few distinct levels -> palette/1-bit)."""
import collections, sys, zlib
import pikepdf


def main():
    for path in sys.argv[1:]:
        pdf = pikepdf.open(path)
        print(f"== {path}")
        tot = gray_gain = pal_gain = 0
        for o in pdf.objects:
            if not isinstance(o, pikepdf.Stream) or str(o.get("/Subtype")) != "/Image":
                continue
            if "/FlateDecode" not in str(o.get("/Filter")):
                continue
            raw = len(o.read_raw_bytes())
            tot += raw
            cs = str(o.get("/ColorSpace"))
            bpc = int(o.get("/BitsPerComponent", 8))
            w, h = int(o.Width), int(o.Height)
            try:
                data = o.read_bytes()
            except Exception:
                continue
            note = ""
            if cs == "/DeviceRGB" and bpc == 8 and len(data) >= w * h * 3:
                px = data[:w * h * 3]
                if all(px[i] == px[i + 1] == px[i + 2] for i in range(0, len(px), 3)):
                    g = bytes(px[0::3])
                    gz = len(zlib.compress(g, 9))
                    gray_gain += raw - gz
                    note = f" GRAYABLE raw {raw} -> ~{gz}"
            if cs == "/DeviceGray" and bpc == 8:
                lv = len(set(data))
                if lv <= 16:
                    note += f" {lv} levels"
                    bits = 1 if lv <= 2 else (2 if lv <= 4 else 4)
                    rowb = (w * bits + 7) // 8
                    pal_gain += raw - len(zlib.compress(b"\0" * rowb * h, 9)) - 200
            print(f"   {cs:14s} bpc{bpc} {w}x{h} raw {raw:8d}{note}")
        print(f"   total flate image raw {tot}, gray-reduction gain ~{gray_gain}, "
              f"depth-reduction upper bound ~{pal_gain}")


main()
