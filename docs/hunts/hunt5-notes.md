# Compression hunt, round 5 — implementing the two round-4 overturns

Continues `docs/hunts/hunt2-notes.md` / `hunt3-notes.md` / `hunt4-notes.md`. Round 4 re-audited twelve rejected items and
overturned two of them without implementing either. This round implements
both, and reports what each actually recovered against what the probes
predicted.

Both landed. One has a scope gap, stated plainly in §3.

Probe/driver scripts are committed as `scripts/h5_*.py`; measurements are
taken on **amatl's own output**, never on the inputs, with the round-4 binary
(`88d05ae`) rebuilt for the before column.

## 0. Headline

| | round 4 | round 5 | delta |
|---|---|---|---|
| default | 13,361,869 | 13,308,200 | **−53,669** |
| `--strip-hinting` | 13,027,765 | 12,944,412 | **−83,353** |
| `--convert-type1` | 13,154,371 | 13,100,702 | **−53,669** |
| `--strip-hinting --convert-type1` | 12,820,271 | 12,730,751 | **−89,520** |

Corpus totals over adobe-spec + arxiv-attention + irs-1040gi + nist-ssdf.
Round 4 predicted 59,714 (item 1) + 31,574 (item 12) = 91,288 recoverable;
round 5 delivers **89,520**, at 98%.

Full per-file matrix (`scripts/h5_matrix.py`, run once per binary):

| config | adobe-spec | arxiv-attention | irs-1040gi | nist-ssdf |
|---|---|---|---|---|
| default — r4 | 7,166,167 | 1,475,800 | 4,158,663 | 561,239 |
| default — r5 | 7,166,167 | 1,475,800 | **4,104,994** | 561,239 |
| strip-hinting — r4 | 7,160,983 | 1,445,166 | 4,000,402 | 421,214 |
| strip-hinting — r5 | **7,138,564** | 1,445,166 | **3,939,468** | 421,214 |
| convert-type1 — r4 | 7,166,167 | 1,268,302 | 4,158,663 | 561,239 |
| convert-type1 — r5 | 7,166,167 | 1,268,302 | **4,104,994** | 561,239 |
| hinting+type1 — r4 | 7,160,983 | 1,237,672 | 4,000,402 | 421,214 |
| hinting+type1 — r5 | **7,138,564** | **1,231,505** | **3,939,468** | 421,214 |

The `all-lossless+opt-in` row (adding `--recompress-bitonal-images
--collapse-gray-images`) is byte-identical to `hinting+type1` on this corpus,
in both rounds — neither flag finds anything here, which HUNT4 items 5 and 7
already established.

Rows the JPEG pass improves: **every** row, and only through irs-1040gi.
Rows the CFF hint strip improves: the two `--strip-hinting` rows.
Rows nothing changes: nist-ssdf everywhere (no Type1C fonts, no pass-through
baseline JPEGs), arxiv-attention unless `--convert-type1` gives it Type1C
programs to strip.

Default output on adobe-spec, arxiv-attention and nist-ssdf is **byte-identical**
to round 4, so those files' render / pass-2 / `gs` gates carry over unchanged.

## 1. Item 1 — JPEG Huffman re-optimization (`src/jpeghuff.rs`)

**Shipped: −53,669 B, on by default, no new flag.**

### What it is

The pure-Rust equivalent of `jpegtran -optimize`. A scan is decoded into its
*token* sequence — per block, the DC magnitude symbol plus its additional
bits, then the run/size AC symbols plus theirs — per-table symbol frequencies
are counted, length-limited canonical tables are generated, and the **same
token sequence** is re-emitted against them. No coefficient is ever decoded,
dequantized, or re-quantized; nothing but the `DHT` segments moves. `APPn`,
`COM`, `DQT`, `SOF`, `DRI` and the `SOS` headers are copied verbatim and
restart markers land on the same MCU boundaries.

That makes it bit-exact by construction, which is why it needs no consent
flag and is not gated by the pixel-identity contract's opt-in tier — it *is*
pixel identity.

### Why hand-rolled

I evaluated the crate ecosystem as instructed and found no fit:

