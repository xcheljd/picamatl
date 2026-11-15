#!/usr/bin/env python3
"""Census of CCITT image XObjects by DecodeParms shape.

Usage: census-ccitt-parms.py FILE.pdf [...]   (needs pikepdf)

Prints one line per (file, K, EncodedByteAlign, ImageMask, EndOfLine,
EndOfBlock, BlackIs1) group with the image count and stored bytes, then a
per-file EncodedByteAlign subtotal and the grand totals. Only image
XObjects are counted (inline images never reach the bitonal pass).
"""
import sys
from collections import defaultdict

import pikepdf


def parms_of(obj):
    dp = obj.get("/DecodeParms")
    filt = obj.get("/Filter")
    if isinstance(filt, pikepdf.Array):
        names = [str(f) for f in filt]
        if isinstance(dp, pikepdf.Array):
            dps = list(dp)
        else:
            dps = [dp] * len(names)
        for n, d in zip(names, dps):
            if n == "/CCITTFaxDecode":
                return True, d, len(names)
        return False, None, len(names)
    if filt is not None and str(filt) == "/CCITTFaxDecode":
        return True, dp, 1
    return False, None, 0


def g(d, key, default):
    if d is None or not isinstance(d, pikepdf.Dictionary):
        return default
    v = d.get(key)
    if v is None:
        return default
    try:
        return int(v) if not isinstance(v, bool) else v
    except Exception:
        return v


def main():
    grand = defaultdict(lambda: [0, 0])
    for path in sys.argv[1:]:
        groups = defaultdict(lambda: [0, 0])
        with pikepdf.open(path) as pdf:
            for obj in pdf.objects:
                if not isinstance(obj, pikepdf.Stream):
                    continue
                if obj.get("/Subtype") != "/Image":
                    continue
                ccitt, dp, nfilters = parms_of(obj)
                if not ccitt:
                    continue
                key = (
                    g(dp, "/K", 0),
                    bool(g(dp, "/EncodedByteAlign", False)),
                    bool(obj.get("/ImageMask", False)),
                    bool(g(dp, "/EndOfLine", False)),
                    bool(g(dp, "/EndOfBlock", True)),
                    bool(g(dp, "/BlackIs1", False)),
                    nfilters,
                )
                size = len(obj.read_raw_bytes())
                groups[key][0] += 1
                groups[key][1] += size
        name = path.rsplit("/", 1)[-1]
        eba = [0, 0]
        for key in sorted(groups):
            n, b = groups[key]
            k, ba, mask, eol, eob, bi1, nf = key
            print(
                f"{name}: K={k} EncodedByteAlign={ba} "
                f"{'ImageMask' if mask else 'Image'} EndOfLine={eol} "
                f"EndOfBlock={eob} BlackIs1={bi1} filters={nf}: "
                f"{n} images, {b:,} B"
            )
            grand[(k, ba, mask)][0] += n
            grand[(k, ba, mask)][1] += b
            if ba:
                eba[0] += n
                eba[1] += b
        if eba[0]:
            print(f"{name}: EncodedByteAlign subtotal {eba[0]} images, {eba[1]:,} B")
    print("TOTALS")
    for key in sorted(grand):
        k, ba, mask = key
        n, b = grand[key]
        print(
            f"  K={k} EncodedByteAlign={ba} {'ImageMask' if mask else 'Image'}: "
            f"{n} images, {b:,} B"
        )


if __name__ == "__main__":
    main()
