#!/usr/bin/env bash
# forms-vs-gs.sh — what Ghostscript and amatl each do to a filled form.
#
# Two fixtures, one difference that matters:
#   fixtures/forms/filled-acroform.pdf     values WITH appearance streams
#   fixtures/forms/unappearanced-value.pdf a value with NO appearance stream
#     (the /NeedAppearances shape: the reader lays the text out from /V + /DA)
#
# Both tools flatten the first. On the second, gs drops /AcroForm anyway and
# the entered value is gone; amatl declines the document and keeps the form.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
bin="${AMATL_BIN:-$repo_root/target/release/amatl}"
out="$repo_root/target/scratch/forms-vs-gs"
mkdir -p "$out"

for name in filled-acroform unappearanced-value; do
  src="$repo_root/fixtures/forms/$name.pdf"
  gs -q -dNOPAUSE -dBATCH -sDEVICE=pdfwrite -dPDFSETTINGS=/ebook \
     -sOutputFile="$out/$name.gs.pdf" "$src"
  "$bin" --flatten-forms -o "$out/$name.amatl.pdf" "$src" >/dev/null 2>&1
  echo "== $name"
  python3 - "$src" "$out/$name.gs.pdf" "$out/$name.amatl.pdf" <<'PYEOF'
import os, sys
import pikepdf

def field_values(path):
    pdf = pikepdf.open(path)  # must outlive the objects read from it
    acroform = pdf.Root.get("/AcroForm")
    if acroform is None:
        return None
    found = []
    def walk(fields, depth=0):
        if depth > 32:
            return
        for field in fields:
            if "/V" in field:
                found.append(f"{field.get('/T')}={field.get('/V')}")
            if "/Kids" in field:
                walk(field.Kids, depth + 1)
    walk(acroform.get("/Fields", []))
    return found

for path in sys.argv[1:]:
    values = field_values(path)
    shown = "form removed" if values is None else ", ".join(map(str, values))
    print(f"  {os.path.basename(path):34s} {os.path.getsize(path):7,} B  {shown}")
PYEOF
done

# The decisive case, when the (gitignored) sample is present: a REAL filled
# dynamic XFA form. Ghostscript renders the "please wait" placeholder page and
# drops every one of its 9,298 filled data nodes; amatl declines at D3 and
# still shrinks the file 88.7% losslessly, datasets packet intact.
dynamic="$repo_root/corpus-expanded/xfa_filled_imm1344e.pdf"
if [ -f "$dynamic" ]; then
  gs -q -dNOPAUSE -dBATCH -sDEVICE=pdfwrite -dPDFSETTINGS=/ebook \
     -sOutputFile="$out/dynamic-xfa.gs.pdf" "$dynamic"
  "$bin" --flatten-forms -o "$out/dynamic-xfa.amatl.pdf" "$dynamic" >/dev/null 2>&1
  echo "== filled dynamic XFA (xfa_filled_imm1344e)"
  python3 - "$dynamic" "$out/dynamic-xfa.gs.pdf" "$out/dynamic-xfa.amatl.pdf" <<'XFAEOF'
import os, re, sys
import pikepdf

LEAF = re.compile(rb"<([\w:.-]+)[^>]*>([^<]*)</\1\s*>")

for path in sys.argv[1:]:
    pdf = pikepdf.open(path)
    acroform = pdf.Root.get("/AcroForm")
    filled = 0
    if acroform is not None and "/XFA" in acroform:
        xfa = acroform.XFA
        packets = dict(zip(map(str, xfa[0::2]), xfa[1::2]))
        datasets = packets.get("datasets")
        if datasets is not None:
            filled = sum(1 for m in LEAF.finditer(datasets.read_bytes()) if m.group(2).strip())
    print(f"  {os.path.basename(path):34s} {os.path.getsize(path):9,} B  "
          f"{filled:,} filled XFA data nodes")
XFAEOF
fi
