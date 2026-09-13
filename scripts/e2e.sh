#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# End to end on one machine: our daemon, our wallet sync, our transaction build.
#
#   scripts/e2e.sh
#
# What it does, in order:
#
#   1. build the daemon and the wallet tools
#   2. bring a chain state up (see WRKZ_STATE below) and start the daemon
#   3. wait for the RPC to answer
#   4. curl the endpoints an operator checks by hand
#   5. sync a view wallet against our daemon         (needs WRKZ_VIEW_KEY/WRKZ_ADDRESS)
#   6. build a transaction against our daemon, dry run (needs a synced wallet file)
#   7. stop the daemon cleanly and report
#
# Steps 5 and 6 are SKIPPED, with a message, when the keys are not supplied;
# every other step is required and any failure exits non-zero.
#
# Environment:
#
#   WRKZ_DATA_DIR   where the daemon's state lives     (default: a temp dir)
#   WRKZ_STATE      how to get a chain to serve:
#                     import  — the state is already there (see below). Default
#                               when WRKZ_DATA_DIR/state exists.
#                     sync    — sync from the network up to WRKZ_SYNC_TO
#                     empty   — start from genesis and do not sync (offline; the
#                               wallet steps then have nothing to find)
#   WRKZ_SYNC_TO    block index to sync to with WRKZ_STATE=sync (default 2000)
#   WRKZ_RPC_PORT   default 17856
#   WRKZ_P2P_PORT   default 17855
#   WRKZ_FEATURES   cargo features (default "rocksdb"; use "" on a host with no
#                   libclang, which then keeps the chain in memory)
#   WRKZ_VIEW_KEY   64 hex characters, the private view key           (step 5)
#   WRKZ_ADDRESS    the standard address it belongs to                (step 5)
#   WRKZ_SCAN_HEIGHT  where to start scanning                (default 0)
#   WRKZ_SEND_TO    destination address for the dry-run build         (step 6)
#   WRKZ_SEND_AMOUNT  atomic units for the dry-run build     (default 1000)
#   WRKZ_REFERENCE  a C++ daemon to compare shapes against
#                   (default http://node-fin.wrkz.work:17856; set to "" to skip)
#
# Bringing a state up by importing the operator's C++ database (the fast way):
#
#   cargo build --release -p wrkz-chain --bin wrkz-replay --features rocksdb
#   ./target/release/wrkz-replay --db ~/.WRKZCoin/DB --state ~/.wrkz-rust/state
#   WRKZ_DATA_DIR=~/.wrkz-rust WRKZ_STATE=import scripts/e2e.sh
#
# See docs/DAEMON.md.
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$here"

RPC_PORT=${WRKZ_RPC_PORT:-17856}
P2P_PORT=${WRKZ_P2P_PORT:-17855}
SYNC_TO=${WRKZ_SYNC_TO:-2000}
FEATURES=${WRKZ_FEATURES-rocksdb}
REFERENCE=${WRKZ_REFERENCE-http://node-fin.wrkz.work:17856}
SCAN_HEIGHT=${WRKZ_SCAN_HEIGHT:-0}
SEND_AMOUNT=${WRKZ_SEND_AMOUNT:-1000}
RPC=http://127.0.0.1:$RPC_PORT

TMP=$(mktemp -d)
DATA_DIR=${WRKZ_DATA_DIR:-$TMP/data}
WALLET_FILE=$TMP/e2e.wallet
DAEMON_LOG=$TMP/daemon.log
DAEMON_PID=

failures=0
skipped=0

banner() { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }
ok()     { printf '  \033[32mok\033[0m      %s\n' "$*"; }
skip()   { printf '  \033[33mskip\033[0m    %s\n' "$*"; skipped=$((skipped + 1)); }
fail()   { printf '  \033[31mFAILED\033[0m  %s\n' "$*"; failures=$((failures + 1)); }
die()    { printf '\n\033[31m%s\033[0m\n' "$*" >&2; cleanup; exit 1; }

cleanup() {
  if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    banner "stopping the daemon"
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    for _ in $(seq 1 30); do
      kill -0 "$DAEMON_PID" 2>/dev/null || break
      sleep 1
    done
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
      fail "the daemon did not stop on SIGTERM; killing it"
      kill -KILL "$DAEMON_PID" 2>/dev/null || true
    else
      ok "stopped cleanly on SIGTERM"
    fi
    if [ ! -f "$DATA_DIR/wrkz-node.pid" ]; then
      ok "the pid lock was released"
    elif [ "$(uname -o 2>/dev/null)" = Msys ] || [ "$(uname -o 2>/dev/null)" = Cygwin ]; then
      # MSYS `kill -TERM` calls TerminateProcess on a native Windows binary, so
      # no handler and no destructor runs. Not a daemon fault, and not what the
      # Linux host does.
      skip "the pid lock is still there; MSYS kill does not deliver a signal to a native binary"
      rm -f "$DATA_DIR/wrkz-node.pid"
    else
      fail "the pid lock $DATA_DIR/wrkz-node.pid was left behind"
    fi
  fi
  DAEMON_PID=
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
banner "1. build"
# ---------------------------------------------------------------------------
feature_args=()
[ -n "$FEATURES" ] && feature_args=(--features "$FEATURES")
echo "  cargo build --release ${feature_args[*]-}"
cargo build --release -p wrkz-node --bin wrkz-node "${feature_args[@]}" || die "the daemon did not build"
cargo build --release -p wrkz-wallet --bin wrkz-wallet-sync --bin wrkz-wallet-send || die "the wallet tools did not build"
DAEMON=$here/target/release/wrkz-node
ok "$("$DAEMON" --version)"
if [ -z "$FEATURES" ]; then
  echo "  note: built without rocksdb, so the chain state is in memory and is lost on exit"
fi

# ---------------------------------------------------------------------------
banner "2. chain state and daemon"
# ---------------------------------------------------------------------------
mkdir -p "$DATA_DIR"
STATE=${WRKZ_STATE:-}
if [ -z "$STATE" ]; then
  if [ -d "$DATA_DIR/state" ]; then STATE=import; else STATE=sync; fi
fi
echo "  data dir : $DATA_DIR"
echo "  state    : $STATE"

daemon_args=(
  --data-dir "$DATA_DIR"
  --p2p-bind-port "$P2P_PORT"
  --rpc-bind-ip 127.0.0.1
  --rpc-bind-port "$RPC_PORT"
  --log-level info
)
case "$STATE" in
  import) ok "serving the state already in $DATA_DIR/state" ;;
  sync)   echo "  syncing to block index $SYNC_TO from the network" ;;
  empty)  daemon_args+=(--no-listen --no-default-seeds); ok "genesis only, no network" ;;
  *)      die "WRKZ_STATE must be import, sync or empty (got $STATE)" ;;
