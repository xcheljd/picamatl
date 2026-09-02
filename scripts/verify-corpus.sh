#!/usr/bin/env bash
# Verify every corpus file's SHA-256 against corpus.sha256 (the hashes the
# README benchmarks were measured with). Run from the repo root after
# placing the files (scripts/fetch-corpus.sh handles the direct downloads).

set -euo pipefail
cd "$(dirname "$0")/.."

fail=0; ok=0; missing=0
while IFS=  read -r line; do
    [ -z "$line" ] && continue
    hash=${line%%  *}
    file=${line#*  }
    if [ ! -f "$file" ]; then
        echo "MISSING  $file"
        missing=$((missing+1))
        continue
    fi
    got=$(sha256sum "$file" | cut -d' ' -f1)
    if [ "$got" = "$hash" ]; then
        echo "OK       $file"
    else
        echo "MISMATCH $file (expected ${hash:0:16}..., got ${got:0:16}...)"
        echo "         (source documents are occasionally updated — note this"
        echo "          in docs/CORPUS.md if the hash changed upstream)"
        missing=$((missing+1))
    fi
done < corpus.sha256

echo
if [ "$missing" -gt 0 ]; then
    echo "$missing file(s) differ from the benchmarked versions — README numbers"
    echo "may shift slightly. Re-run scripts/bench-full.sh to measure YOUR copies."
    exit 1
fi
echo "all present files verified."