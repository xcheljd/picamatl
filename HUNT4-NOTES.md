# Compression hunt, round 4 — re-audit of every rejected item

Continues HUNT2-NOTES.md / HUNT3-NOTES.md. This round ships **no size change**:
it re-measures the twelve items previously rejected or deferred and says, with
numbers, which rejections hold. Two of them do not.

All measurements are taken on **amatl's own output** (`target/scratch/h4/`,
regenerated at HEAD; the default numbers reproduce the HUNT3 table exactly),
never on the inputs. Probe scripts are committed as `scripts/h4_*.py`.

## Verdict table

| # | item | original verdict | round-4 measurement | verdict |
|---|---|---|---|---|
| 1 | jpegtran-style DCT cleanup | 242 B | **59,714 B** corpus (adobe 4,972 · irs 54,758), +1,501 on the NASA pair; 0 pixel mismatches over 200 DCT streams | **OVERTURN +59,714** |
| 2 | payload-only dedup variants | 61 B, superseded | equal-payload/equal-dict residue **0 B** in all four files | CONFIRM |
| 3 | content whitespace beyond the minifier | 1,263 B | naive whitespace-run squeeze, z9 vs z9: **2,424 B** corpus (nist 574 · arxiv 86 · irs 716 · adobe 1,048) — and the squeeze is unsafe as written | CONFIRM |
| 4 | XMP pruning | 122 B, superseded | `--strip-metadata` removes every `/Metadata` (adobe 133 packets / 859,984 B); residue beyond it **0** | CONFIRM (redundant) |
| 5 | bitonal re-encode | 0 B | corpus has 5 one-bit images, all in adobe: 4 already CCITT G4, 1 is a 2-byte stream. `--strip-hinting` does not interact | CONFIRM 0 B |
| 6 | ICC profile pruning | 0 B | 5 real ICC profiles corpus-wide (adobe 2,594 · nist 52,535 · arxiv 17,290 raw), all distinct, **0** duplicate bytes; dropping them is a colour-space change, not lossless | CONFIRM 0 B |
| 7 | gray-in-RGB | 0 B | **0** channel-identical DeviceRGB images across all four outputs | CONFIRM 0 B |
| 8 | arxiv image reflate | already optimal | Flate-image reflate headroom (z9 vs stored) **0 B** in all four files | CONFIRM 0 B |
| 9 | CFF bytes-minus-Name-INDEX dedup | irs 8,259 B, "doesn't pay" | post round-3 merge: **0** masked-dup groups, **0** redundant bytes, everywhere. The union merge collected all of it | CONFIRM (now moot) |
| 10 | adobe CFF families needing an outline interpreter | est ~18 KB | real union build: **679 B** strictly lossless, **9,124 B** at ±1 font-unit tolerance. 18 KB was a Σ−max heuristic | number **OVERTURNED down**; deferral CONFIRMED |
| 11 | lossy quality ladder | killed, "zero MAD declines" | mechanism test: worst-case noise q78 MAD **49.6** vs ceiling **96**; q90 only reaches 47.1 | CONFIRM dead (now proven unreachable) |
| 12 | CFF (Type1C) hint strip | not previously measured | **31,574 B** corpus (adobe 18,066 · irs 7,001 · arxiv 6,507 under `--convert-type1`), outline-verified identical | **OVERTURN +31,574** |

## The two overturns

### Item 1 — pass-through JPEGs carry unoptimised Huffman tables (+59,714 B)

`scripts/h4_jpegtran.py`. Running `jpegtran -optimize` over every `DCTDecode`
stream in the default output is **strictly lossless** — DCT coefficients are
untouched, only the Huffman tables are rebuilt — and PIL confirms bit-identical
decoded pixels on all 200 streams. The split:

| file | DCT streams | current | `-optimize -copy all` | save |
|---|---|---|---|---|
| adobe-spec | 128 | 293,272 | 288,300 | 4,972 |
| irs-1040gi | 2 | 1,579,440 | 1,524,682 | **54,758** |
| nasa pair | 140 | 3,052,645 | 3,051,144 | 1,501 |

Marker dropping (`-copy none`) adds only **16 B** on top — this is Huffman
coding, not metadata.

