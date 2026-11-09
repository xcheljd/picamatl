#!/bin/sh
# Fuzz campaign (default 90 min, override: MINUTES=10 ./campaign.sh): every target in parallel, fork mode so a crash
# does not end the run, single allocations over 512 MB reported as OOM.
cd "$(dirname "$0")"
# mozjpeg-sys (vendored C) defines the static aanscales table twice in
# jcdctmgr.c; ASan flags that ODR quirk at startup before any input runs.
export ASAN_OPTIONS=detect_odr_violation=0
B=target/x86_64-unknown-linux-gnu/release
mkdir -p logs
for t in optimize optimize_all jpeghuff type1 cff cff_font truetype deflate content lzw; do
    mkdir -p "artifacts/$t"
    # Whole-pipeline targets get a longer hang limit: the sanitizer build runs
    # ~20x slower than release (a 0.9 s corpus PDF takes ~17 s), so 10 s
    # would flag ordinary seeds as hangs.
    case $t in optimize*) ML=262144 TO=60 ;; *) ML=300000 TO=10 ;; esac
    # libFuzzer saves every new-coverage input back into corpus/<t>, so the
    # corpus grows run after run. Before fuzzing, shrink each corpus to the
    # inputs that add coverage (-merge=1); for the pipeline targets this is
    # what keeps the startup pass from eating the campaign.
    if [ "$(ls "corpus/$t" | wc -l)" -gt 100 ]; then
        rm -rf "corpus/$t.min" && mkdir -p "corpus/$t.min"
        if "$B/$t" -merge=1 -max_len=$ML -timeout=$TO -rss_limit_mb=2048 \
            -malloc_limit_mb=512 "corpus/$t.min" "corpus/$t" > "logs/$t.merge.log" 2>&1; then
            rm -rf "corpus/$t" && mv "corpus/$t.min" "corpus/$t"
        else
            rm -rf "corpus/$t.min"
        fi
    fi
    "$B/$t" "corpus/$t" -artifact_prefix="artifacts/$t/" -fork=1 \
        -ignore_crashes=1 -ignore_timeouts=1 -ignore_ooms=1 \
        -max_total_time=$((${MINUTES:-90} * 60)) -rss_limit_mb=2048 -malloc_limit_mb=512 \
        -timeout=$TO -max_len=$ML -print_final_stats=1 > "logs/$t.log" 2>&1 &
done
wait
echo "CAMPAIGN DONE"
status=0
for t in optimize optimize_all jpeghuff type1 cff cff_font truetype deflate content lzw; do
    runs=$(grep -aoE '^#[0-9]+' "logs/$t.log" | tail -1)
    counters=$(grep -aoE 'oom/timeout/crash: [0-9]+/[0-9]+/[0-9]+' "logs/$t.log" | tail -1)
    if [ -z "$runs" ]; then
        # A target that never executed an input proves nothing; say so.
        echo "$t: DID NOT RUN -- $(grep -a 'ERROR' "logs/$t.log" | head -1)"
        status=1
    else
        # crash-/oom-/timeout-/leak- files are findings; slow-unit- files are
        # libFuzzer's note of the slowest input, informational only.
        findings=$(ls "artifacts/$t" | grep -cvE '^slow-unit-' || true)
        slow=$(ls "artifacts/$t" | grep -cE '^slow-unit-' || true)
        echo "$t: runs=${runs#\#} ${counters:-oom/timeout/crash: 0/0/0} findings=$findings slow-units=$slow"
        [ "$findings" -eq 0 ] || status=1
    fi
done
exit $status
