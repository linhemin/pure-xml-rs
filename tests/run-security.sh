#!/bin/sh
# Run the M1 security and defect regressions. No network.
set -eu
cd "$(dirname "$0")/.."
cargo test --workspace --offline --no-fail-fast \
  --test defects_m1 --test writer_tests --test m1 -- --test-threads=8