esac

"$DAEMON" "${daemon_args[@]}" >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!
echo "  daemon pid $DAEMON_PID, log $DAEMON_LOG"

# ---------------------------------------------------------------------------
banner "3. wait for the RPC"
# ---------------------------------------------------------------------------
for i in $(seq 1 60); do
  if curl -sf --max-time 2 "$RPC/getheight" >/dev/null 2>&1; then break; fi
  if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "--- daemon log ---"; cat "$DAEMON_LOG"; die "the daemon exited before the RPC came up"
  fi
  sleep 1
done
curl -sf --max-time 5 "$RPC/getheight" >/dev/null || { echo "--- daemon log ---"; cat "$DAEMON_LOG"; die "the RPC never answered on $RPC"; }
ok "the RPC answers on $RPC"

if [ "$STATE" = sync ]; then
  banner "3b. sync to block index $SYNC_TO"
  for i in $(seq 1 1800); do
    h=$(curl -sf --max-time 5 "$RPC/getheight" | sed -n 's/.*"height":\([0-9]*\).*/\1/p')
    [ -n "$h" ] || h=0
    if [ "$h" -gt "$SYNC_TO" ]; then break; fi
    if [ $((i % 15)) = 0 ]; then echo "  height $h / $SYNC_TO"; fi
    sleep 2
  done
  h=$(curl -sf --max-time 5 "$RPC/getheight" | sed -n 's/.*"height":\([0-9]*\).*/\1/p')
  if [ "${h:-0}" -gt "$SYNC_TO" ]; then ok "synced to height $h"; else fail "only reached height ${h:-0} of $SYNC_TO"; fi
fi

# ---------------------------------------------------------------------------
banner "4. the endpoints an operator checks"
# ---------------------------------------------------------------------------
check() { # name url [post body]
  local name=$1 url=$2 body=${3:-}
  local out
  if [ -n "$body" ]; then
    out=$(curl -sf --max-time 20 -H 'Content-Type: application/json' -d "$body" "$url" 2>/dev/null)
  else
    out=$(curl -sf --max-time 20 "$url" 2>/dev/null)
  fi
  if [ -z "$out" ]; then fail "$name: no answer"; return 1; fi
  case "$out" in
    *'"status":"OK"'*|*'"result"'*) ok "$name  ${out:0:110}" ;;
    *) fail "$name: $out" ;;
  esac
}
check "/info                " "$RPC/info"
check "/getheight           " "$RPC/getheight"
check "/height              " "$RPC/height"
check "/peers               " "$RPC/peers"
check "getblockcount        " "$RPC/json_rpc" '{"jsonrpc":"2.0","id":"e2e","method":"getblockcount"}'
check "getlastblockheader   " "$RPC/json_rpc" '{"jsonrpc":"2.0","id":"e2e","method":"getlastblockheader"}'
check "/getwalletsyncdata   " "$RPC/getwalletsyncdata" '{"blockHashCheckpoints":[],"startHeight":0,"startTimestamp":0,"blockCount":2,"skipCoinbaseTransactions":false}'
check "/getrawblocks        " "$RPC/getrawblocks" '{"blockHashCheckpoints":[],"startHeight":0,"startTimestamp":0,"blockCount":1,"skipCoinbaseTransactions":false}'
check "/get_transactions_status" "$RPC/get_transactions_status" '{"transactionHashes":["0000000000000000000000000000000000000000000000000000000000000001"]}'
check "/getrandom_outs      " "$RPC/getrandom_outs" '{"amounts":[10000],"outs_count":3}'
# `/sendrawtransaction` is the one route behind the sync gate, exactly as the
# C++ has it (`RpcServer.cpp:576`): a node no peer has confirmed us to be at the
# top of answers 503, C++ and Rust alike. Both answers are correct; which one
# comes back says whether this node is synced.
send_out=$(curl -s -w '
%{http_code}' --max-time 20 -H 'Content-Type: application/json'   -d '{"tx_as_hex":"zz"}' "$RPC/sendrawtransaction" 2>/dev/null)
send_code=${send_out##*$'
'}
send_body=${send_out%$'
'*}
case "$send_code:$send_body" in
  200:*'Failed to parse transaction from hex buffer'*)
    ok "/sendrawtransaction     the node is synced and refused the bad hex, as the C++ does" ;;
  503:*'Daemon must be synced'*)
    ok "/sendrawtransaction     503, the sync gate — this node has no confirmed peer yet (the C++ answers the same)" ;;
  *)
    fail "/sendrawtransaction: [$send_code] $send_body" ;;
