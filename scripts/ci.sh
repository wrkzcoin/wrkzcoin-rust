#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# Continuous-integration pass. Run on every push (GitHub Actions,
# .github/workflows/ci.yml) or by hand on the Ubuntu host:
#
#   scripts/ci.sh                        # every section below
#   scripts/ci.sh clippy tests           # only the sections named
#   WRKZ_DB=/path/DB scripts/ci.sh       # plus the RocksDB inspection
#   WRKZ_CI_DEBUG_TESTS=1 scripts/ci.sh  # plus a debug-profile test run
#
# The sections, in the order they run: fmt, clippy, tests, docs, pluton (Rust
# Pluton Wallet) and rocksdb (the RocksDB engine and the programs that need
# it). The workflow runs them as separate jobs, so that a failure names itself
# and one section's system packages are not every section's.
#
# Fails on the first problem. Network tests (`--ignored`) and the P2P probe
# are NOT part of CI; scripts/ubuntu-test.sh runs those. Neither is the fuzz
# smoke run or the dependency audit, which the workflow runs as their own jobs
# because each needs a toolchain this script does not assume.
#
# Everything here passes `--locked`: CI proving a build that quietly resolved
# newer dependencies than Cargo.lock names is CI proving the wrong build.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck disable=SC1090
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

all="fmt clippy tests docs pluton rocksdb"
named=$#
sections=${*:-$all}
for s in $sections; do
    case " $all " in
        *" $s "*) ;;
        *)
            echo "ci.sh: no section '$s' (the sections are: $all)" >&2
            exit 2
            ;;
    esac
done
want() {
    case " $sections " in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
    esac
}

if want fmt; then
    echo "== fmt =="
    cargo fmt --all -- --check
fi

if want clippy; then
    echo "== clippy (warnings are errors) =="
    cargo clippy --workspace --all-targets --locked -- -D warnings
fi

if want tests; then
    echo "== tests =="
    cargo test --workspace --release --locked

    # The release profile has `debug-assertions = false` and `overflow-checks =
    # false`, so a release-only run never executes a `debug_assert!` and never
    # traps an arithmetic overflow. Both matter here — the difficulty and reward
    # arithmetic is deliberately wrapping in some places and deliberately checked
    # in others — so the debug profile is a separate pass. It is off by default
    # because it rebuilds the world; the workflow runs it as its own job.
    if [ -n "${WRKZ_CI_DEBUG_TESTS:-}" ]; then
        echo "== tests (debug profile: debug_assert! and overflow checks) =="
        cargo test --workspace --locked
    fi
fi

if want docs; then
    echo "== docs =="
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
fi

# Rust Pluton Wallet is its own workspace (it pulls in a GUI toolkit), so
# `--workspace` above never reaches it. Without this it is the one part of the
# tree nothing lints and nothing tests.
if want pluton; then
    echo "== rust pluton wallet =="
    (
        cd apps/pluton
        cargo clippy --all-targets --locked -- -D warnings
        cargo test --locked
    )
fi

# The RocksDB engine and the four binaries that need it never compile in the
# default build, because the bindings need libclang. Where it is present, they
# are built and tested here; where it is not, that is said out loud rather
# than passing silently, and when the section was asked for by name (as the
# workflow's `rocksdb` job does) it is a failure.
if want rocksdb; then
    echo "== rocksdb engine =="
    if [ -n "${WRKZ_SKIP_ROCKSDB:-}" ]; then
        echo "   skipped (WRKZ_SKIP_ROCKSDB is set)"
    elif command -v llvm-config >/dev/null 2>&1 || [ -n "${LIBCLANG_PATH:-}" ] || ls /usr/lib/llvm-*/lib/libclang.so >/dev/null 2>&1; then
        cargo clippy --workspace --all-targets --locked --features rocksdb -- -D warnings
        cargo test -p wrkz-storage -p wrkz-chain -p wrkz-rpc -p wrkz-node --release --locked --features rocksdb
    else
        echo "   NOT BUILT: no libclang on this host (apt install libclang-dev), so the"
        echo "   RocksDB engine, wrkz-replay, wrkz-verify-state and wrkz-db-inspect are"
        echo "   not compiled by this run. Set WRKZ_SKIP_ROCKSDB=1 to make that deliberate."
        [ "$named" -eq 0 ] || exit 1
    fi
fi

if [ -n "${WRKZ_DB:-}" ]; then
    echo "== rocksdb inspection =="
    if [ ! -f "$WRKZ_DB/CURRENT" ]; then
        echo "WRKZ_DB=$WRKZ_DB is not a RocksDB directory (no CURRENT file)"
        exit 1
    fi
    cargo build --release -p wrkz-storage --features rocksdb --bin wrkz-db-inspect
    ./target/release/wrkz-db-inspect "$WRKZ_DB" --count 1000 --rings 100 --quiet
fi
echo "CI OK"