Why round 1 saw 242 B: it measured streams amatl had **re-encoded**, where
mozjpeg already emits optimised tables (hence NASA's near-zero). The headroom
lives entirely in JPEGs amatl **passes through untouched** — above all irs's two
DeviceCMYK / Separation images (1.58 MB, 38% of that file's output), which the
image path skips because they are not RGB or gray. Those two streams alone are
1.3% of the irs output.

Implementable as a Huffman-only re-encoder over every pass-through `DCTDecode`
stream: entropy-decode, rebuild optimal tables, re-emit, keep only if smaller.
No pixel contract to negotiate — it is bit-exact. **Top implementation
candidate of the round, and default-eligible.**

### Item 12 — CFF hint stripping, the Type1C analogue of `--strip-hinting` (+31,574 B)

`scripts/h4_cff_hints_token.py` models what a Rust implementation would do:
decompile each Type2 charstring to its token program, drop
`hstem`/`vstem`/`hstemhm`/`vstemhm`/`hintmask`/`cntrmask` with their operands,
re-fold the leading width operand, recompile. Coordinates stay the original
relative deltas — no outline re-encoding, no subr removal.

| file | Type1C programs | current | hint-stripped z9 | save |
|---|---|---|---|---|
| adobe-spec | 35 | 142,071 | 124,005 | 18,066 |
| irs-1040gi | 17 | 37,781 | 30,780 | 7,001 |
| arxiv (`--convert-type1`) | 22 | 45,036 | 38,529 | 6,507 |

Every program is verified outline- and width-identical after the strip (pen
traces compared glyph by glyph; **0** mismatches on adobe and irs). Two of
arxiv's converted fonts *did* mismatch — they use local subrs whose stack state
my in-place subr rewrite breaks — so they are declined above; a real
implementation would either inline subrs first or decline the same way.

An additional ~1.6 KB (adobe) sits in the Private DICT hint keys
(`BlueValues`, `StemSnap*`, …), and a further ~10 KB on adobe would come from
full subr-free outline re-encoding — both beyond the token-level rewrite and
not counted above.

This belongs behind the existing `--strip-hinting` consent, which already
covers exactly this class of change. Note the current flag is documented as
TrueType-only, so extending it is a widening of its meaning, not a new flag.

## The corrected number for item 10, and a HUNT3 correction

`scripts/h4_cff_semantic.py` builds the actual merged union font (fontTools,
charstrings re-encoded from outlines, subrs dropped) and measures its zlib-9
size against the group's current streams — rather than the Σ−max heuristic that
produced "18 KB".

Comparison is **rotation-normalised**: contours are matched allowing any start
point, which matters because several conflicting glyphs (e.g. MyriadPro-Semibold
`zero`) are the same contour traced from a different origin.

| tolerance | adobe merge save | families merged |
|---|---|---|
| 0 (exact outlines) | **679** | Helvetica-Bold |
| ±1 font unit | **9,124** | + MyriadPro-Bold, MyriadPro-Regular |
| ±20 units | 13,084 | + MinionPro-Regular (visibly different, rejected) |

So the honest answer: an interpreter-verified tier recovers **9,124 B on
adobe — 0.13% of its output** — and only if ±1/1000 em coordinate drift is
accepted. Strictly lossless it is 679 B. Not worth a Type2 interpreter plus a
charstring re-encoder in Rust; the deferral stands, at half the advertised size.

**HUNT3 said "fonttools-probe confirms all adobe conflicts are
outline-identical." That is wrong.** The fragments differ by ±1 unit on real
coordinates (`scripts/h4_cff_probe.py`: MyriadPro-Regular `hyphen` is
`(30,303)→(277,303)` in one fragment and `(30,302)→(277,302)` in another) and,
in a few glyphs, by contour start point. Separate subsetting runs re-rounded the
outlines. An exact-outline interpreter tier would therefore have recovered 679 B,
not 18 KB — the recommendation was sound in direction but off by 26×.

## Item 11 — the ladder is provably unreachable

New test `lossy_quality_ladder_has_no_reachable_trigger` (src/lib.rs). It feeds
`encode_jpeg` the worst input a DCT can get — per-channel independent uniform
noise — and pins two facts:

1. **The guard cannot fire.** q78 on pure noise lands at MAD **49.6**, against
   `DECODE_BACK_MAX_MAD = 96`. No real image gets near it; "zero MAD declines"
   was an accurate observation of an unreachable branch, not blind
   instrumentation.
2. **The ladder's premise would still work.** At a threshold between the two
   measured MADs the test forces the decline and watches q78 fail where q90
   passes — so the mechanism is sound, it simply has no trigger.

The second fact is the one HUNT3 could not show. Worth noting for anyone
tempted to tighten the ceiling: on the pathological case q90 moves MAD only
from 49.6 to 47.1 (−5%), so a higher-quality retry is a weak rescue even where
a decline exists. Every decline seen in practice remains the 5% size guard,
which a higher-quality retry cannot pass by construction.

## Totals

| bucket | bytes | status |
|---|---|---|
| item 1 — Huffman re-optimisation of pass-through JPEGs | **59,714** | lossless, default-eligible |
| item 12 — CFF hint strip | **31,574** | opt-in, under `--strip-hinting` consent |
| item 10 — interpreter-verified CFF merge (±1 unit) | 9,124 | opt-in, high implementation cost |
| items 2–9, 11 | 0 | rejections confirmed |

**Newly-recoverable corpus-wide: ~100,412 B**, of which 59,714 B is strictly
lossless and needs no new consent flag.

## Gates

`cargo test --release` (144 lib + 15 integration, 0 failed), clippy
`-D warnings`, `cargo fmt` — all green at each commit. No `src/` change alters
output: the only source edit this round is a `#[cfg(test)]` test, so the
render-identity / pass-2 / `gs -sDEVICE=nullpage` gates carry over unchanged
from HUNT3 (default outputs are byte-identical to that round's table).
