#!/bin/sh
# hunt5: run the jpeghuff corpus harness over a directory of raw JPEG streams.
# Usage: scripts/h5_jpegcorpus.sh <dir-of-jpegs>
set -e
AMATL_JPEG_DIR="${1:-target/scratch/h5/jpg}"
export AMATL_JPEG_DIR
exec cargo test --release --lib jpeghuff -- --ignored --nocapture
