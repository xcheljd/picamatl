# Compression hunt, round 3

Continues `docs/hunts/hunt2-notes.md`. Three commits this round: `e74deff` (strip-hinting),
`3ccb245` (Type1C family merge), `5d00644` (merge generality fixes). Gates for
every claim: `cargo test --release` / clippy `-D warnings` / fmt, plus for the
size claims `pdftoppm` 150dpi sha256 render compare, pass-2 idempotence, and
`gs -sDEVICE=nullpage` (error counts unchanged from the inputs; irs carries a
pre-existing "error executing PDF token" that gs also reports on the original).

## Shipped

### 1. `--strip-hinting` (opt-in; rasterization-lossy at small sizes) — e74deff

Drops `fpgm`/`prep`/`cvt ` and every per-glyph instruction block from TrueType
programs the subsetter already rewrites (both the CID and simple paths),
rebuilding `glyf`/`loca`/`maxp` and all checksums. HUNT2 estimated 77 KB
*decoded* on nist; the compressed reality is far larger because hinting
bytecode compresses poorly:

- nist 561,239 → **421,214** (−140,025, −25% of output)
- irs 4,262,520 → 4,104,255 (−158 KB; its CID TrueType subsets are hint-heavy)
- arxiv −30,634; adobe −5,184

poppler renders every page of nist/arxiv (and irs pp1–40) byte-identically at
150 dpi — FreeType's autohinter ignores the embedded programs — but classic
GDI-style rasterizers do not, hence its own opt-in flag rather than a default.

### 2. Lossless same-family Type1C union merge — 3ccb245 + 5d00644

The C-M2 "irs CFF" item, and the round's key discovery: **no show-string
rewriting is needed at all** for simple (non-CID) fonts, because glyph lookup
goes PDF `/Encoding` → glyph *name* → CFF charset. What looked like per-subset
charstring conflicts is width-operand relativity: the leading width delta is
relative to each fragment's own `nominalWidthX`, so the same glyph
byte-differs across fragments. Probing irs: **all 513 apparent conflicts
across 11 families were pure width-encoding artifacts** — identical absolute
width, identical remaining bytes.

`src/cffmerge.rs` merges a family only when: global+local subr INDEXes are
byte-identical; Private DICTs agree semantically outside
`defaultWidthX`/`nominalWidthX`/`Subrs`; every shared glyph name is
byte-identical after width normalization; FontMatrix equal; not CID-keyed;
charset/encoding formats parse. PDF side: explicit `/Encoding` with a named
base — or an unnamed base when the fragment's built-in encoding is verifiably
empty (the merged program then carries an explicitly empty built-in);
`/Widths` present; descriptor/FontFile3 sharing admitted only when every
reference comes from group members. Appended charstrings are re-based on the
base fragment's width parameters byte-exactly; a parse-back round trip
re-verifies before shipping. All members get identical stream bytes; the
existing dedup collapses them to one object. Runs under default
`subset_fonts` (pixel-lossless; all 126 irs pages render byte-identical).

- irs 4,262,520 → **4,158,663** (−103,857 B — ~96% of the 108,621 B measured
  headroom; the remainder is TimesNewRomanPSMT ×2, which is TrueType, plus a
  Helvetica-Bold pair with an Expert-predefined built-in, declined)
- adobe/arxiv/nist: 0 (see dead ends)

## Measured dead ends this round

- **Lossy quality ladder (retry q85/q90 on q78 MAD decline).** Instrumented
  `decode_back_matches`: **zero MAD declines** across nasa-cli (11.5 MB),
  nasa-dm1-2 (7.5 MB), adobe, and irs under `--allow-lossy`. Every decline in
  practice is the 5% size guard, which a *higher*-quality retry cannot pass.
  No beneficiary exists; not implemented.
- **adobe-spec Type1C families (est 18 KB).** The MyriadPro/MinionPro
  fragments have genuinely different charstring *bodies* for shared glyphs
  (49/33/19 conflicts with equal widths but different bytes — different subr
  factoring or hints from separate subsetting runs). The byte-conservative
  tier correctly declines. Recovering this needs a Type2 interpreter proving
  outline equivalence and accepting hint variance — see below.

## Recommended, not implemented

- **Interpreter-verified merge tier.** A Type2 charstring interpreter
  (absolute-outline + width comparison) would unlock adobe's ~18 KB and any
  producer that re-subsets with different hint factoring. Since choosing one
  hint variant can shift small-size rasterization, it belongs behind
  `--strip-hinting`'s consent, which already covers exactly that class of
  change. Fonttools-probe confirms all adobe conflicts are outline-identical.
- **TrueType family union merge** (arxiv residual ~14 KB, irs
  TimesNewRomanPSMT ~7 KB): the TT analogue over glyf/loca/cmap union; more
  invasive (gid remapping re-enters CIDToGIDMap territory) for less headroom.

## Final table (bytes)

| file | input | round-2 default | **round-3 default** | round-2 best opt-ins¹ | **round-3 best opt-ins²** |
|---|---|---|---|---|---|
| adobe-spec | 22,491,828 | 7,166,167 | 7,166,167 | 6,289,645 | **6,284,454** |
| arxiv-attention | 2,233,053 | 1,475,800 | 1,475,800 | 1,268,302 | **1,237,672** |
| irs-1040gi | 4,434,643 | 4,262,520 | **4,158,663** | 4,261,904 | **3,997,915** |
| nist-ssdf | 739,891 | 561,239 | 561,239 | 559,602 | **419,580** |
| dummy | 13,264 | 12,459 | 12,459 | 12,459 | 12,459 |

¹ round-2: `--strip-metadata --convert-type1 --recompress-bitonal-images`.
² round-3: same + `--strip-hinting`.

NASA files (lossy validation targets):

| file | input | default | `--allow-lossy --strip-hinting --strip-metadata` |
|---|---|---|---|
| nasa-cli | 11,511,039 | 4,444,328 | **3,151,713** |
| nasa-dm1-2 | 7,512,010 | 4,426,136 | **3,133,538** |

Round-3 deltas: default −103,857 (irs); opt-ins −862,679 corpus-wide
(irs −263,989, nist −140,022, arxiv −30,630, adobe −5,191).

New CLI this round: `--strip-hinting` / `--no-strip-hinting`
(`OptimizeOptions::with_strip_hinting`). The T1C merge is on by default under
`subset_fonts`.
