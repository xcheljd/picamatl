#!/usr/bin/env python3
"""Hunt2 image census: image XObjects per corpus file via pikepdf."""
import json, sys
import pikepdf

FILES = {
    "adobe-spec": "corpus/adobe-spec.pdf",
    "arxiv-attention": "corpus/arxiv-attention.pdf",
    "irs-1040gi": "corpus/irs-1040gi.pdf",
    "nist-ssdf": "corpus/nist-ssdf.pdf",
}

def cs_name(cs):
    if cs is None:
        return None
    try:
        if isinstance(cs, pikepdf.Name):
            return str(cs)
        return str(cs.get("/Name", "?" + str(cs[0] if len(cs) else "?")))
    except Exception:
        return str(cs)

def census(path):
    pdf = pikepdf.open(path)
    seen = set()
    imgs = []
    for page in pdf.pages:
        res = page.get("/Resources")
        if res is None or "/XObject" not in res:
            continue
        for name, xo in dict(res["/XObject"]).items():
            objid = xo.objgen
            if objid in seen or xo.get("/Subtype") != "/Image":
                continue
            seen.add(objid)
            filt = xo.get("/Filter")
            filt = str(filt) if not isinstance(filt, pikepdf.Array) else ",".join(str(f) for f in filt)
            try:
                slen = len(xo.read_raw_bytes())
            except Exception:
                try:
                    slen = int(xo.stream_dict.get("/Length", -1))
                except Exception:
                    slen = -1
            imgs.append({
                "obj": f"{objid[0]} {objid[1]}",
                "filter": filt,
                "w": int(xo.get("/Width", 0)), "h": int(xo.get("/Height", 0)),
                "bpc": int(xo.get("/BitsPerComponent", 0)),
                "cs": cs_name(xo.get("/ColorSpace")),
                "bytes": slen,
            })
    pdf.close()
    return imgs

out = {}
for key, path in FILES.items():
    out[key] = census(path)
    # incremental write
    with open("scripts_hunt2_census.json", "w") as f:
        json.dump(out, f)
    tot = sum(i["bytes"] for i in out[key])
    from collections import Counter
    byf = Counter()
    for i in out[key]:
        byf[i["filter"]] += i["bytes"]
    print(f"{key}: {len(out[key])} images, {tot:,}B total | " +
          ", ".join(f"{k}={v:,}B(n={sum(1 for i in out[key] if i['filter']==k)})" for k, v in byf.most_common()))
