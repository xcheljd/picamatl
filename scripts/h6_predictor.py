#!/usr/bin/env python3
"""h6_predictor.py — how much would PNG-predictor re-planning save?

For every FlateDecode image stream in a PDF that carries NO /DecodeParms
predictor today, try each PNG predictor (plus adaptive, PNG's own heuristic)
and report the best zlib-9 result against what is stored now.
"""
import sys, os, zlib, collections, pikepdf

def rows(data, stride, h):
    return [data[r*stride:(r+1)*stride] for r in range(h)]

def filt(row, prev, bpp, mode):
    n = len(row)
    if mode == 0:
        return bytes(row)
    out = bytearray(n)
    for i in range(n):
        a = row[i-bpp] if i >= bpp else 0
        b = prev[i]
        c = prev[i-bpp] if i >= bpp else 0
        if mode == 1: p = a
        elif mode == 2: p = b
        elif mode == 3: p = (a+b)//2
        else:
            pp = a+b-c; pa=abs(pp-a); pb=abs(pp-b); pc=abs(pp-c)
            p = a if (pa<=pb and pa<=pc) else (b if pb<=pc else c)
        out[i] = (row[i]-p) & 255
    return bytes(out)

def encode(data, stride, h, bpp, mode):
    out = bytearray()
    prev = bytes(stride)
    for row in rows(data, stride, h):
        if len(row) != stride: return None
        if mode == 5:  # adaptive: PNG's minimum-sum-of-absolute-differences
            best, bestscore = None, None
            for m in range(5):
                f = filt(row, prev, bpp, m)
                score = sum(min(x, 256-x) for x in f)
                if bestscore is None or score < bestscore:
                    best, bestscore, bm = f, score, m
            out.append(bm); out += best
        else:
            out.append(mode); out += filt(row, prev, bpp, mode)
        prev = row
    return bytes(out)

NCOMP = {"/DeviceGray":1, "/CalGray":1, "/DeviceRGB":3, "/CalRGB":3, "/DeviceCMYK":4}

def ncomp(cs, pdf):
    if isinstance(cs, pikepdf.Name):
        return NCOMP.get(str(cs))
    if isinstance(cs, pikepdf.Array):
        fam = str(cs[0])
        if fam == "/ICCBased":
            try: return int(cs[1]["/N"])
            except Exception: return None
        if fam in ("/Indexed", "/I"): return 1
        if fam in ("/Separation",): return 1
        if fam in ("/DeviceN",): return len(cs[1])
        if fam in ("/CalRGB", "/Lab"): return 3
        if fam == "/CalGray": return 1
    return None

def main(paths):
    tot_now = tot_best = 0
    per = collections.Counter()
    for path in paths:
        pdf = pikepdf.open(path)
        for o in pdf.objects:
            if not isinstance(o, pikepdf.Stream): continue
            if str(o.get("/Subtype")) != "/Image": continue
            f = o.get("/Filter")
            fs = [str(x) for x in f] if isinstance(f, pikepdf.Array) else ([str(f)] if f else [])
            if fs != ["/FlateDecode"]: continue
            if "/DecodeParms" in o and o.get("/DecodeParms") is not None:
                dp = o["/DecodeParms"]
                if isinstance(dp, pikepdf.Dictionary) and "/Predictor" in dp: continue
            w, h = int(o["/Width"]), int(o["/Height"])
            bpc = int(o.get("/BitsPerComponent", 8) or 8)
            n = 1 if o.get("/ImageMask") else ncomp(o.get("/ColorSpace"), pdf)
            if not n: continue
            try: data = o.read_bytes()
            except Exception: continue
            stride = (w*n*bpc + 7)//8
            if stride*h != len(data): continue
            bpp = max(1, (n*bpc+7)//8)
            now = len(o.read_raw_bytes())
            best, bestmode = now, None
            for mode in (5,):
                enc = encode(data, stride, h, bpp, mode)
                if enc is None: continue
                z = len(zlib.compress(enc, 9))
                if z < best: best, bestmode = z, mode
            tot_now += now; tot_best += best
            if bestmode is not None:
                per[os.path.basename(path)] += now - best
    for k, v in per.most_common():
        print(f"  {k:<24} -{v:,}")
    print(f"TOTAL flate images (no predictor today): {tot_now:,} -> {tot_best:,}  (-{tot_now-tot_best:,})")

main(sys.argv[1:])