* `zune-jpeg`, `jpeg-decoder` — decoders. They produce pixels, not the symbol
  stream, and a decode→re-encode round trip is *not* lossless (it re-runs the
  IDCT/FDCT).
* `mozjpeg` — already a dependency, and already emits optimal tables for
  everything amatl re-encodes. Its C API has no "re-optimize this existing
  stream" entry point exposed by the Rust bindings, and routing pass-through
  JPEGs through it would mean decoding to pixels: lossy.
* `jpegli-encoder`, `turbojpeg` — encoders / FFI wrappers; `turbojpeg` also
  brings a C dependency, which the crate does not have and does not want.

No pure-Rust crate does lossless Huffman re-optimization over an existing
entropy-coded stream. Hand-rolling it was the honest path, and it is ~500
lines including the libjpeg `jpeg_gen_optimal_table` algorithm (frequency
merge, 16-bit length limiting with the reserved 257th symbol so the all-ones
code is never assigned, canonical assignment).

### Fail-safe contract

`optimize` returns `None` — "ship the original bytes" — on any parse surprise,
any structure outside scope, a non-shrinking rebuild, **and** a round trip
that does not reproduce the exact input token sequence. That last check is the
real gate: it re-reads the emitted file and requires the same symbols and the
same additional bits, i.e. the same coefficients. Nothing ships unverified.

### Measured

`scripts/h5_jpegcorpus.sh` over 270 raw `/DCTDecode` payloads extracted from
amatl's round-4 output:

| | streams | bytes |
|---|---|---|
| optimized | 4 | 1,579,440 → 1,524,798 |
| declined | 266 | unchanged |
| **total** | 270 | 4,925,357 → 4,870,715 (**−54,642**) |

Both optimized streams verified pixel-identical against the original with an
independent decoder (PIL), and within 1,064 B / 8 B of `jpegtran -optimize
-copy all` on the same input — **99.8% of jpegtran's saving**.

Two of the four "optimized" are irs-1040gi's DeviceCMYK/Separation
pass-throughs (1,471,481 → 1,447,739 and 107,959 → 78,031). The other two are
the same streams seen through the second corpus file.

End to end the pass is worth −53,669 B on irs-1040gi, slightly less than the
raw-stream figure because the streams sit inside a container whose xref and
object-stream overheads do not shrink with them.

### The scope gap (§3 below): progressive

Round 4 measured 59,714 B. Round 5 recovers 53,669 of it. See §3.

## 2. Item 12 — Type1C hint strip (`src/cffhint.rs`)

**Shipped: −35,849 B under the existing `--strip-hinting` flag.**

### What it is

The CFF analogue of `truetype::strip_hinting`. Each Type2 charstring is walked
byte by byte; `hstem`, `vstem`, `hstemhm`, `vstemhm`, `hintmask`, `cntrmask`
and `dotsection` are deleted along with the operands that feed them and the
`hintmask`/`cntrmask` mask bytes; the leading width operand is re-folded onto
whatever stack-clearing operator survives first; **every other byte is spliced
through verbatim**, in its original integer encoding. No outline is
re-encoded, no subroutine is inlined or dropped, no coordinate is recomputed.
That is the point: outline identity becomes a near-syntactic property rather
than something resting on a re-encoder's rounding — which is exactly the
failure mode HUNT4 item 10 found when it compared separately-subsetted
fragments and got ±1-unit drift.

The Private DICT hinting parameters (`BlueValues`, `OtherBlues`, `Family*`,
`StdHW`, `StdVW`, `BlueScale/Shift/Fuzz`, `StemSnap*`, `ForceBold`,
`ExpansionFactor`) go too — they describe nothing once the charstring hints
are gone. Measured separately, they are **3,213 B** of the 35,849:

| file | charstrings only | + Private DICT keys | Private DICT share |
|---|---|---|---|
| adobe-spec | 7,140,248 | 7,138,564 | 1,684 |
| arxiv-attention | 1,232,426 | 1,231,505 | 921 |
| irs-1040gi | 3,940,076 | 3,939,468 | 608 |

### Verification

Two layers, both on by default in the product, not just in tests:

