# Upstream issue draft — lopdf u16 truncation of ObjStm indices

**Status: DRAFT ONLY. Not published.** Target repo: `J-F-Liu/lopdf`
(the crates.io source for `lopdf 0.42.0`, checksum `25aab26d…`; the
crate's own `Cargo.toml` `repository` field is authoritative — confirm it
before filing). Everything below is ready to paste into a new issue.

---

**Title:** Writer silently corrupts the xref stream when an ObjStm packs
more than 65,535 objects (`index_in_stream as u16`)

**Version:** lopdf 0.42.0 (also present in the current `src/writer.rs`
at time of writing)

### Summary

`Document::save_with_options` with `use_object_streams(true)` +
`use_xref_streams(true)` accepts a `max_objects_per_stream` above 65,535,
but the writer records each cross-reference type-2 entry's
object-stream index as a `u16`:

```rust
// src/writer.rs:175
index: index_in_stream as u16,
```

and emits the xref stream with a fixed `/W [1 4 2]` (`src/writer.rs`
~226, 462–463). Any object whose index within its ObjStm is ≥ 65,536
therefore has its index written **mod 2¹⁶**. The resulting xref rows
point at the wrong objects. There is no error, no warning, and no
truncation of the write — the file is produced and `%%EOF`-terminated.

The corruption is also mostly invisible to a naive round-trip: lopdf
itself will re-load the file, because every referenced index is still
*in range* for the stream — it just names a different object. Readers
resolve, say, `/Font` to whatever object happens to live at the wrapped
index.

### Impact / reproduction

Observed while packing the 756-page Adobe PDF 1.7 specification
(≈118k objects) into object streams with a large
`max_objects_per_stream`: the writer produced a single ObjStm holding
~118k objects, and **51,264 of the type-2 xref entries named the wrong
object**. Spot-checked mismatches were offset by a constant slot count
within a run, e.g.

```
obj  97728 -> xref index 31037, ObjStm header pairs[31037] = obj 31052
obj  97729 -> 31038 -> obj 31053
obj 113643 -> 46912 -> obj 46927
```

(31037 = 96573 mod 65536, etc.)

Sketch:

```rust
let mut doc = Document::load("big.pdf")?;      // > 65_535 packable objects
doc.renumber_objects();
let opts = lopdf::SaveOptions::builder()
    .use_object_streams(true)
    .use_xref_streams(true)
    .max_objects_per_stream(100_000_000)       // accepted without complaint
    .build();
doc.save_with_options(&mut out, opts)?;
// decode the /Type/XRef stream: type-2 rows whose index >= 65_536 in the
// unwrapped numbering now disagree with the ObjStm header's (num, offset)
// pair table.
```

### Suggested fixes (any one of these closes the silent-corruption hole)

1. **Widen `/W` automatically.** Compute the largest index actually
   written and size field 3 accordingly (2 bytes when it fits, 3–4
   otherwise). Per the spec `/W` is per-file, so this is a legal and
   fully backward-compatible fix; it also lifts the cap entirely.
2. **Cap internally.** Clamp `max_objects_per_stream` to 65,535 inside
   the writer, starting a new ObjStm at the cap. The writer already
   supports multiple object streams, so this is small — but it silently
   overrides the caller's request.
3. **Fail loudly.** Return an error (or at minimum `debug_assert!`) when
   `index_in_stream > u16::MAX`, instead of a lossy `as` cast.
4. **Document the cap** on `SaveOptions::max_objects_per_stream` in any
   case — right now nothing tells a caller that values above 65,535 are
   unsafe.

Fix 1 (with 3 as a backstop) seems the right shape. The `as u16` cast is
the single load-bearing line; a checked conversion there would have
turned this into a loud failure rather than a corrupt file.

### Workaround for other users

Set `.max_objects_per_stream(65_535)`. lopdf starts a fresh ObjStm when
the cap is reached, so every index stays representable in the 2-byte
field. After applying this, a full audit of all 116,800 type-2 entries in
the file above showed 0 mismatches.
