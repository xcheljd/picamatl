# XREF-WT diagnosis notes (agent run 2026-08-23)

Facts taken as given from orchestrator brief. Plan: walk page 19 resource graph
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
>=65536 wrap silently. amatl bug = trusting upstream width for huge ObjStms.

## FIX (t+12min): cap max_objects_per_stream at 65_535
src/lib.rs pack_and_save(): 100_000_000 -> 65_535. lopdf starts a fresh ObjStm
when the cap is hit, so every type-2 index fits its 2-byte /W field.
Output now has 2 ObjStms; full audit of all 116,800 type-2 entries: 0 mismatches.
GATES (pdftoppm -r 72, cmp/magick AE vs corpus/adobe-spec.pdf):
  page 19: IDENTICAL   page 84: IDENTICAL
  page 752: 195 px diff — byte-for-byte same residual as main-adobe.pdf,
            i.e. caused by another optimizer pass, NOT the xref bug.
Committed on fix/xref-prev. Upstream-worthy: lopdf writer.rs should widen W or
assert index < 65536.
