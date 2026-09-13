#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# Full test pass on the Linux host:
#   1. every vector and live-data test (no network needed except the ignored ones)
#   2. live RPC checks against a seed node (needs outbound 17856)
#   3. live P2P probe (needs outbound 17855)
#   4. RocksDB build, and if WRKZ_DB is set, inspection of that C++ database
#      (spec/11 acceptance 1-3): headers, hashes, PoW, ring signatures.
#   5. If WRKZ_DB is set, an offline replay of that database through the port's
#      own consensus code (spec/12 stage 3 step 2): every block validated and applied to
#      our own state, with the block hash, cumulative difficulty, emission, size
#      and transaction count checked against the C++ records after each one.
#
#      By default this runs the *windowed* mode: the blocks on either side of
#      every height where a consensus rule changes, with checkpoints switched
#      OFF, so the proof of work, the ring signatures, the transaction proof of
#      work and every state rule actually run. A few minutes.
#        WRKZ_REPLAY_WINDOW=500     window size (blocks each side of a height)
#        WRKZ_REPLAY_STATE=DIR      where our state goes
#
#      Two optional heavier passes, neither run by default:
#        WRKZ_REPLAY_SAMPLE=N       also replay N random windows (a seed is
#                                   printed; pass WRKZ_REPLAY_SEED=S to repeat)
#        WRKZ_REPLAY_FULL=1         also do the full linear pass, genesis to the
#                                   tip with checkpoints exactly as the C++ has
#                                   them. Hours against a 40 GB database, but
#                                   resumable: rerun it and it continues from the
#                                   height it reached, so it can go overnight in
#                                   pieces. It is the only pass that proves the
#                                   emission and the cumulative difficulty of the
#                                   whole chain, and it is cheap per block
#                                   because the checkpoint zone skips the work.
#
#      The linear and windowed passes leave incompatible states behind, so they
#      use separate directories and each refuses the other's.
#
#   WRKZ_DB=/path/to/.wrkzcoin/DB scripts/ubuntu-test.sh
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck disable=SC1090
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

echo "== 1. workspace tests =="
cargo test --workspace --release

echo "== 2. live RPC =="
cargo test --release -p wrkz-wallet -- --ignored

echo "== 3. live P2P probe =="
cargo run --release -p wrkz-p2p --bin wrkz-p2p-probe -- node-fin.wrkz.work:17855 5

echo "== 4. rocksdb engine =="
cargo build --release -p wrkz-storage --features rocksdb --bin wrkz-db-inspect
if [ -n "${WRKZ_DB:-}" ]; then
    if [ ! -f "$WRKZ_DB/CURRENT" ]; then
        echo "WRKZ_DB=$WRKZ_DB is not a RocksDB directory (no CURRENT file)."
        echo "Use the daemon's DB directory, by default $HOME/.WRKZCoin/DB, e.g."
        echo "  WRKZ_DB=$HOME/.WRKZCoin/DB scripts/ubuntu-test.sh"
        exit 1
    fi
    # Stop Wrkzd first or point at a copy: RocksDB read-only open still needs
    # the directory to be consistent.
    ./target/release/wrkz-db-inspect "$WRKZ_DB" --count 1000 --rings 100 --quiet
else
    echo "WRKZ_DB not set; skipping database inspection"
fi

echo "== 5. offline replay (chain state and validation) =="
cargo build --release -p wrkz-chain --features rocksdb --bin wrkz-replay
if [ -n "${WRKZ_DB:-}" ]; then
    # State directories of our own, in our own key namespace; the C++ database
    # is only ever opened read-only. Delete one to start that pass over.
    STATE_BASE="${WRKZ_REPLAY_STATE:-$PWD/target/replay-state}"
    WINDOW="${WRKZ_REPLAY_WINDOW:-500}"

    # The default: rule-change windows, checkpoints off, everything verified.
    mkdir -p "$STATE_BASE-forks"
    ./target/release/wrkz-replay --db "$WRKZ_DB" --state "$STATE_BASE-forks" --windows forks --window "$WINDOW" --progress 100

    if [ -n "${WRKZ_REPLAY_SAMPLE:-}" ]; then
        mkdir -p "$STATE_BASE-sample"
        # Not `[ ... ] && SEED_ARG=...`: under `set -e` a false test would end
        # the script.
        SEED_ARG=""
        if [ -n "${WRKZ_REPLAY_SEED:-}" ]; then
            SEED_ARG="--seed $WRKZ_REPLAY_SEED"
        fi
        # shellcheck disable=SC2086
        ./target/release/wrkz-replay --db "$WRKZ_DB" --state "$STATE_BASE-sample" --sample "$WRKZ_REPLAY_SAMPLE" --window "$WINDOW" $SEED_ARG --progress 100
    fi

    if [ -n "${WRKZ_REPLAY_FULL:-}" ]; then
        # The full acceptance run: genesis to the source top, checkpoints as the
        # C++ has them. Hours, and resumable, so it can be rerun until it ends.
        mkdir -p "$STATE_BASE-linear"
        ./target/release/wrkz-replay --db "$WRKZ_DB" --state "$STATE_BASE-linear" --progress 10000
    fi
else
    echo "WRKZ_DB not set; skipping the replay"
fi

echo "== 6. live P2P sync (spec/12 stage 3 step 3) =="
# The stage 3 step 3 acceptance in miniature: connect to the seed nodes, run the
# handshake, peer list, timed sync and the sync state machine, and pull
# WRKZ_SYNC_TO blocks over Levin alone, validating every one through the same
# consensus code the replay uses. Exit 0 means the target height was reached.
#
# Needs outbound TCP to port 17855. The run is bounded twice: --sync-to stops
# the node at the height, and `timeout` stops it if the network does not
# cooperate, so a seed node being down fails the script instead of hanging it.
SYNC_TO="${WRKZ_SYNC_TO:-2000}"
SYNC_DIR="${WRKZ_SYNC_DIR:-/tmp/wrkz-node-test}"
SYNC_TIMEOUT="${WRKZ_SYNC_TIMEOUT:-600}"
cargo build --release -p wrkz-node --bin wrkz-node
rm -rf "$SYNC_DIR"
mkdir -p "$SYNC_DIR"
# --p2p-port 0 takes a free port: this must not collide with a Wrkzd on the
# same host, and the node needs no reachable port to sync (only to be
# back-pinged into other nodes' white lists).
timeout "$SYNC_TIMEOUT" ./target/release/wrkz-node \
    --data-dir "$SYNC_DIR" \
    --p2p-port "${WRKZ_SYNC_PORT:-0}" \
    --sync-to "$SYNC_TO" \
    --exit-when-synced
echo "synced $SYNC_TO blocks over P2P"

echo "ALL OK"
