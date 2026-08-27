# `--flatten-forms` — opt-in interactive-form flattening

Status: **implemented** (`src/forms.rs`, library `with_flatten_forms(bool)`,
CLI `--flatten-forms` / `--no-flatten-forms`, default **off**).

Every structural claim below was measured on 2026-08-24 against the files named
in [Corpus survey](#corpus-survey), not recalled.

---

## Why this exists

picamatl leaves every piece of form machinery alone by default, and will keep
doing so. Form structure is where documents break and — much worse — where
*entered data lives*. Ghostscript is 4× smaller than picamatl on
`corpus-expanded/irs-w2.pdf` (183,761 B vs 1,392,531 B) purely because
`pdfwrite` deletes `/AcroForm`, all 568 widget annotations and the 1.58 MB XFA
packet set unconditionally — including, on a *filled* form, the values.

`--flatten-forms` is the consent to trade interactivity for bytes, under a
contract that Ghostscript does not offer: **no value is ever silently lost.**
Where a value cannot be preserved, picamatl declines the whole document and hands
back the input bytes.

The difference is not theoretical. `scripts/forms-vs-gs.sh` runs both tools
over three inputs:

| input | Ghostscript `/ebook` | picamatl `--flatten-forms` |
| --- | --- | --- |
| filled AcroForm, values **with** appearance streams | flattens; 2,469 → **2,780 B** (grew) | flattens; 2,469 → **1,248 B** |
| filled AcroForm, a value with **no** appearance | drops `/AcroForm`; the field data is gone, the ink is regenerated | **declines** (D9); 2,454 → 1,450 B, form and all four values intact |
| `xfa_filled_imm1344e.pdf`, a real filled **dynamic XFA** form | 3,023,968 → 4,158 B: the output is the *"Please wait…"* placeholder page and **all 9,298 filled data nodes are gone** | **declines** (D3); 3,023,968 → 343,099 B losslessly, all 9,298 data nodes intact |

The third row is the one that matters. Ghostscript does not know it is looking
at a document whose entire content lives in an XML packet it is about to
delete, so it deletes it and reports success.

## Corpus survey

| file | AcroForm | XFA | `/NeedsRendering` | widgets | widgets that draw ink | fields with a value | verdict |
| --- | --- | ---: | --- | ---: | ---: | ---: | --- |
| `corpus-expanded/irs-w2.pdf` | yes | 1,583,884 B (9 packets) | absent | 568 | **0** | 40 (`/Btn`, all `/Off`) | **flatten** |
| `corpus-expanded/census-brief.pdf` | yes, `/Fields` empty | — | absent | 0 | 0 | 0 | **flatten** (vestigial `/DR`) |
| `corpus-expanded/xfa_filled_imm1344e.pdf` | yes | 2,451,896 B (10 packets), **481,594 B of filled `datasets`** | **true** | 1 | 1 | 1 | **decline** |
| `corpus-expanded/xfa_issue14315.pdf` | yes | 3,080 B (6 packets) | **true** | 0 | 0 | 0 | **decline** |
| `corpus/irs-1040gi.pdf` | no | — | — | 0 (498 `/Link`) | — | — | no-op |
| every other corpus file | no | — | — | 0 | — | — | no-op |

The two XFA declines are downloaded from the Mozilla pdf.js test corpus into
the gitignored `corpus-expanded/`; `scripts/forms-census.py` re-derives the
whole table from any set of PDFs.

`irs-w2.pdf` is the shape this feature was built for and it is worth spelling
out, because it is not the shape people assume. It is a **static** ("XFA
foreground") form: the 11 pages carry 313,208 bytes of real content streams
that draw the entire W-2, and the XFA layer only supplies Acrobat's interactive
rendering on top. Of its 568 widget annotations, **528 have no `/AP` at all**
and the remaining 40 have an `/AP /N` state dictionary containing only a `/1`
state while `/AS` is `/Off` — a state that is not in the dictionary, so ISO
32000-1 12.5.5 says nothing is drawn. 106 of them are additionally `NoView`.
The entire AcroForm layer of that file draws exactly zero ink. Removing it is
not "flattening" in the render-into-the-page sense; it is removing 1.6 MB of
machinery that no non-Acrobat viewer ever consults and that holds no data.

## What "the data is preserved" means

A field's value is **preserved** iff at least one of these holds. Anything
else declines the document.

* **(P1) Burned.** The widget is visible (neither `Hidden` nor `NoView`), its
  appearance stream is selected unambiguously, and that stream — byte for byte,
  with its own `/Resources` — is drawn into the page content stream at the
  widget's `/Rect` under the ISO 32000-1 12.5.5 `/BBox`→`/Rect` mapping. What
  the reader painted before is what the page paints after, and the text
  operators that painted it are now in the page's content stream, so the value
  becomes extractable by ordinary text extraction that never looked inside
  annotation appearances.
* **(P2) Inert.** The field has nothing to lose: `/V` is absent, an empty
  string, or the button off-state `/Off`, **and** the XFA `datasets` node that
  mirrors it is empty or `0`.

Note what P1 buys that a "keep a non-interactive remnant" design does not: the
value ends up in the *page*, which is the only place every viewer, printer and
text extractor agrees to look. Keeping `/V` in a stripped field tree would keep
the bytes we are trying to remove and would still not make the value visible in
a viewer that ignores widget appearances.

## Decline rules

The plan is all-or-nothing per document. Any of these returns "no plan" and the
document is optimized exactly as it would have been without the flag:

| # | condition | why |
| --- | --- | --- |
| D1 | encrypted, or PDF/A-declared | same posture as every other structural pass |
| D2 | no `/AcroForm` in the catalog | nothing to flatten; widget annots without a field tree are not guessed at |
| D3 | `/NeedsRendering true` (catalog) | ISO 32000-1 12.7.8's marker for a **dynamic XFA** form. The static pages are a "please wait" placeholder; there is nothing to flatten and the reader builds the page from the template at view time. `xfa_filled_imm1344e.pdf` is exactly this |
| D4 | an XFA `datasets` (or `form`) leaf carries text that the AcroForm field tree does not mirror, or `/XFA` is a single-stream XDP rather than a packet array | the data lives only in the XML, and a single-stream XDP cannot be split into packets without an XML parser picamatl is not adding. See [XFA mirroring](#xfa-mirroring) |
| D5 | `/NeedAppearances true` **and** any field has a non-empty value | the reader was told to *generate* appearances from `/V`; the stored `/AP` may be stale or missing and picamatl has no text layout engine to generate one |
| D6 | a `/FT /Sig` field whose `/V` is a dictionary | the document carries a real signature in a form field; flattening would delete it |
| D7 | a widget with `/OC` | visibility depends on optional-content state; burning it into the page makes it unconditional |
| D8 | a `Hidden` or `NoView` widget that *does* draw ink | view/print divergence cannot be expressed in a page content stream |
| D9 | a field with a non-empty value **none** of whose widgets burns an appearance | the value would vanish. This is the core data-preservation gate. Note that it is per *field*, not per widget: a radio group's unselected buttons all inherit the group's `/V` and paint nothing, and only the selected one has to burn |
| D10 | `/AP /N` is a state subdictionary and `/AS` is absent | ISO 32000-1 requires `/AS` here; guessing which state to burn is guessing at data |
| D11 | a field with a non-empty value that has no widgets at all, or none on any page | the same gate as D9 reaching the case where there was never anything to burn |
| D12 | a widget to burn whose `/AP` stream has no `/BBox`, a degenerate `/BBox`/`/Rect`, or a non-invertible `/Matrix` | the 12.5.5 mapping is undefined |
| D13 | a page whose content cannot be parsed, contains an inline image (`BI`), is not `q`/`Q` balanced, or ends inside an unclosed text object (`BT` with no `ET`) | see [Content-stream splicing](#content-stream-splicing) |
| D14 | a page to burn into whose `/Resources` or `/Resources /XObject` does not resolve to a dictionary | the burn names would have nowhere to bind, and a `Do` on an undefined name is skipped *silently* — the one way a value could disappear without the document declining |
| D15 | a widget to burn whose appearance stream has no `/Resources` and whose operators name one (`Tf`, `Do`, `gs`, `sh`, `BDC`, a non-device `cs`/`CS`, a pattern `scn`) | it was resolving those names against `/AcroForm /DR`, which this pass deletes. Same silent-`Do` failure mode as D14 |

Degenerate is not "small": a zero-width or zero-height `/Rect` or transformed
`/BBox` is what D12 rejects.

## XFA mirroring

For a *static* XFA form, Acrobat keeps the AcroForm field tree as a complete
mirror of the XFA `datasets` data. That mirror is what makes flattening safe:
if every piece of data in the XML also exists as an AcroForm `/V`, then
flattening the AcroForm faithfully flattens the XFA data too.

picamatl checks exactly that, with a ~90-line dependency-free XML leaf scanner
(`datasets_leaves`, no XML crate, no namespaces resolved — it only needs
element names and character data):

1. Collect every `<name>text</name>` leaf with non-whitespace `text`.
2. For each, look up the AcroForm leaf fields whose partial name `/T` equals
   `name` with any trailing `[n]` index stripped.
3. Require at least one such field, and require **every** match's effective
   value to be equivalent to `text`: a `/V` string equal to it, a `/V` name
   equal to it, or the pair (`/Off`, `"0"`) — XFA's canonical checkbox
   off-state against PDF's.

`irs-w2.pdf` has 16 non-empty leaves, all `0`, all matching a `/Btn` field
whose `/V` is `/Off` — inert by P2, so the whole XFA packet set is droppable.
A form filled with `<f1_05>Ada Lovelace</f1_05>` and no matching AcroForm `/V`
declines at D4. One filled *with* a mirrored `/V` proceeds to D9/P1, which is
the right answer: burn the appearance that shows it, or decline.

## What is removed

* `/AcroForm` from the catalog — with it `/XFA` (all packets), `/Fields`,
  `/DR`, `/DA`, `/SigFlags`. `prune_objects()` then collects the field tree and
  the XFA streams.
* `/NeedsRendering` from the catalog (only ever present when we declined, but
  removed for completeness if a `false` one is there).
* Every `/Widget` annotation, from every page's `/Annots`. Non-widget
  annotations — `/Link`, `/Text`, `/Popup`, markup — are **not touched**; they
  are not form machinery. An `/Annots` array left empty is removed.
* `/Perms /UR3` — the Adobe Reader usage-rights signature that grants exactly
  the local-form-filling and save rights being removed. `irs-w2.pdf` carries a
  12 KB one. `/Perms /DocMDP` is left alone. `/Perms` is removed when it
  becomes empty.
* `/Type /OBJR` structure-tree entries whose `/Obj` pointed at a removed
  widget, so the tagged-PDF tree keeps no dangling object references. The
  `/ParentTree` keeps its now-unused numeric keys — nothing looks up a key that
  no longer exists, and rebuilding a number tree to save a few dozen bytes is
  not worth the risk.

## Content-stream splicing

For each burned widget the page gains, at the very end of its content:

```
q  <a> <b> <c> <d> <e> <f> cm  /AmXf<n> Do  Q
```

`/AmXf<n>` is a globally fresh name bound to the *original, unmodified*
appearance-stream object in the page's `/XObject` resources (mutated in place —
inherited and shared resource dictionaries are fine, because the names are
globally unique, so a page that gains an unused name renders identically).
`<a>..<f>` is matrix **A** from ISO 32000-1 12.5.5: map the four `/BBox`
corners through `/Matrix`, take the axis-aligned bounds of the result, and
scale/translate those bounds onto the normalized `/Rect`. The `Do` operator
concatenates `/Matrix` itself, so the effective form-space→page-space matrix is
`Matrix × A` = the spec's **AA**.

To make that `cm` mean what it says, the page's own content must start from the
initial CTM. A content stream may legally leave an arbitrary CTM behind (a
top-level `cm` outside any `q`), so picamatl prepends a stream containing `q` and
appends one containing `Q` before the widget operators, restoring the initial
graphics state. That is only sound if the original content never pops past its
own base level and ends balanced, so the plan parses the concatenated content
with `Content::decode_strict`, refuses inline images (`BI`, which a naive
scanner would miscount `q`/`Q` inside), and requires the `q`/`Q` depth to stay
`>= 0` throughout and end at `0` — and the last `BT` to have been closed, since
a `cm` inside a text object is not legal. Otherwise: D13.

The prepended `q` streams are byte-identical across pages and the existing
`dedup_streams` fixpoint collapses them to one object; `minify_content_streams`
runs after flattening and folds the spliced pieces back into one minified
stream where it owns them.

## Ordering

Flattening runs early in `try_optimize` — after the dedup passes, **before**
`minify_content_streams` and before any image or font planning — so every later
pass sees the final object graph: the appearance streams are page content and
get minified and re-deflated like any other, and the XFA/field-tree objects are
already unreferenced when `prune_objects()` runs.

## Results

`corpus-expanded/irs-w2.pdf`, picamatl defaults plus the flag:

| | bytes | of input |
| --- | ---: | ---: |
| input | 2,150,352 | 100.0% |
| picamatl defaults (today) | 1,392,531 | 64.8% |
| picamatl defaults + `--flatten-forms` | 250,229 | 11.6% |
| Ghostscript `/ebook` (deletes the form layer unconditionally) | 183,761 | 8.5% |

And with the rest of the opt-in flags on (the `scripts/bench-full.sh` kitchen
sink, zlib backend):

| | bytes | of input | pages differing from the original render |
| --- | ---: | ---: | ---: |
| picamatl kitchen sink, `--no-flatten-forms` | 1,281,292 | 59.6% | — |
| picamatl kitchen sink, `--flatten-forms` | **140,215** | **6.5%** | 0 of 11 vs the line above |
| Ghostscript `/ebook` | 189,094 | 8.8% | **11 of 11**, up to 0.55% of pixels |

That is the whole point of the exercise: on the file Ghostscript used to win by
4× it is now picamatl that is smaller, and picamatl's output is the pixel-identical
one.

Render fidelity was measured with `scripts/forms-verify.py` (Ghostscript, 100
and 150 dpi, grayscale, anti-aliasing off). On a flatten-*only* run
(`--target-dpi 0 --no-downsample-flate-images --no-subset-fonts`, so the
comparison is about flattening and nothing else) all 11 pages of `irs-w2.pdf`
and all 9 pages of `census-brief.pdf` are pixel-identical to the input.

That measurement has since been re-run under the harsher oracle the
`flatten_render_moves_no_subpixel_*` tests use — poppler, anti-aliasing **on**,
comparing every subpixel rather than every pixel — and it holds: across the
whole corpus, flatten-only output is bit-identical to the input at both 100 and
150 dpi, on every page of every file that has a form. Anti-aliasing off is the
weaker check of the two; it only sees a geometry change once an edge crosses a
pixel centre. Offsetting the burn `cm` by 0.02 pt fails both, by 0.005 pt only
the anti-aliased one, by 0.001 pt neither.

Two corpus files *do* render differently, and neither has a form: `wiki-pdf.pdf`
(27 of 28 pages) and `wiki-cmyk-topic.pdf` (8 of 10). The cause is not
flattening — the flag is inert on both, D2 — and it is not any opt-in pass; it
reproduces with every flag off. It is `lopdf`'s `Object::Real(f32)` rewriting
`/MediaBox [0 0 595.91998 841.91998]` as `595.92 x 841.92`, which moves the
origin of the page-to-device transform by 2e-5 pt and re-grid-fits every glyph
on the page. Restoring only those four literals takes both files to 0 differing
pages. See `docs/upstream-lopdf-f32-reals.md`; there is no fix available
downstream, because the digits are gone by the time `Document::load` returns.

`census-brief.pdf` gains a small win (a vestigial `/AcroForm` whose `/DR` font
resources become unreferenced). Every other corpus file is untouched: no
`/AcroForm`, D2.

## Honest limits

* **Dynamic XFA is not rendered.** picamatl will never grow an XFA layout engine;
  D3 declines those documents forever. That is the honest answer, not a
  placeholder.
* **Appearances are copied, not generated.** If a producer wrote `/V` without
  an `/AP` and set `/NeedAppearances`, only the reader can lay that text out.
  picamatl declines (D5/D9) rather than guess a font, size, quadding or comb
  layout.
* **Flattening is one-way and it is a semantic change.** The output is not a
  form any more: no filling, no field export, no FDF round trip, no signing.
  That is the entire point of the flag being off by default.
* **A `Hidden` field's value disappears with the field.** D9 catches it: a
  hidden widget paints nothing, so it burns nothing, so a field whose only
  widgets are hidden has no widget accounting for its value and the document
  declines.
* **Ink equality is asserted by construction, and verified by rendering.** The
  burned stream is the original appearance object, unmodified, under the
  spec's own placement matrix; `tests/forms_flatten.rs` additionally renders the
  original (with annotations) and the flattened output — through Ghostscript
  with anti-aliasing off, and through poppler with it on — and compares the
  pages subpixel for subpixel.
