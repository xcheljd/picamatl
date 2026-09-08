#!/usr/bin/env python3
"""hunt4 item 1: split the jpegtran headroom into Huffman re-optimisation
(strictly lossless, coefficients untouched) vs marker dropping (-copy none,
a metadata concern), and verify the decoded pixels are bit-identical."""
import io
import subprocess
import sys

import pikepdf
from PIL import Image

cur = copyall = copynone = 0
for path in sys.argv[1:]:
    pdf = pikepdf.open(path)
    n = bad = 0
    for o in pdf.objects:
        if not isinstance(o, pikepdf.Stream):
            continue
        if "DCTDecode" not in str(o.get("/Filter", "")):
            continue
        raw = bytes(o.read_raw_bytes())
        n += 1
        a = subprocess.run(
            ["jpegtran", "-optimize", "-copy", "all"], input=raw, capture_output=True
        ).stdout
        b = subprocess.run(
            ["jpegtran", "-optimize", "-copy", "none"], input=raw, capture_output=True
        ).stdout
        cur += len(raw)
        copyall += min(len(a) or len(raw), len(raw))
        copynone += min(len(b) or len(raw), len(raw))
        try:
            if Image.open(io.BytesIO(raw)).tobytes() != Image.open(io.BytesIO(b)).tobytes():
                bad += 1
        except Exception:  # noqa: BLE001
            pass
    print(f"{path.split('/')[-1]:26s} {n} DCT streams, pixel mismatches {bad}")
print(
    f"cur {cur} | -optimize -copy all {copyall} (SAVE {cur - copyall}) "
    f"| -optimize -copy none {copynone} (SAVE {cur - copynone})"
)
