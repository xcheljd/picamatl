#!/usr/bin/env python3
"""Measure zlib-9 reflate of arxiv RGB flate images (predictor-aware)."""
import zlib
import pikepdf

pdf = pikepdf.open("corpus/arxiv-attention.pdf")
seen = set()
for page in pdf.pages:
    res = page.get("/Resources")
    if res is None or "/XObject" not in res: continue
    for name, xo in dict(res["/XObject"]).items():
        og = xo.objgen
        if og in seen or xo.get("/Subtype") != "/Image": continue
        seen.add(og)
        filt = xo.get("/Filter")
        filt = str(filt) if not isinstance(filt, pikepdf.Array) else ",".join(str(f) for f in filt)
        if "/FlateDecode" not in filt: continue
        slen = len(xo.read_raw_bytes())
        raw = xo.read_bytes()
        w, h = int(xo["/Width"]), int(xo["/Height"])
        parms = xo.get("/DecodeParms")
        pred = int(parms.get("/Predictor", 1)) if parms is not None else 1
        # plain z9 on samples, no predictor
        plain = len(zlib.compress(raw, 9))
        # keep PNG predictors: re-filter rows adaptively (bpp=3)
        rowlen = w*3; bpp = 3
        body = bytearray(); prev = bytes(rowlen)
        for y in range(h):
            src = raw[y*rowlen:(y+1)*rowlen]
            best = None
            for ft in range(5):
                out = bytearray(len(src))
                if ft == 0:
                    out = bytearray(src)
                elif ft == 1:
                    for i in range(bpp, len(src)): out[i] = (src[i]-src[i-bpp]) & 255
                elif ft == 2:
                    for i in range(len(src)): out[i] = (src[i]-prev[i]) & 255
                elif ft == 3:
                    for i in range(len(src)):
                        a = src[i-bpp] if i>=bpp else 0
                        out[i] = (src[i]-((a+prev[i])>>1)) & 255
                else:
                    for i in range(len(src)):
                        a = src[i-bpp] if i>=bpp else 0
                        b = prev[i]; c = prev[i-bpp] if i>=bpp else 0
                        p = a+b-c; pa,pb,pc = abs(p-a),abs(p-b),abs(p-c)
                        pr = a if (pa<=pb and pa<=pc) else (b if pb<=pc else c)
                        out[i] = (src[i]-pr)&255
                cost = sum(1 for x in out if x)  # cheap proxy
                if best is None or cost < best[0]: best = (cost, bytes(out))
            body += b"\x02" + best[1]  # fixed ft=2 encoding of chosen filtered row
            prev = src
        png = len(zlib.compress(bytes(body), 9))
        print(f"obj{og[0]} {w}x{h} pred={pred} stored={slen} plain_z9={plain} png_pred={png}")
pdf.close()
