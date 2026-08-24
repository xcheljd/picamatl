#!/usr/bin/env python3
"""Inspect colorspace arrays of Flate images in adobe-spec; gray-in-RGB check on ICCBased N=3."""
import zlib, pikepdf

def inspect(path):
    pdf = pikepdf.open(path)
    seen = set(); out = []
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
            cs = xo.get("/ColorSpace")
            desc = str(cs)
            kind = "other"
            ncomp = None
            hival = None
            if isinstance(cs, pikepdf.Array):
                head = str(cs[0])
                if "/ICCBased" in head:
                    ncomp = int(cs[1]["/N"]); desc = f"ICCBased N={ncomp}"; kind = "icc"
                elif "/Indexed" in head:
                    base = cs[1]
                    if isinstance(base, pikepdf.Array):
                        ncomp = int(base[1]["/N"])
                        desc = f"Indexed[ICCBased N={ncomp}]"; kind = "indexed-icc"
                        hival = None
                    else:
                        hival = int(str(cs[1])); desc = f"Indexed base={cs[2]} hival={hival} base={cs[3]}"; kind = "indexed"
            out.append((og[0], kind, ncomp, hival, int(xo["/Width"]), int(xo["/Height"]), len(xo.read_raw_bytes()), xo))
    from collections import Counter
    c = Counter((k, n) for _, k, n, _, _, _, _, _ in out)
    for d, n in c.most_common(): print(n, "x", d)
    print("--- largest ---")
    for o, k, n, hv, w, h, slen, xo in sorted(out, key=lambda t: -t[4]*t[5])[:8]:
        print(o, k, f"N={n}", f"hival={hv}", f"{w}x{h}", f"{slen}B")
    # gray-in-RGB for ICC N=3 flate images
    print("--- gray-in-RGB check (ICC N=3) ---")
    for o, k, n, hv, w, h, slen, xo in out:
        if k != "icc" or n != 3: continue
        raw = xo.read_bytes()
        if len(raw) != w*h*3:
            print(o, "size mismatch", len(raw), "expect", w*h*3); continue
        r = raw[0::3]; g = raw[1::3]; b = raw[2::3]
        isg = (r == g == b)
        print(o, f"{w}x{h} {slen}B gray_in_rgb={isg}")
        if isg:
            gray = bytes(r)
            newd = zlib.compress(gray, 9)
            print("   -> re-encode no-pred:", len(newd), "save", slen-len(newd))

inspect("corpus/adobe-spec.pdf")
print("\n=== nist-ssdf ===")
inspect("corpus/nist-ssdf.pdf")
