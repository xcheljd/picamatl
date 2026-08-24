#!/usr/bin/env python3
"""Hunt2: gray-in-RGB check + measured DeviceGray re-encode savings; small-palette quick check."""
import json, zlib
import pikepdf

def _enc(ft, cur, prev, bpp):
    out = bytearray(len(cur))
    if ft == 0: return bytes(cur)
    for i in range(len(cur)):
        a = cur[i-bpp] if i >= bpp else 0
        b = prev[i]; c = prev[i-bpp] if i >= bpp else 0
        if ft == 1: v = a
        elif ft == 2: v = b
        elif ft == 3: v = (a+b)>>1
        else:
            p = a+b-c; pa,pb,pc = abs(p-a),abs(p-b),abs(p-c)
            v = a if (pa<=pb and pa<=pc) else (b if pb<=pc else c)
        out[i] = (cur[i]-v)&255
    return bytes(out)

census = json.load(open("scripts_hunt2_census.json"))

def get_imgs(path):
    pdf = pikepdf.open(path)
    seen = set(); out = []
    for page in pdf.pages:
        res = page.get("/Resources")
        if res is None or "/XObject" not in res: continue
        for name, xo in dict(res["/XObject"]).items():
            og = xo.objgen
            if og in seen or xo.get("/Subtype") != "/Image": continue
            seen.add(og)
            out.append((name, xo))
    return pdf, out

def apply_predictor(data, w, h, bpc, ncomp, parms):
    if parms is None: return data
    pred = int(parms.get("/Predictor", 1))
    if pred < 10:
        # TIFF predictor 2: just diff samples — rare here; bail out (report)
        return None
    nch = ncomp
    rowlen = (w * bpc * nch + 7)//8
    stride = rowlen + 1
    if len(data) != stride*h:
        return None
    out = bytearray()
    prev = bytes(rowlen)
    bpp = max(1, (bpc*nch+7)//8)
    pos = 0
    for _ in range(h):
        ft = data[pos]; row = bytearray(data[pos+1:pos+stride]); pos += stride
        if ft == 0: pass
        elif ft == 1:
            for i in range(bpp, len(row)): row[i] = (row[i]+row[i-bpp]) & 255
        elif ft == 2: row = bytearray(a+b for a,b in zip(row,prev))
        elif ft == 3:
            for i in range(len(row)):
                left = row[i-bpp] if i>=bpp else 0
                row[i] = (row[i] + ((left + prev[i])>>1)) & 255
        elif ft == 4:
            for i in range(len(row)):
                a = row[i-bpp] if i>=bpp else 0
                b = prev[i]; c = prev[i-bpp] if i>=bpp else 0
                p = a+b-c; pa,pb,pc = abs(p-a),abs(p-b),abs(p-c)
                pr = a if (pa<=pb and pa<=pc) else (b if pb<=pc else c)
                row[i] = (row[i]+pr)&255
        out += b"\x00"+bytes(row); prev = bytes(row)
    return bytes(out)

results = []
for key in ["adobe-spec","arxiv-attention","nist-ssdf"]:
    path = f"corpus/{key}.pdf"
    pdf, imgs = get_imgs(path)
    for name, xo in imgs:
        filt = xo.get("/Filter")
        filt = str(filt) if not isinstance(filt, pikepdf.Array) else ",".join(str(f) for f in filt)
        if "/FlateDecode" not in str(filt): continue
        try:
            slen = len(xo.read_raw_bytes())
        except Exception: continue
        cs = xo.get("/ColorSpace")
        css = str(cs) if not isinstance(cs, pikepdf.Array) else ""
        w,h,bpc = int(xo["/Width"]), int(xo["/Height"]), int(xo["/BitsPerComponent"])
        rec = {"file":key,"obj":f"{xo.objgen[0]}","w":w,"h":h,"bpc":bpc,"cs":css,"bytes":slen}
        if css in ("/DeviceRGB","/RGB"):
            try:
                raw = xo.read_bytes()
            except Exception as e:
                rec["err"]=str(e); results.append(rec); continue
            if len(raw) != w*h*3:
                rec["err"]=f"size {len(raw)} != {w*h*3}"; results.append(rec); continue
            r = raw[0::3]; g = raw[1::3]; b = raw[2::3]
            gray_in_rgb = (r==g==b)
            rec["gray_in_rgb"] = gray_in_rgb
            if gray_in_rgb:
                gray = bytes(r)
                parms = xo.get("/DecodeParms")
                pred = int(parms.get("/Predictor",1)) if parms is not None else 1
                rec["pred"] = pred
                newdata = None
                if pred >= 10:
                    # re-apply PNG predictors on gray rows
                    rowlen = w; stride=rowlen+1
                    body = bytearray()
                    prev = bytes(rowlen); bpp=1
                    for y in range(h):
                        src = gray[y*w:(y+1)*w]
                        best = min(range(5), key=lambda ft: _cost(ft,src,prev,bpp)) if False else 2
                        # use Paeth for consistency with typical encoders; measure with sub/up/paeth pick per-row
                        cands=[]
                        for ft in (0,1,2,3,4):
                            e=_enc(ft,src,prev,bpp)
                            cands.append((sum(abs((c<<24)>>24) for c in e), ft, e))
                        _,ft,e = min(cands)
                        body += bytes([ft])+e; prev=src
                    newdata = zlib.compress(bytes(body),9)
                    rec["predmode"]="png-adaptive"
                elif pred == 2:
                    rec["skip"]="tiff-pred"
                    results.append(rec); continue
                else:
                    newdata = zlib.compress(gray,9)
                    rec["predmode"]="none"
                rec["newbytes"]=len(newdata)
                rec["save"]=slen-len(newdata)
        elif css.startswith("[") or "ICCBased" in css or "Indexed" in css:
            pass
        # small-palette quick check for gray / indexed
        if css in ("/DeviceGray","/CalGray") and bpc==8 and "gray_in_rgb" not in rec:
            try:
                raw = xo.read_bytes()
                uniq = len(set(raw[::max(1,len(raw)//200000)]))
                rec["uniq_sampled"]=uniq
            except Exception: pass
        results.append(rec)
    pdf.close()

json.dump(results, open("scripts_hunt2_grayrgb.json","w"), indent=1)
for r in results:
    print(json.dumps(r))
