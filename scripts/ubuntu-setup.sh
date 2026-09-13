#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# One-time setup on Ubuntu 22.04/24.04 for building and testing the port,
# including the RocksDB engine (needs a C++ compiler and libclang for bindgen).
set -euo pipefail

sudo apt-get update
sudo apt-get install -y build-essential clang libclang-dev cmake pkg-config git curl

if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
fi
rustc --version
cargo --version
echo "setup done; run scripts/ubuntu-test.sh"
