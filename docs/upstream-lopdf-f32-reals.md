# Upstream issue draft — lopdf stores PDF reals as `f32`, losing digits a viewer can see

**Status: DRAFT ONLY. Not published.** Target repo: `J-F-Liu/lopdf`
(confirm against the crate's own `Cargo.toml` `repository` field before
filing, as with `docs/upstream-lopdf-u16.md`). Observed on `lopdf 0.44.0`,
the version amatl pins.

---

**Title:** `Object::Real(f32)` silently changes the value of a real on
load/save round trip (`841.91998` becomes `841.92`)

### Summary

`Object::Real` holds an `f32` (`src/object.rs:42`) and the writer prints
it with `{}` (`src/writer.rs:594`), i.e. Rust's shortest representation
that round-trips **as `f32`**. A PDF real needing more than ~7
significant digits therefore changes value across a load/save:

| literal in the file | `f32` lopdf keeps | lopdf writes back | drift |
| --- | --- | --- | ---: |
| `841.91998` | 841.9199829101562 | `841.92` | 2.0e-5 |
| `595.91998` | 595.9199829101562 | `595.92` | 2.0e-5 |
| `789.41998` | 789.4199829101562 | `789.42` | 2.0e-5 |
| `333.00781` | 333.0078125 | `333.0078` | 1.0e-5 |
| `666.99219` | 666.9921875 | `666.9922` | 1.0e-5 |
| `152.669983` | 152.66998291015625 | `152.66998` | 3.0e-6 |

Both the shortest `f32` form and the original literal parse to the same
`f32`, so nothing inside lopdf notices. Viewers parse PDF reals as
doubles, so to them the number simply changed.

### Impact / reproduction

The drift is small enough to look like rounding noise and large enough to
change what a page renders as, because `/MediaBox` is the origin of the
page-to-device transform: every coordinate on the page is grid-fit
against it.

Reproduced on a LibreOffice-produced document (`/MediaBox [0 0 595.91998
841.91998]`, A4 — this producer's normal output, so the reach is wide).
Round-tripping it through lopdf changes nothing except that box, and:

```
poppler (pdftoppm -r 100 -gray), page 2 of 28:
  16901 subpixels differ by >= 32 grey levels
  3562 outright black <-> white flips
  max delta 255
```

Whole text lines shift by a pixel where the fitted baseline lands either
side of a pixel centre. It is visually invisible and trivially detectable.

Sketch:

```rust
let doc = Document::load("a4-libreoffice.pdf")?;   // MediaBox 595.91998 x 841.91998
doc.save("out.pdf")?;
// out.pdf now says MediaBox 595.92 x 841.92; render both and diff.
```

Confirmed as the *sole* cause in amatl's corpus by restoring only the
`/MediaBox` literals in the output and re-rendering: 27 of 28 differing
pages and 8 of 10 on a second file both drop to **0**, at 100 and 150
dpi. Across 11 corpus documents / 267 pages, the two files whose
`/MediaBox` literals cannot survive an `f32` round trip are exactly the
two files that render differently. No other object needed touching.

`/Rect` on link annotations and `/W` on CID fonts drift the same way in
the same files; those did not happen to move a pixel here, but `/BBox`,
`/Matrix`, `/Decode` and `/FontMatrix` are all the same shape of value.

Note that Ghostscript does **not** show this: it rounds the page box to
whole device pixels before fitting anything to the grid (it reports a
different raster width for the two files and is otherwise bit-identical).
A round-trip test rendered only through `gs` will report all-clear.

### Suggested fix

**`Object::Real(f64)`.** Every literal in the table above then survives
exactly — parsing `841.91998` as `f64` and printing it back with `{}`
yields `841.91998`, because shortest-round-trip formatting of a `f64`
reproduces any decimal short enough to have been written by hand.
Verified for all seven values above.

This is a breaking change to a public enum, so it wants a major version.
Alternatives that are not really alternatives:

* *Print more digits.* Emitting the `f32`'s true value (`841.9199829101562`)
  cuts the error ~7x but is still not the original number, and it inflates
  every real in every output file — bad for anything size-sensitive.
* *Keep the source literal alongside the value.* Exact, but a much larger
  change to the object model than widening the float.

The information is destroyed at parse time, so there is no fix available
to a downstream caller: by the time `Document::load` returns, the digits
are gone.

### Why amatl cares

amatl's contract is that a lossless run renders identically to its input.
It already defends the *content stream* side of this exact hazard — see
`content_number_values` and `replan_content` in `src/lib.rs`, which splice
the original numeric literals back into re-emitted content streams
specifically because "lopdf holds operands as f32, so its re-emit can
print a shorter decimal that maps to the same f32 but a different f64."
That defence cannot extend to object dictionaries, where amatl never sees
the original bytes.

Until this is fixed upstream, documents whose page boxes carry 8-digit
reals render with sub-pixel differences. `docs/FORMS-PLAN.md` records
which corpus files are affected.
