#!/usr/bin/env python3
"""h6_budget.py — per-file byte budget of a PDF, by object class.

Classifies every indirect object's *stored* byte cost (raw stream bytes plus
a dict estimate) into buckets: image classes (by filter x colorspace),
font programs (by type), content streams, other streams, non-stream objects.

Usage: h6_budget.py file.pdf [file.pdf ...]
"""
import sys, os, collections, pikepdf

def rawlen(obj):
    try:
        return len(obj.read_raw_bytes())
    except Exception:
        return 0

def cs_name(cs, pdf):
    try:
        if isinstance(cs, pikepdf.Name):
            return str(cs)[1:]
        if isinstance(cs, pikepdf.Array):
            fam = str(cs[0])[1:]
            if fam == "ICCBased":
                try:
                    n = int(cs[1].get("/N", 0))
                except Exception:
                    n = 0
                return f"ICC{n}"
            if fam in ("Indexed", "I"):
                base = cs_name(cs[1], pdf)
                return f"Indexed({base})"
            return fam
        return "?"
    except Exception:
        return "?"

def classify(o, pdf):
    d = o
    try:
        st = d.get("/Subtype")
    except Exception:
        return "other-stream"
    st = str(st) if st is not None else ""
    filt = d.get("/Filter")
    if isinstance(filt, pikepdf.Array):
        f = "+".join(str(x)[1:] for x in filt)
    elif filt is not None:
        f = str(filt)[1:]
    else:
        f = "none"
    if st == "/Image":
        cs = cs_name(d.get("/ColorSpace"), pdf) if "/ColorSpace" in d else ("ImageMask" if d.get("/ImageMask") else "?")
        bpc = int(d.get("/BitsPerComponent", 0) or 0)
        if d.get("/ImageMask"):
            cs = "ImageMask"
        smask = "+SM" if "/SMask" in d else ""
        return f"img:{f}:{cs}:{bpc}bpc{smask}"
    if st in ("/Type1C", "/CIDFontType0C", "/OpenType"):
        return f"font:{st[1:]}"
    if "/Length1" in d and "/Length2" in d:
        return "font:Type1"
    if "/Length1" in d:
        return "font:TrueType"
    t = str(d.get("/Type", ""))
    if t == "/ObjStm":
        return "objstm"
    if t == "/XRef":
        return "xref"
    if t == "/Metadata" or st == "/XML":
        return "metadata"
    if st == "/Form":
        return "form-xobject"
    return "stream:" + t.lstrip("/") if t else "stream:untyped"

def budget(path):
    pdf = pikepdf.open(path)
    buckets = collections.Counter()
    counts = collections.Counter()
    # content streams referenced by pages
    content_ids = set()
    for page in pdf.pages:
        c = page.obj.get("/Contents")
        items = c if isinstance(c, pikepdf.Array) else [c]
        for it in items:
            try:
                content_ids.add(it.objgen)
            except Exception:
                pass
    total_raw = 0
    for o in pdf.objects:
        try:
            if not isinstance(o, pikepdf.Stream):
                continue
        except Exception:
            continue
        n = rawlen(o)
        total_raw += n
        try:
            og = o.objgen
        except Exception:
            og = None
        k = "content" if og in content_ids else classify(o, pdf)
        buckets[k] += n
        counts[k] += 1
    fsize = os.path.getsize(path)
    return fsize, total_raw, buckets, counts

if __name__ == "__main__":
    grand = collections.Counter()
    gcounts = collections.Counter()
    gsize = 0
    for p in sys.argv[1:]:
        try:
            fsize, traw, b, c = budget(p)
        except Exception as e:
            print(f"{os.path.basename(p)}: FAILED {e}")
            continue
        gsize += fsize
        grand.update(b); gcounts.update(c)
        print(f"\n== {os.path.basename(p)}  file={fsize:,}  streams={traw:,} ({100*traw/fsize:.1f}%)")
        for k, v in b.most_common(12):
            if v < fsize * 0.005: break
            print(f"   {k:<44}{v:>10,}  {100*v/fsize:>5.1f}%  n={c[k]}")
    print(f"\n==== CORPUS TOTAL  files={gsize:,}")
    tot = sum(grand.values())
    for k, v in grand.most_common(40):
        print(f"   {k:<44}{v:>10,}  {100*v/gsize:>5.1f}%  n={gcounts[k]}")
    print(f"   {'(all streams)':<44}{tot:>10,}  {100*tot/gsize:>5.1f}%")