esac

if [ -n "$REFERENCE" ]; then
  banner "4b. shape comparison against $REFERENCE"
  if cargo run --release -q -p wrkz-rpc --bin wrkz-rpc-diff -- --reference "$REFERENCE" --ours "$RPC"; then
    ok "every probe matched the C++ daemon's shape"
  else
    fail "wrkz-rpc-diff reported differences (see above)"
  fi
fi

# ---------------------------------------------------------------------------
banner "5. wallet sync against our daemon"
# ---------------------------------------------------------------------------
if [ -z "${WRKZ_VIEW_KEY:-}" ] || [ -z "${WRKZ_ADDRESS:-}" ]; then
  skip "set WRKZ_VIEW_KEY and WRKZ_ADDRESS to sync a real wallet against this daemon"
else
  echo "  syncing $WRKZ_ADDRESS from height $SCAN_HEIGHT"
  if ./target/release/wrkz-wallet-sync \
      --daemon "$RPC" \
      --view-key "$WRKZ_VIEW_KEY" \
      --address "$WRKZ_ADDRESS" \
      --scan-height "$SCAN_HEIGHT" \
      --out "$WALLET_FILE" | tee "$TMP/sync-ours.txt"; then
    ok "the wallet synced against our daemon"
  else
    fail "wrkz-wallet-sync failed against our daemon"
  fi

  if [ -n "$REFERENCE" ]; then
    echo "  the same sync against $REFERENCE, for comparison"
    if ./target/release/wrkz-wallet-sync \
        --daemon "$REFERENCE" \
        --view-key "$WRKZ_VIEW_KEY" \
        --address "$WRKZ_ADDRESS" \
        --scan-height "$SCAN_HEIGHT" \
        --quiet > "$TMP/sync-reference.txt"; then
      # Balance and transaction list only; heights differ while ours catches up.
      grep -E 'balance|transactions' "$TMP/sync-ours.txt"      | sort > "$TMP/a.txt" || true
      grep -E 'balance|transactions' "$TMP/sync-reference.txt" | sort > "$TMP/b.txt" || true
      if diff -u "$TMP/b.txt" "$TMP/a.txt" >"$TMP/sync-diff.txt"; then
        ok "our daemon and the C++ daemon report the same wallet"
      else
        echo "  --- reference vs ours ---"; cat "$TMP/sync-diff.txt"
        fail "the two daemons report different wallet contents (expected while ours is behind)"
      fi
    else
      skip "the reference daemon could not be synced against; comparison skipped"
    fi
  fi
fi

# ---------------------------------------------------------------------------
banner "6. transaction build against our daemon (dry run)"
# ---------------------------------------------------------------------------
if [ ! -f "$WALLET_FILE" ]; then
  skip "no synced wallet file (step 5 was skipped), so there is nothing to spend"
elif [ -z "${WRKZ_SEND_TO:-}" ]; then
  skip "set WRKZ_SEND_TO to a destination address to build a transaction"
else
  if ./target/release/wrkz-wallet-send \
      --wallet "$WALLET_FILE" \
      --daemon "$RPC" \
      --to "$WRKZ_SEND_TO" \
      --amount "$SEND_AMOUNT" \
      --dry-run; then
    ok "a transaction was built against our daemon's /getrandom_outs"
  else
    fail "wrkz-wallet-send --dry-run failed against our daemon"
  fi
fi

# ---------------------------------------------------------------------------
cleanup
banner "result"
# ---------------------------------------------------------------------------
echo "  daemon log: $DAEMON_LOG"
tail -5 "$DAEMON_LOG" | sed 's/^/    /'
echo
if [ "$failures" -gt 0 ]; then
  printf '\033[31m%d step(s) failed, %d skipped\033[0m\n' "$failures" "$skipped"
  exit 1
fi
printf '\033[32mall steps passed'
[ "$skipped" -gt 0 ] && printf ' (%d skipped)' "$skipped"
printf '\033[0m\n'
