#!/usr/bin/env python3
"""Quick check: distinct index usage + measured 4bpc repack savings for indexed Flate images."""
import zlib
import pikepdf

def probe(path, label):
    pdf = pikepdf.open(path)
    seen = set(); rows = []
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
            if not isinstance(cs, pikepdf.Array) or "/Indexed" not in str(cs[0]): continue
            hival = int(str(cs[1]))
            w, h = int(xo["/Width"]), int(xo["/Height"])
            slen = len(xo.read_raw_bytes())
            raw = xo.read_bytes()
            uniq = len(set(raw))
            row = dict(obj=og[0], w=w, h=h, hival=hival, uniq=uniq, bytes=slen)
            # measured 4bpc repack when it would fit (uniq<=17 incl. need pow2 check)
            if uniq <= 17:
                packed = bytearray()
                for i in range(0, len(raw)-1, 2):
                    packed.append((raw[i] << 4) | raw[i+1])
                    if len(raw) % 2: packed.append(raw[-1] << 4)
                row["packed4_z9"] = len(zlib.compress(bytes(packed), 9))
            rows.append(row)
    pdf.close()
    print(f"== {label}: {len(rows)} indexed flate images ==")
    tot_slen = sum(r["bytes"] for r in rows); tot_new = sum(r.get("packed4_z9", r["bytes"]) for r in rows)
    for r in sorted(rows, key=lambda x: -x["bytes"]):
        extra = f" -> packed4+z9={r['packed4_z9']}B save={r['bytes']-r['packed4_z9']}" if "packed4_z9" in r else ""
        print(f"  obj{r['obj']} {r['w']}x{r['h']} hival={r['hival']} uniq={r['uniq']} {r['bytes']}B{extra}")
    print(f"  TOTAL stored={tot_slen:,}B best-case-packed={tot_new:,}B save~{tot_slen-tot_new:,}B")

probe("corpus/adobe-spec.pdf", "adobe-spec")
probe("corpus/nist-ssdf.pdf", "nist-ssdf")
