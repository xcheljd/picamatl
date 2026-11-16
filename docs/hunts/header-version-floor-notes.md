# Header version floor: every feature the output uses (accuracy notes)

Measured on main `5a64f56` (reference binary
`workspace/bin/picamatl-combo-5a64f56`) over the 32-file accuracy set
(`workspace/acc-files.txt`: 15 corpus files, 16 scans, `nasa.pdf`), 8 lanes
x 32 files = 256 outputs, all exit 0. Scripts (pica-builder
`workspace/reports/ver/`): `run-lanes.sh`, `feature_census.py` (pikepdf).

Lanes: `def` (defaults), `nopack` (`--no-pack-object-streams`), `nojbig2`
(`--no-jbig2-bitonal-images`), `nojbig2-nopack` (both), `type1`
(`--convert-type1`), `type1-nopack`, `kitchen` (every opt-in: strip
accessibility/metadata/private data, convert Type1, strip hinting, recompress
bitonal, allow lossy, collapse gray, flatten forms) and `kitchen-nopack`.

## Feature floors (ISO 32000-1:2008, checked against `corpus/adobe-spec.pdf`)

- Stream filters, Table 6: `FlateDecode` (PDF 1.2), `JBIG2Decode` (PDF 1.4),
  `JPXDecode` (PDF 1.5), `Crypt` (PDF 1.5). `ASCIIHexDecode`, `ASCII85Decode`,
  `LZWDecode`, `RunLengthDecode`, `CCITTFaxDecode`, `DCTDecode` carry no
  version annotation (PDF 1.0).
- Embedded fonts, Table 122: `FontFile2` (PDF 1.1), `FontFile3` (PDF 1.2).
  Tables 126/127, the `FontFile3` program's `/Subtype`: `Type1C` (PDF 1.2),
  `CIDFontType0C` (PDF 1.3), `OpenType` (PDF 1.6).
- 7.5.7 object streams and 7.5.8 cross-reference streams: "first introduced
  in PDF 1.5" / "Beginning with PDF 1.5".

What picamatl itself can write: `FlateDecode` (`doc.compress()`, re-deflate,
LZW/ASCII/RunLength transcodes, row filters, Flate→Flate image re-encodes),
`DCTDecode` and `CCITTFaxDecode` (1.0), `JBIG2Decode` (bitonal re-store),
`FontFile3`/`Type1C` (`--convert-type1`), object streams and an xref stream
(packing, on by default). It never writes `JPXDecode` (the opt-in JPX→JPEG
conversion removes it), `Crypt`, `CIDFontType0C`, `OpenType` or `FontFile2`
of its own; those reach the output only from the input.

Also counted, for information (picamatl never adds them): image `/SMask`
1.4, transparency `/Group` 1.4, catalog `/Metadata` 1.4, `/MarkInfo` 1.4,
`/StructTreeRoot` 1.3, `/OCProperties` 1.5, `/OutputIntents` 1.4, and
`/F /Fl` inline images in page content 1.2.

## Under-declared outputs on main (header < a feature the output uses)

4 of 256, all in `nojbig2-nopack`:

- us10174323 (`%PDF-1.1`): 60 `/FlateDecode` streams (PDF 1.2).
- us10522188 (`%PDF-1.1`): 34 `/FlateDecode` streams (PDF 1.2).
- gao-077234 (`%PDF-1.3`): catalog `/Metadata` (PDF 1.4) — informational.
- gao-104000 (`%PDF-1.3`): catalog `/Metadata` (PDF 1.4) — informational.

The two gao files carry that `/Metadata` stream in the original under the
same 1.3 header; the original is equally under-declared, and picamatl
neither adds nor removes it. No output in any lane under-declares JBIG2,
JPX, Type1C, CIDFontType0C, FontFile2, object streams or an xref stream.
In every other lane the patents are already raised (to 1.5 by packing, or
to 1.4 by the JBIG2 re-store), which hides the Flate gap.

Headers main changes, per lane (never lowered; the 1.4 raises are JBIG2,
the 1.5 raises are packing):

- def, nojbig2, type1: 12 raised to 1.5 — dummy, wiki-cmyk-topic, wiki-pdf,
  naca-19930081614, nasa-tn-d162 (1.4); gao-077234, gao-104000,
  us20120251502, us6811268 (1.3); fbi-custodial (1.2); us10174323,
  us10522188 (1.1). 2 passthroughs.
- kitchen: the same 12 plus pypdf-cmyk (1.3), usgs-bul1000 and usgs-bul1100
  (1.4), to 1.5 (`--strip-metadata` drops the PDF/A-1 claim). 0
  passthroughs.
- nopack, type1-nopack, kitchen-nopack: 7 raised to 1.4 — fbi-custodial,
  gao-077234, gao-104000, us10174323, us10522188, us20120251502, us6811268.
- nojbig2-nopack: none changed.

Feature presence over the 32 originals: Flate 25 files, FontFile2 12,
ObjStm 11, xref stream 8, FontFile3 6 (Type1C 4, CIDFontType0C 2), JBIG2 4,
JPX 2; informational: catalog /Metadata 19, StructTreeRoot 12, MarkInfo 11,
SMask 8, transparency groups 8, OutputIntents 5, OCProperties 1.
