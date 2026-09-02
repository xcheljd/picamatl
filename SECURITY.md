# Security policy

## Supported versions

Only the latest tagged release receives security fixes.

## Reporting a vulnerability

Email **xjdom2@gmail.com** with the subject prefix `[picamatl security]`.
Please include a reproducing input (or a minimal PDF) if you can. You will
get an acknowledgment within 72 hours. Do not open a public GitHub issue
for an unpatched vulnerability.

## Security model

Picamatl's pitch is that it is safe to point at untrusted PDFs. The design
that backs that claim:

1. **Fail-safe boundary.** The whole optimization pipeline runs inside a
   `std::panic::catch_unwind`. A panic anywhere — the PDF parser, the JPEG
   decoder, the mozjpeg encoder — becomes an ordinary error, and `optimize`
   returns the input bytes unchanged. Regression tests pin the three
   failure shapes: panic, degenerate input, parse error.

   Known limits, stated plainly: the boundary does not catch a consumer
   build with `panic = "abort"`, and it cannot catch an abort/segfault
   inside the compiled C JPEG codec. Mitigation for the latter is narrow
   attack surface (a JPEG codec is not a PostScript interpreter) plus
   decode-back verification with independent decoders.

2. **Decompression-bomb guards.** Every full-raster decode checks a
   pre-decoded byte ceiling *before* allocating; malformed dimensions or
   overflowing geometry decline rather than clamp.

3. **No network.** `optimize` performs zero I/O beyond the caller's bytes.
   The dependency tree contains no network crates.

4. **Independent verification.** Re-encoded candidates are decoded back and
   compared against the exact pixels submitted, using decoders other than
   the one that produced the candidate.

5. **Conservative decline.** Any parse uncertainty disables the affected
   transformation. PDF/A-declared and encrypted documents are skipped
   outright.

## Known caveats

- Optimizing a digitally signed PDF invalidates the signature (the whole
  document is re-serialized). This is documented, not a bug.
- `optimize` is not sandboxed; it should run in the same trust domain as any
  code that would parse the input anyway. If you need hard process
  isolation, run it in a subprocess/jail of your own.

## Fuzzing

Structural fuzzing targets are on the roadmap (see `docs/ROADMAP.md`); the
current evidence is regression tests for crafted, truncated, and degenerate
inputs. Reports from fuzzers you run yourself are welcome and will be
credited.
