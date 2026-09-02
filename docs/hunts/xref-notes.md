# XREF-WT diagnosis notes (2026-08-23)

Working notes. Plan: walk page 19 resource graph
in original via pypdf; collect indirect ref numbers; resolve each in
/tmp/xf-adobe2.pdf via RAW decoded xref stream rows + ObjStm header pairs;
look for free/missing/misindexed entries.

## FINDING (t+3min): H2 CONFIRMED — type-2 xref rows point at wrong ObjStm indices
Page-19 reachable graph: 329 indirect refs (root obj 1321). All rows present,
no free entries, all type-1 offsets correct. BUT of 314 type-2 entries
(all inside ObjStm 117996), FIVE have xref N-index != ObjStm-header object number:
  obj 97728 -> xref idx 31037, header pairs[31037]=obj 31052
  obj 97729 -> 31038 -> header=obj 31053
  obj 97730 -> 31039 -> header=obj 31054
  obj 97731 -> 31040 -> header=obj 31055
  obj 113643 -> 46912 -> header=obj 46927
i.e. xref rows are off by exactly 15 slots in these ranges. Next: determine
which side is authoritative (compare object bytes w/ original) + full-file count.

## UPSTREAM SITE (t+7min)
lopdf 0.42.0 src/writer.rs:175 -> `index: index_in_stream as u16` (hardcoded),
written as u16 BE with /W [1 4 2] (writer.rs ~226,462-463). ObjStm indices
>=65536 wrap silently. picamatl bug = trusting upstream width for huge ObjStms.

## FIX (t+12min): cap max_objects_per_stream at 65_535
src/lib.rs pack_and_save(): 100_000_000 -> 65_535. lopdf starts a fresh ObjStm
when the cap is hit, so every type-2 index fits its 2-byte /W field.
Output now has 2 ObjStms; full audit of all 116,800 type-2 entries: 0 mismatches.
GATES (pdftoppm -r 72, cmp/magick AE vs corpus/adobe-spec.pdf):
  page 19: IDENTICAL   page 84: IDENTICAL
  page 752: 195 px diff — byte-for-byte same residual as main-adobe.pdf,
            i.e. caused by another optimizer pass, NOT the xref bug.
Committed on fix/xref-prev. Upstream-worthy: lopdf writer.rs should widen W or
assert index < 65536. Issue drafted (not filed) in the upstream u16-ObjStm issue draft (held privately pending filing).

## RENDER RESIDUAL RESOLVED (follow-up run 2026-08-23)
The 21 differing pages (211 311-315 543-545 548 553 554 644 742-744 746
749-752) had two causes, found by flag/code bisection at 72 dpi:

1. DPI downsampling (19-20 pages) — BY DESIGN. `--target-dpi 0` renders all 21
   pages byte-identical to the original; `--no-downsample-flate-images` alone
   does not, so it is the DCT path resampling over-resolution images.
2. `dedup_decoded_streams` (pages 749 and 752) — A REAL BUG, now fixed. It
   bucketed on the inflated payload ALONE, ignoring the stream dict, so
   same-payload/different-dict images merged and the lowest-id dict won.
   Five bad merges in this file: 65x66 <- 66x65 (transposed), and three pairs
   whose /Indexed palette object differed (68189 vs 68192, 68191 vs 68193,
   68330 vs 68327) — the page 752 overprint figure's overlap lost its dark
   olive blend. Fix: key on (decoded bytes, dict minus /Length). Cost +1,248 B
   on this file, 0 B on the other three corpora.
