# Picamatl roadmap

Feature roadmap. Last revised: 2026-08-27.

## Guiding principles

- **Library-first**: picamatl is an embeddable engine with a CLI on top. The
  library API (`optimize_with_options`) is the primary contract.
- **Fail-safe by default**: no output is ever larger or less faithful than the
  input allows; untrusted input declines, never corrupts.
- **No Ghostscript, no network**: pure Rust, local-only, MIT/Apache-2.0.

## Shipped

- v0.4.0 — compression-hunt release: `--figure-dpi` (chart/text-aware DPI),
  `--flatten-forms`, `--strip-private-data`, `--strip-metadata`,
  `--strip-hinting` (TrueType + Type1C), `--collapse-gray-images`,
  `--recompress-bitonal-images`, `--convert-type1`, JPX→DCT, progressive
  JPEG Huffman, Type1C union merge, CMYK/YCCK end-to-end path, f32
  real-literal restoration, decode-ceiling hardening. See
  [CHANGELOG](../CHANGELOG.md).
- v0.4.1 — batch output options: directory input, `--output-dir`,
  `--suffix` (combinable), failure-tolerant batch runs.
- crates.io publication + cargo-dist release pipeline (5 platforms).

## In progress

- Homebrew tap pointing at release artifacts.

## Next

- **Presets**: `--preset web/print/archive` mapping to curated flag sets.
- **Interactive mode**: bare `picamatl` in a PDF-heavy directory suggests a
  batch run with a summary before doing anything.
- **Fuzzing**: cargo-fuzz targets on the parse/re-encode paths.
- **Library guide**: "using picamatl in a Rust service" — error handling,
  memory profile, threading posture, untrusted-input semantics.
- **Corpus growth**: broader third-party document classes in the benchmark
  corpus.

## Considering

- Multi-OS test matrix (macOS/Windows runners for the test suite, not just
  release builds).
- Shell completions (bash/zsh/fish) via clap_complete.
- JSON output mode for scripting (`--json` machine-readable summary).
- A Tauri desktop front-end (drag-and-drop, presets, preview) — separate
  product, would wrap the same library.
- crates.io docs build + docs.rs badge polish.
