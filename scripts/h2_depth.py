#!/usr/bin/env python3
"""hunt2: measure lossless bit-depth reduction headroom for Flate images.

For each 8-bpc Indexed or DeviceGray image, find the smallest bit depth that
represents every sample used (Indexed: max index; Gray: levels evenly spaced
on the n-bit ladder), repack rows and re-deflate to get a real number.
"""
import sys, zlib
import pikepdf


def repack(data, w, h, bits, mapfn):
    rowin = w
    out = bytearray()
    for y in range(h):
        row = data[y * rowin:(y + 1) * rowin]
        acc = 0
        nb = 0
        rb = bytearray()
        for x in range(w):
            acc = (acc << bits) | mapfn(row[x])
            nb += bits
            while nb >= 8:
                nb -= 8
                rb.append((acc >> nb) & 0xFF)
        if nb:
            rb.append((acc << (8 - nb)) & 0xFF)
        out += rb
    return bytes(out)


def main():
    for path in sys.argv[1:]:
        pdf = pikepdf.open(path)
        gain = 0
        print(f"== {path}")
        for o in pdf.objects:
            if not isinstance(o, pikepdf.Stream) or str(o.get("/Subtype")) != "/Image":
                continue
            if "/FlateDecode" not in str(o.get("/Filter")) or int(o.get("/BitsPerComponent", 0)) != 8:
                continue
            cs = o.get("/ColorSpace")
            w, h = int(o.Width), int(o.Height)
            try:
                data = o.read_bytes()
            except Exception:
                continue
            if len(data) < w * h:
                continue
            indexed = isinstance(cs, pikepdf.Array) and str(cs[0]) == "/Indexed"
            grayish = str(cs) == "/DeviceGray"
            if not (indexed or grayish) or len(data) != w * h:
                continue
            used = sorted(set(data))
            if indexed:
                need = max(used)
                bits = 1 if need <= 1 else 2 if need <= 3 else 4 if need <= 15 else 8
                mapfn = lambda v: v
            else:
                # gray is losslessly re-depthable only if every level sits on
                # the n-bit ladder (v = round(i*255/(2^n-1)))
                bits = None
                for b in (1, 2, 4):
                    ladder = {round(i * 255 / ((1 << b) - 1)): i for i in range(1 << b)}
                    if all(v in ladder for v in used):
                        bits, mapfn = b, (lambda v, L=ladder: L[v])
                        break
                if bits is None:
                    continue
            if bits == 8:
                continue
            raw = len(o.read_raw_bytes())
            new = len(zlib.compress(repack(data, w, h, bits, mapfn), 9))
            gain += raw - new
            print(f"   {'Indexed' if indexed else 'Gray'} {w}x{h} levels {len(used)} "
                  f"-> {bits}bpc  raw {raw} -> {new}  (save {raw - new})")
        print(f"   total depth-reduction save {gain}")


main()
