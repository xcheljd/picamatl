# Stale classic trailer keys and the header version (accuracy notes)

Measured on main `7b8a288` (reference binary `workspace/bin/picamatl-7b8a288`)
over the 32-file accuracy set (`workspace/acc-files.txt`: 15 corpus files, 16
scans, `nasa.pdf`). Scripts: `workspace/reports/acc/` (`stale.py`,
`version_census.py`, `props.py`, `check.py`, `pypdf_strict.py`).

## 1. Stale trailer keys with `--no-pack-object-streams`

### Symptom

`corpus/adobe-spec.pdf` (Distiller 8.1.0) ends in a classic `xref` table whose
`trailer` dictionary carries a cross-reference-STREAM key set:
`/DecodeParms<</Columns 5/Predictor 12>> /Filter /FlateDecode /Length 137
/Type /XRef /W [1 3 1] /Index [124332 848]` (plus `/Encrypt`, `/ID`, `/Info`,
`/Root`, `/Size 126989`).

lopdf seeds `Document::trailer` from that dictionary. `strip_stale_xref_trailer_keys`
drops `/DecodeParms /Filter /Prev /XRefStm /Length` on every path. With
packing off (`--no-pack-object-streams`) the output keeps the input's
classic table, and lopdf's classic writer sets only `/Size`, so the other
three keys reach the output:

    trailer
    <</Index[124332 848]/W[1 3 1]/ID[...]/Info 9 0 R/Type/XRef/Root 1 0 R/Size 118001>>

`/Type /XRef`, `/W` and `/Index` are not trailer keys (ISO 32000-1 Table 15);
they describe a cross-reference stream the file does not contain, and the
`/Index` names objects 124332-125179 while the output's `/Size` is 118001.
7b8a288 dropped these keys only on the PDF/A-1 path.

### What readers do with it

Nothing visible. On main's `--no-pack-object-streams` output of adobe-spec:
libqpdf (pikepdf 10.12 `check_pdf_syntax` + open warnings) 0 problems, 0
warnings; Ghostscript nullpage exit 0 with no messages; pypdf 6.16.2
`strict=True` loads all 756 pages with 0 warnings; poppler `pdfinfo -struct`
prints 1,237 "Loop detected in structure tree" warnings, the same count as
main's default output (the original prints 1,104 — a structure-tree property
of the input, not of the trailer). The keys are wrong data that every tested
reader ignores.

### Which files and lanes

Final-section key check (`stale.py`: classic trailer may carry only Table 15
keys; an xref stream only Table 15 + 17 + its own stream keys, with a
`/DecodeParms /Columns` equal to the `/W` sum) over 5 lanes x 32 files = 160
outputs: 157 clean. The 3 stale outputs are all adobe-spec:

- `--no-pack-object-streams`: `/Index /Type /W`
- `--no-jbig2-bitonal-images --no-pack-object-streams`: `/Index /Type /W`
- kitchen sink + `--no-pack-object-streams`: `/Index /Type /W`

and, run on adobe-spec only, `--allow-lossy --no-pack-object-streams` and
`--target-dpi 0 --no-pack-object-streams` show the same three keys.
Defaults and kitchen sink (packing on): 32/32 clean — the xref-stream writer
overwrites `/Type`, `/W` and `/Index`. No other input among the 32 ends in a
classic trailer carrying stream keys, so no other file is affected.

### Sibling found while fixing

The generic stream keys `/F /FFilter /FDecodeParms /DL` (Table 5) were not in
the strip list either. None of the 32 inputs carries them, but a constructed
input does show the leak on main in EVERY lane, defaults included: `/DL`
survives onto the output's xref stream (test
`stream_only_keys_never_reach_the_output_trailer` fails on main with
"classic input, pack=true: /DL leaked into the output trailer"). On an xref
stream `/F` would declare the table's data to live in an external file.

## 2. When the header version changes

Only two things ever write the header: lopdf raises it to 1.5 whenever it
writes an xref stream or object streams (`writer.rs`: `save_with_options`
with `use_xref_streams`, `save_with_object_streams`, and `save_internal` when
the kept xref form is a stream), and picamatl raises a header below 1.4 to
1.4 when the JBIG2 re-store wrote a `/JBIG2Decode` stream (`lib.rs`, after
the bitonal apply). Nothing lowers it, and nothing touches the catalog
`/Version` (usgs-pp250's `/Version /1.4` under a 1.5 header is kept as is).

Per lane, 32 files (`version_census.py`):

- defaults (pack on): 12 headers change, all raised to 1.5 —
  dummy, wiki-cmyk-topic, wiki-pdf, naca-19930081614, nasa-tn-d162 (1.4);
  gao-077234, gao-104000, us20120251502, us6811268 (1.3); fbi-custodial
  (1.2); us10174323, us10522188 (1.1). Every one of them carries an object
  stream and an xref stream.
- `--no-pack-object-streams`: 7 change, all raised to 1.4 by JBIG2 —
  gao-077234, gao-104000, us20120251502, us6811268 (1.3), fbi-custodial
  (1.2), us10174323, us10522188 (1.1).
- `--no-jbig2-bitonal-images --no-pack-object-streams`: none change.
- kitchen sink (pack on, `--strip-metadata` drops the PDF/A-1 claim): the
  12 above plus pypdf-cmyk (1.3), usgs-bul1000 and usgs-bul1100 (1.4), all
  to 1.5.
- kitchen sink + `--no-pack-object-streams`: the same 7 as the no-pack lane,
  to 1.4.
- Passthroughs (pypdf-cmyk and nbs-circ539v1 on defaults; irs-1040gi and
  nist-ssdf with packing off) are byte copies, header included.

PDF/A-1 path (usgs-bul1000, usgs-bul1100, usgs-pp150 on defaults and with
packing off): classic table, no object streams, header kept at 1.4, 1.4 and
1.6. These headers are already >= 1.4, so the JBIG2 raise cannot fire.

Is a raise ever unnecessary? Not on these files. Every default/kitchen output
with a raised header holds at least one `ObjStm` plus an xref stream (both
PDF 1.5), and every no-pack raise holds `/JBIG2Decode` streams (PDF 1.4).

The opposite does happen: an UNDER-declared header. With
`--no-jbig2-bitonal-images --no-pack-object-streams`, us10174323 and
us10522188 (PDF 1.1, CCITT only, no Flate) come out still saying 1.1 while
carrying 60 and 34 `/FlateDecode` streams (the content streams
`doc.compress()` deflated). `FlateDecode` is PDF 1.2. No measured reader
cares; noted for the lead, behaviour unchanged.

## Document properties on defaults (`props.py`)

Original vs main's default output, all 32: `/Info` values, trailer `/ID`,
catalog key set, catalog `/Version`, page count, every page's `/Rotate`,
MediaBox and CropBox are equal on all 32. Image `/ColorSpace` families are the
same set on all 32 (nasa's DeviceGray count falls 75 -> 65 because duplicate
images merge, not because anything was converted). The same holds with packing
off.
