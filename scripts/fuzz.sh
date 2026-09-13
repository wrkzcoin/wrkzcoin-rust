#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# Run every fuzz target for a while. Linux only (libFuzzer needs clang and a
# nightly toolchain):
#
#   rustup toolchain install nightly
#   cargo install cargo-fuzz
#   scripts/fuzz.sh [seconds-per-target, default 300]
#
# Seed corpora live in fuzz/corpus/<target>/ and are committed; crashes land
# in fuzz/artifacts/<target>/ and must be turned into regression tests.
set -euo pipefail
cd "$(dirname "$0")/.."
secs="${1:-300}"
for t in $(cargo +nightly fuzz list); do
    echo "== fuzz $t for ${secs}s =="
    cargo +nightly fuzz run "$t" -- -max_total_time="$secs" -max_len=65536
done
echo "FUZZ OK"