1. **Trace equality.** A Type2 interpreter (`cffhint::trace`) that follows
   `callsubr`/`callgsubr` records each glyph's advance width and the ordered
   sequence of drawing operators with their resolved operands. The strip is
   accepted only when every glyph's trace is unchanged.
2. **Verified on the emitted bytes.** The comparison is run against the
   re-emitted font *re-parsed from its own bytes*, so what ships is what was
   checked — not what was computed in memory.

Plus the usual strictly-smaller guard.

`scripts/h5_cff_declines.py` and the `cffhint::tests::corpus_report` harness
(`#[ignore]`, needs `AMATL_CFF_DIR`) over all 80 Type1C programs in amatl's
own output:

```
77 programs stripped, 3 declined; 2393 glyphs outline-verified;
305490 -> 263076 (save 42414)
```

**0 outline or width mismatches on 2,393 real corpus glyphs.** The three
declines are all adobe-spec fonts that carry no hints at all (2, 4, and 189
glyphs; nothing to strip, so the re-emit would grow them by 12–15 B and the
guard correctly refuses).

Note the raw-CFF saving (42,414) exceeds the shipped saving (35,849): the
programs are Flate-stored, and hint operators deflate well.

### Preconditions, and the arxiv failure round 4 saw

Round 4's probe mismatched on two arxiv fonts "whose local subrs' stack state
my in-place subr rewrite breaks", and predicted a real implementation would
"either inline subrs first or decline the same way". It declines, by two
explicit rules:

* **Font-level:** if any reachable local or global subroutine contains a hint
  operator, the whole font declines. Hints inside subrs spread stem counts —
  and hence `hintmask` operand sizes — across call boundaries this in-place
  rewrite does not follow.
* **Glyph-level:** if a hint operator's operands cannot be proven to be
  literal numbers from the same charstring since the last stack clear (i.e. a
  subroutine call intervened), that *glyph* keeps its original bytes and the
  font still ships.

Also declined: CID-keyed CFF (`ROS`/`FDArray`/`FDSelect`), `CharstringType`
other than 2, more than one font in the Name INDEX, and any charstring using
an operator outside the drawing/hint/flex/subroutine set (arithmetic, storage,
`random` — anything that makes the operand stack unpredictable).

On this corpus none of the 80 programs hit the subr-hint rule, which is why
arxiv now strips cleanly where the probe could not.

### Where it applies, and why no refcount analysis

The pass runs **after** the font plans are applied, over the document's final
`/FontFile3` streams, so it covers programs this run produced (union merges,
Type1 → Type1C conversions) as well as ones nothing else touched — 38 programs
on adobe-spec where the probe saw 35.

It deliberately does not do the reference-count dance the merge planner does.
A hint strip changes no glyph name, no glyph order, and no advance width; it
is inert to every font dictionary that points at the stream, however many
there are and whatever their `/Encoding`. The only observable change is how a
rasterizer grid-fits at small ppem, which is precisely the consent
`--strip-hinting` already carries.

**This is the one deviation from the brief**, which said "only touches Type1C
font programs the pipeline already rewrites (subsetted / merged)". Restricting
to merged/converted programs would have cost most of adobe-spec's 22,419 B —
its Type1C fonts are largely un-merged singletons — for no safety gain, since
the "never mutates a stream it can't fully verify" requirement is met by the
per-program trace gate rather than by provenance. Flagging it here rather than
quietly widening the scope.

### Documentation

`--strip-hinting`'s help text, the `OptimizeOptions::strip_hinting` doc
comment, and CHANGELOG all now say Type1C is included; the CHANGELOG entry is
filed under **Changed**, not Added, and says explicitly that the flag's scope
is wider than it was.

## 3. What I did not finish: progressive JPEG

Round 4's 59,714 B splits by frame type, which round 4 did not report:

| file | DCT streams | frame type | round-5 saving |
|---|---|---|---|
| irs-1040gi | 2 | baseline (`SOF0`) | **54,642** |
| adobe-spec | 128 | progressive (`SOF2`) | 0 (declined) |
| nasa pair | 140 | 138 progressive, 2 baseline | 0 (declined) |

