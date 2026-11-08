# Fuzzing picamatl

cargo-fuzz targets for the parsers that read untrusted bytes. They run
locally; there is no CI fuzz job.

## Setup

```sh
rustup toolchain install nightly
cargo install cargo-fuzz --locked
```

## Seed, then run

Seeds come from real PDFs (fonts, JPEGs, Flate and content streams are
extracted from each one). Corpora are git-ignored; build them locally:

```sh
cd fuzz
./seed.sh ../fixtures/sample.pdf ../fixtures/forms/*.pdf /path/to/more/*.pdf
cargo +nightly fuzz build -O
MINUTES=90 ./campaign.sh      # every target in parallel; default 90 minutes
```

Or one target at a time:

```sh
cargo +nightly fuzz run -O type1 corpus/type1 -- -max_total_time=600 \
    -rss_limit_mb=2048 -malloc_limit_mb=512 -timeout=10
```

`campaign.sh` runs the built binaries directly in fork mode, so a crash does
not end the run. Findings land in `artifacts/<target>/`; the
`oom/timeout/crash` counters on each target's last line in `logs/` are the
summary. Reproduce a finding with
`cargo +nightly fuzz run -O <target> artifacts/<target>/<file>`.

## Targets

| Target | Code under test | Contract checked |
|---|---|---|
| `optimize` | the whole pipeline, default options | never panics out; output never larger; a rewrite always loads |
| `optimize_all` | the whole pipeline, every opt-in pass on | same |
| `jpeghuff` | lossless JPEG Huffman re-table | no panic, hang or runaway allocation |
| `type1` | Type1 font parse + CFF conversion | same |
| `cff` | CFF hint stripping + two-fragment merge | same |
| `cff_font` | a whole CFF font through hint stripping and self-merge | same |
| `truetype` | sfnt table, cmap, subset-tag and hint-stripping parsers | same |
| `deflate` | capped inflate + PNG predictor decoding | same |
| `content` | content-stream number lexer and re-planner | same |

The focused targets reach internals through the `fuzzing` cargo feature
(`picamatl::fuzz_api`). It is off by default, hidden from the docs, and not a
public API.

`-malloc_limit_mb=512` matters: a single huge allocation is the failure a
desktop never shows (untouched zero pages cost nothing until a memory limit is
in force), and an out-of-memory abort is the one failure picamatl's
`catch_unwind` fail-safe cannot turn into a pass-through.

## Notes

- `campaign.sh` sets `ASAN_OPTIONS=detect_odr_violation=0`: the vendored
  mozjpeg C code defines one static table twice, which AddressSanitizer
  reports at startup before any input runs. `cargo fuzz run` sets the same
  option itself.
- The whole-pipeline targets run at ~25 inputs per second; the focused
  targets at hundreds to tens of thousands. Give the pipeline targets the
  longest runs.
- The sanitizer build is ~20x slower than release, so `campaign.sh` gives the
  whole-pipeline targets a 60 s hang limit (10 s for the rest). A `timeout-*`
  artifact that is just one of your seed PDFs, unchanged, is that slowdown,
  not a hang: check it with the release CLI before triaging.
