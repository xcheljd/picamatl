#!/usr/bin/env python3
"""hunt4 items 1-8: re-measure the small/marginal rejections on amatl output.

  1 jpegtran-style DCT cleanup  (lossless JPEG re-optimisation)
  2 payload-only dedup residue  (beyond the shipped dict-aware dedup)
  3 content-stream whitespace   (beyond the shipped minifier)
  4 XMP pruning                 (beyond --strip-metadata)
  6 ICC profile pruning
  7 gray-in-RGB
  8 already-optimal image reflate
"""
import collections
import hashlib
import io
import subprocess
import sys
import zlib

import pikepdf

try:
    from PIL import Image
except ImportError:  # noqa: BLE001
    Image = None


def jpegtran(path):
    """Item 1: lossless DCT re-optimisation of every DCTDecode stream."""
    pdf = pikepdf.open(path)
    cur = new = n = 0
    for o in pdf.objects:
        if not isinstance(o, pikepdf.Stream):
            continue
        f = o.get("/Filter")
        if f is None or "DCTDecode" not in str(f):
            continue
        raw = bytes(o.read_raw_bytes())
        best = len(raw)
        for args in (
            ["jpegtran", "-optimize", "-copy", "none"],
        ):
            try:
                r = subprocess.run(args, input=raw, capture_output=True, check=True)
            except Exception:  # noqa: BLE001
                continue
            if r.stdout and len(r.stdout) < best:
                best = len(r.stdout)
        n += 1
        cur += len(raw)
        new += best
    print(f"  [1] DCT streams {n}: cur {cur} jpegtran-best {new} SAVE {cur - new}")


def payload_dedup(path):
    """Item 2: identical decoded payloads that the shipped dict-aware dedup
    could not collapse, split by whether the dicts also agree."""
    pdf = pikepdf.open(path)
    by_payload = collections.defaultdict(list)
    for o in pdf.objects:
        if not isinstance(o, pikepdf.Stream):
            continue
        try:
            d = bytes(o.read_bytes())
        except Exception:  # noqa: BLE001
            continue
        key = hashlib.sha256(d).hexdigest()
        dk = hashlib.sha256(
            repr(sorted((str(k), str(v)) for k, v in o.items() if str(k) != "/Length")).encode()
        ).hexdigest()
        by_payload[key].append((dk, len(bytes(o.read_raw_bytes()))))
    same_dict = diff_dict = 0
    for members in by_payload.values():
        if len(members) < 2:
            continue
        groups = collections.defaultdict(list)
        for dk, sz in members:
            groups[dk].append(sz)
        for sizes in groups.values():
            same_dict += sum(sizes) - max(sizes)
        diff_dict += (
            sum(s for _, s in members) - max(s for _, s in members)
        ) - sum(sum(v) - max(v) for v in groups.values())
    print(
        f"  [2] equal-payload residue: same-dict (should be 0, shipped) {same_dict}, "
        f"differing-dict (unsafe to share) {diff_dict}"
    )


def whitespace(path):
    """Item 3: further whitespace/token squeeze on the largest content streams."""
    pdf = pikepdf.open(path)
    streams = []
    for pg in pdf.pages:
        c = pg.obj.get("/Contents")
        for s in c if isinstance(c, pikepdf.Array) else [c]:
            if isinstance(s, pikepdf.Stream):
                streams.append(s)
    streams.sort(key=lambda s: -len(bytes(s.read_bytes())))
    cur = new = 0
    doubles = runs = 0
    for s in streams[:40]:
        d = bytes(s.read_bytes())
        cur += len(zlib.compress(d, 9))
        doubles += d.count(b"  ")
        # squeeze: collapse any run of whitespace to one byte
        out = bytearray()
        prev_ws = False
        for b in d:
            ws = b in (0x20, 0x0A, 0x0D, 0x09, 0x00, 0x0C)
            if ws:
                if not prev_ws:
                    out.append(0x0A)
                    runs += 1
                prev_ws = True
            else:
                out.append(b)
                prev_ws = False
        new += len(zlib.compress(bytes(out), 9))
    print(
        f"  [3] top-{min(40, len(streams))} content streams: cur z9 {cur} "
        f"ws-squeezed z9 {new} SAVE {cur - new} (double-space occurrences {doubles})"
    )


def xmp(path):
    pdf = pikepdf.open(path)
    n = tot = 0
    for o in pdf.objects:
        if isinstance(o, pikepdf.Dictionary) and "/Metadata" in o:
            m = o.get("/Metadata")
            if isinstance(m, pikepdf.Stream):
                n += 1
                tot += len(bytes(m.read_raw_bytes()))
    print(f"  [4] /Metadata packets {n}, raw bytes {tot}")


def icc(path):
    pdf = pikepdf.open(path)
    seen = {}
    tot = 0
    for o in pdf.objects:
        if not isinstance(o, pikepdf.Stream) or "/N" not in o:
            continue
        dec = bytes(o.read_bytes())
        # /N also means "object count" on an ObjStm; require the ICC 'acsp' tag.
        if len(dec) < 40 or dec[36:40] != b"acsp":
            continue
        d = bytes(o.read_raw_bytes())
        tot += len(d)
        seen.setdefault(hashlib.sha256(d).hexdigest(), []).append(len(d))
    dup = sum(sum(v) - max(v) for v in seen.values() if len(v) > 1)
    print(
        f"  [6] ICC streams {sum(len(v) for v in seen.values())} raw {tot}, "
        f"distinct {len(seen)}, duplicate bytes (already shared?) {dup}"
    )


def images(path):
    """Items 7 + 8: gray-in-RGB, and reflate headroom on Flate images."""
    pdf = pikepdf.open(path)
    grayable = 0
    reflate = 0
    n = 0
    for o in pdf.objects:
        if not isinstance(o, pikepdf.Stream) or str(o.get("/Subtype")) != "/Image":
            continue
        f = str(o.get("/Filter", ""))
        if "FlateDecode" not in f:
            continue
        n += 1
        raw = bytes(o.read_raw_bytes())
        d = bytes(o.read_bytes())
        best = min(len(zlib.compress(d, 9)), len(raw))
        reflate += len(raw) - best
        cs = str(o.get("/ColorSpace", ""))
        if cs == "/DeviceRGB" and int(o.get("/BitsPerComponent", 8)) == 8:
            if all(d[i] == d[i + 1] == d[i + 2] for i in range(0, len(d) - 2, 3)):
                grayable += 1
    print(f"  [7] Flate images {n}: channel-identical RGB {grayable}")
    print(f"  [8] Flate image reflate headroom (z9 vs stored) {reflate}")


for path in sys.argv[1:]:
    print(path.split("/")[-1])
    for fn in (jpegtran, payload_dedup, whitespace, xmp, icc, images):
        try:
            fn(path)
        except Exception as e:  # noqa: BLE001
            print(f"  {fn.__name__}: ERROR {e}")
