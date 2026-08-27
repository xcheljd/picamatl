#!/usr/bin/env python3
"""Form census: what form machinery each PDF carries, and how much it costs.

Re-derives the corpus table in `docs/FORMS-PLAN.md`. Requires pikepdf.

    scripts/forms-census.py corpus/*.pdf corpus-expanded/*.pdf
"""
import collections, os, sys

import pikepdf


def is_stream(obj):
    try:
        obj.read_raw_bytes()
        return True
    except Exception:
        return False


def walk_fields(fields, out, depth=0):
    if depth > 32:
        return
    for field in fields:
        out.append(field)
        if "/Kids" in field:
            walk_fields(field.Kids, out, depth + 1)


def appearance_draws(pdf, annot):
    """Whether this widget's selected /AP /N paints anything at all."""
    ap = annot.get("/AP")
    if ap is None:
        return False
    normal = ap.get("/N")
    if normal is None:
        return False
    if is_stream(normal):
        return len(normal.read_bytes().strip()) > 0
    state = annot.get("/AS")
    if state is None:
        return False  # malformed: picamatl declines, but it draws nothing today
    selected = normal.get(str(state))
    if selected is None or not is_stream(selected):
        return False
    return len(selected.read_bytes().strip()) > 0


def report(path):
    pdf = pikepdf.open(path)
    root = pdf.Root
    acroform = root.get("/AcroForm")
    widgets = drawn = 0
    others = collections.Counter()
    for page in pdf.pages:
        for annot in page.get("/Annots", []):
            if annot.get("/Subtype") == "/Widget":
                widgets += 1
                drawn += appearance_draws(pdf, annot)
            else:
                others[str(annot.get("/Subtype"))] += 1

    xfa_bytes = 0
    xfa_packets = 0
    if acroform is not None and "/XFA" in acroform:
        xfa = acroform.XFA
        entries = xfa[1::2] if isinstance(xfa, pikepdf.Array) else [xfa]
        for entry in entries:
            if is_stream(entry):
                xfa_bytes += len(entry.read_raw_bytes())
                xfa_packets += 1

    fields = []
    if acroform is not None:
        walk_fields(acroform.get("/Fields", []), fields)
    valued = sum(1 for f in fields if "/V" in f)

    print(f"{os.path.basename(path)}")
    print(f"  size            {os.path.getsize(path):,}")
    print(f"  AcroForm        {'yes' if acroform is not None else 'no'}")
    print(f"  XFA             {xfa_bytes:,} B in {xfa_packets} packets")
    print(f"  NeedsRendering  {root.get('/NeedsRendering')}")
    print(f"  NeedAppearances {acroform.get('/NeedAppearances') if acroform is not None else None}")
    print(f"  field nodes     {len(fields)} ({valued} with /V)")
    print(f"  widgets         {widgets} ({drawn} draw ink)")
    print(f"  other annots    {dict(others) or '{}'}")


for arg in sys.argv[1:]:
    try:
        report(arg)
    except Exception as exc:  # noqa: BLE001 - a census must not stop on one bad file
        print(f"{os.path.basename(arg)}: ERROR {exc}")
