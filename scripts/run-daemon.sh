#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# Start the WrkzCoin Rust daemon with sensible defaults for a server.
#
#   scripts/run-daemon.sh [extra wrkz-node arguments...]
#
# Defaults, all overridable by environment variable:
#
#   WRKZ_DATA_DIR   ~/.wrkz-rust          chain state, peer file, pid lock
#   WRKZ_RPC_IP     0.0.0.0               ** public **, see the warning below
#   WRKZ_RPC_PORT   17856
#   WRKZ_P2P_PORT   17855
#   WRKZ_LOG_FILE   $WRKZ_DATA_DIR/wrkz-node.log
#   WRKZ_LOG_LEVEL  info
#   WRKZ_TOKEN      (unset)               --rpc-access-token, strongly advised
#                                         when the RPC is public
#   WRKZ_BIN        target/release/wrkz-node
#
# Build first, with RocksDB so the chain survives a restart:
#
#   cargo build --release -p wrkz-node --bin wrkz-node --features rocksdb
#
# See docs/DAEMON.md for the recommended way to bring the state up (importing
# the operator's existing C++ database with wrkz-replay) and for the systemd
# unit.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

DATA_DIR=${WRKZ_DATA_DIR:-$HOME/.wrkz-rust}
RPC_IP=${WRKZ_RPC_IP:-0.0.0.0}
RPC_PORT=${WRKZ_RPC_PORT:-17856}
P2P_PORT=${WRKZ_P2P_PORT:-17855}
LOG_LEVEL=${WRKZ_LOG_LEVEL:-info}
LOG_FILE=${WRKZ_LOG_FILE:-$DATA_DIR/wrkz-node.log}
BIN=${WRKZ_BIN:-$here/target/release/wrkz-node}

if [ ! -x "$BIN" ]; then
  echo "no daemon at $BIN" >&2
  echo "build it first:" >&2
  echo "  cargo build --release -p wrkz-node --bin wrkz-node --features rocksdb" >&2
  exit 2
fi

mkdir -p "$DATA_DIR"

args=(
  --data-dir "$DATA_DIR"
  --p2p-bind-port "$P2P_PORT"
  --rpc-bind-ip "$RPC_IP"
  --rpc-bind-port "$RPC_PORT"
  --log-level "$LOG_LEVEL"
  --log-file "$LOG_FILE"
)

if [ -n "${WRKZ_TOKEN:-}" ]; then
  args+=(--rpc-access-token "$WRKZ_TOKEN")
fi

case "$RPC_IP" in
  127.0.0.1|::1|localhost) ;;
  *)
    echo
    echo "  ================================================================"
    echo "  WARNING: the RPC is bound to $RPC_IP:$RPC_PORT."
    echo "  It is reachable from outside this machine. Anyone who can reach"
    echo "  it can read the chain and submit transactions and blocks."
    if [ -z "${WRKZ_TOKEN:-}" ]; then
      echo "  No access token is set. Set WRKZ_TOKEN, or firewall the port,"
      echo "  or set WRKZ_RPC_IP=127.0.0.1."
    else
      echo "  An access token is set; callers must send X-API-Key."
    fi
    echo "  ================================================================"
    echo
    ;;
esac

echo "data dir : $DATA_DIR"
echo "log file : $LOG_FILE"
echo "p2p      : 0.0.0.0:$P2P_PORT"
echo "rpc      : http://$RPC_IP:$RPC_PORT"
echo "version  : $("$BIN" --version)"
echo
echo "check it with:  curl -s http://127.0.0.1:$RPC_PORT/info"
echo "stop it with:   Ctrl-C, or kill -TERM \$(cat $DATA_DIR/wrkz-node.pid)"
echo

exec "$BIN" "${args[@]}" "$@"
