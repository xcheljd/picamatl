import pikepdf, collections, hashlib

FILES = ["corpus/adobe-spec.pdf","corpus/arxiv-attention.pdf","corpus/irs-1040gi.pdf","corpus/nist-ssdf.pdf"]

def scan(path):
    pdf = pikepdf.open(path)
    bf_of_stream = {}
    def handle_desc(desc, bf):
        try:
            for key in ("/FontFile", "/FontFile2", "/FontFile3"):
                if key in desc:
                    ff = desc[key]
                    if isinstance(ff, pikepdf.Stream):
                        bf_of_stream[ff.objgen] = bf
        except Exception:
            pass
    for obj in pdf.objects:
        try:
            if isinstance(obj, pikepdf.Dictionary) and "/BaseFont" in obj:
                bf = str(obj["/BaseFont"])
                desc = obj.get("/FontDescriptor")
                if desc is None and "/DescendantFonts" in obj:
                    try: desc = obj["/DescendantFonts"][0].get("/FontDescriptor")
                    except Exception: desc = None
                if desc is not None:
                    handle_desc(desc, bf)
            elif isinstance(obj, pikepdf.Dictionary) and "/FontName" in obj and "/FontFile" in str(obj):
                handle_desc(obj, str(obj["/FontName"]))
        except Exception:
            pass
    progs = []
    def norm(bf):
        b = bf.lstrip("/")
        # strip 6-letter uppercase subset tag
        if len(b) > 7 and b[6] == "+" and b[:6].isalpha() and b[:6].isupper():
            b = b[7:]
        return b
    for obj in pdf.objects:
        try:
            if not isinstance(obj, pikepdf.Stream):
                continue
            d = {}; st = None
            for k, v in obj.items():
                d[str(k)] = v
                if str(k) == "/Subtype": st = str(v)
            isfont = ("/Length1" in d) or st in ("/CIDFontType0C", "/OpenType", "/Type1C") or obj.objgen in bf_of_stream
            if isfont:
                raw = obj.read_bytes()
                kind = st if st else ("TrueType" if "/Length2" in d else "raw-Type1")
                progs.append((norm(bf_of_stream.get(obj.objgen, "(unattributed)")), kind, len(raw), hashlib.sha256(raw).hexdigest()[:12]))
        except Exception:
            pass
    print(f"== {path}: {len(progs)} font programs, {sum(p[2] for p in progs):,}B")
    groups = collections.defaultdict(list)
    for bf, k, l, h in progs:
        groups[(bf, k)].append((l, h))
    merge_total = 0
    for (bf, k), items in sorted(groups.items(), key=lambda x: -(sum(s for s, _ in x[1]) - max(s for s, _ in x[1]))):
        sizes = [s for s, _ in items]
        if len(sizes) > 1:
            save = sum(sizes) - max(sizes)
            merge_total += save
            exact = len(items) - len({h for _, h in items})
            print(f"   MERGE {bf} [{k}] x{len(sizes)} (exact-dup {exact}) sizes={sorted(sizes)} -> est save {save:,}B")
    print(f"   TOTAL est merge savings (sum-max heuristic): {merge_total:,}B")

for f in FILES:
    scan(f)