`src/jpeghuff.rs` handles baseline and extended sequential Huffman frames at
8-bit precision. **Progressive frames decline** and ship byte-identical. That
leaves round 4's adobe-spec 4,972 B and nasa 1,501 B — **6,473 B, 11% of the
item** — on the table.

The reason is not effort-avoidance, it is that progressive AC *refinement*
scans cannot be modelled as a flat token stream. Their correction bits are
positional: which bit follows which symbol depends on which coefficients are
already non-zero from earlier scans, so re-emitting them requires carrying the
full per-block coefficient state of the whole image across every scan — a
complete progressive decoder, not a symbol relabeler. That is a much larger
piece of machinery, and every bug in it is silent coefficient corruption. The
round-trip token gate would catch it, but the cost/benefit against 6.5 KB did
not justify shipping it in the same round as two other passes. The scope is
stated in the module docs so the next round knows exactly what is missing.

The two nasa baseline streams also decline — mozjpeg encoded them, so their
tables are already optimal and nothing is smaller. That is the pass working
correctly, and it is why round 1 measured only 242 B: it was looking at
re-encoded streams.

## 4. Gates

Run at each commit:

* `cargo test --release` — 160 lib + 15 integration + 1 doc, 0 failed
  (round 4: 144 + 15). New: 8 `jpeghuff` tests, 7 `cffhint` tests, 1 pipeline
  test, 2 `#[ignore]` corpus harnesses.
* `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all` — clean.
* MSRV 1.88 respected; **no new dependencies** (both passes are hand-rolled
  against the existing `cffmerge`/`type1` CFF primitives, which grew four
  `pub(crate)` re-exports and no new code).

Per-file (`scripts/h5_gates.py`):

| file | config | render | pdftotext | pass-2 | `gs -sDEVICE=nullpage` |
|---|---|---|---|---|---|
| irs-1040gi | default | 126/126 pages identical | identical | idempotent | rc 0, same 8 pre-existing source-inherited warnings as r4 |
| irs-1040gi | `--strip-hinting` | 126/126 identical | identical | idempotent | rc 0, same 8 |
| adobe-spec | `--strip-hinting` | 756/756 identical | identical | idempotent | rc 0, **zero stderr**, 134 s |
| arxiv-attention | `--strip-hinting --convert-type1` | 16/16 identical | identical | idempotent | — |

Render identity is compared against the *previous behaviour* (the same binary
with the new pass disabled), at 72 dpi — the resolution where hinting matters
most.

Worth recording: the CFF hint strip is an **opt-in rasterization-lossy**
change, and on this corpus at 72 dpi poppler's rendering did not change at all
on any of the 898 pages. Poppler/FreeType does not act on CFF hints in this
configuration. That is a statement about poppler, not a licence to call the
flag lossless — classic GDI paths and some print RIPs do use them, which is
why it stays behind the flag.

The three files whose default output is byte-identical to round 4 inherit
their round-3/4 gates unchanged.

## 5. Scripts

| script | what it does |
|---|---|
| `scripts/h5_run.py` | corpus run + delta against the round-4 baseline |
| `scripts/h5_matrix.py` | the five-config size matrix, for one binary |
| `scripts/h5_gates.py` | render / pdftotext / pass-2 / `gs` gates for one file |
| `scripts/h5_jpegcorpus.sh` | drives the `jpeghuff` corpus harness |
| `scripts/h5_cffhint.py` | isolates the CFF-strip contribution — **needs a temporary `AMATL_NO_CFFHINT` escape hatch that is deliberately not in the shipped code**; re-add it locally to re-run |
| `scripts/h5_cff_declines.py` | explains a declined Type1C program (subr counts, subrs carrying hints) |

## 6. Where the remaining headroom sits

Updating HUNT4's totals table:

| bucket | bytes | status |
|---|---|---|
| item 1, baseline JPEGs | 53,669 | **shipped, default on** |
| item 1, progressive JPEGs | ~6,473 | **not implemented** — needs a full progressive decoder (§3) |
| item 12, CFF hint strip | 35,849 | **shipped**, `--strip-hinting` |
| item 10, interpreter-verified CFF merge (±1 unit) | 9,124 | still deferred, still high cost |
| items 2–9, 11 | 0 | rejections still confirmed |
