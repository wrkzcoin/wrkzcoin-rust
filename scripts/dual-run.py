#!/usr/bin/env python3
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

"""Compare a Rust node against the C++ daemon, block by block, for as long as
you let it run. This is the acceptance tool for stage 3 step 6 of
spec/12-roadmap.md, the last gate before the port serves a public wallet or
accepts mining.

    scripts/dual-run.py --ours http://127.0.0.1:17856 \
                        --reference http://node-fin.wrkz.work:17856

What it does, every --interval seconds:

  1. reads /info from both and records the two heights;
  2. for every block index both nodes now have and this run has not yet
     compared, fetches the header from each and compares the fields that
     consensus fixes: hash, prev_hash, major and minor version, nonce,
     timestamp, difficulty, reward and block size;
  3. prints one line per poll, and on the first mismatch prints both headers
     in full, writes them to the report file and exits non-zero.

A divergence means one of the two nodes would fork off the network. It is
never something to shrug at: capture the block index and both headers, and do
not run the port against anything real until it is explained.

Catching up from a low --from over a long history costs one request per block
per node, so start it near the tip unless you mean to walk the whole chain.
Only the top of the chain can reorganise, so a mismatch within the last few
blocks may resolve itself; --confirmations (default 3) keeps the comparison
that far behind both tips to avoid crying wolf.

Exit codes: 0 stopped cleanly (Ctrl-C or --until reached), 1 divergence
found, 2 a node was unreachable for longer than --tolerate-outage.
"""

import argparse
import json
import sys
import time
import urllib.error
import urllib.request

# Header fields consensus fixes. Anything outside this list (num_txes,
# depth, orphan_status, cumulative difficulty naming) varies between
# implementations without meaning a fork.
COMPARED = [
    "hash",
    "prev_hash",
    "major_version",
    "minor_version",
    "nonce",
    "timestamp",
    "difficulty",
    "reward",
    "block_size",
]


class NodeError(Exception):
    pass


def post_json_rpc(base, method, params, timeout):
    body = json.dumps(
        {"jsonrpc": "2.0", "id": "dual-run", "method": method, "params": params}
    ).encode()
    req = urllib.request.Request(
        base.rstrip("/") + "/json_rpc",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            payload = json.loads(r.read())
    except (urllib.error.URLError, OSError, json.JSONDecodeError) as e:
        raise NodeError(f"{base} {method}: {e}") from e
    if "error" in payload and payload["error"]:
        raise NodeError(f"{base} {method}: {payload['error']}")
    if "result" not in payload:
        raise NodeError(f"{base} {method}: no result in {payload!r}")
    return payload["result"]


def get_json(base, path, timeout):
    try:
        with urllib.request.urlopen(base.rstrip("/") + path, timeout=timeout) as r:
            return json.loads(r.read())
    except (urllib.error.URLError, OSError, json.JSONDecodeError) as e:
        raise NodeError(f"{base}{path}: {e}") from e


def height_of(base, timeout):
    """Block COUNT from /info, so the top block index is this minus one."""
    info = get_json(base, "/info", timeout)
    if "height" not in info:
        raise NodeError(f"{base}/info: no height field")
    return int(info["height"])


def header_at(base, index, timeout):
    result = post_json_rpc(base, "getblockheaderbyheight", {"height": index}, timeout)
    header = result.get("block_header")
    if header is None:
        raise NodeError(f"{base}: getblockheaderbyheight({index}) had no block_header")
    return header


def compare(index, ours, theirs):
    """Return a list of (field, ours, theirs) for every consensus mismatch."""
    bad = []
    for field in COMPARED:
        a, b = ours.get(field), theirs.get(field)
        if a is None and b is None:
            continue
        if a != b:
            bad.append((field, a, b))
    return bad


def report_divergence(index, ours, theirs, mismatches, path):
    lines = [
        "",
        "=" * 72,
        f"DIVERGENCE at block index {index}",
        "=" * 72,
        "",
        "Mismatched fields:",
    ]
    for field, a, b in mismatches:
        lines.append(f"  {field}: ours={a!r} reference={b!r}")
    lines += [
        "",
        "Our header:",
        json.dumps(ours, indent=2, sort_keys=True),
        "",
        "Reference header:",
        json.dumps(theirs, indent=2, sort_keys=True),
        "",
        "One of these two nodes would fork off the network. Do not run the",
        "port against anything real until this is explained.",
        "",
    ]
    text = "\n".join(lines)
    print(text)
    if path:
        try:
            with open(path, "w", encoding="utf-8") as f:
                f.write(text)
            print(f"written to {path}")
        except OSError as e:
            print(f"could not write {path}: {e}", file=sys.stderr)


def main():
    p = argparse.ArgumentParser(
        description="Compare two WrkzCoin daemons block by block.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    p.add_argument("--ours", required=True, help="RPC base URL of the Rust node")
    p.add_argument(
        "--reference", required=True, help="RPC base URL of the C++ daemon"
    )
    p.add_argument(
        "--from",
        dest="start",
        type=int,
        default=None,
        help="first block index to compare (default: start near the common tip)",
    )
    p.add_argument(
        "--until",
        type=int,
        default=None,
        help="stop once this block index has been compared",
    )
    p.add_argument("--interval", type=float, default=30.0, help="seconds between polls")
    p.add_argument(
        "--confirmations",
        type=int,
        default=3,
        help="stay this many blocks behind both tips, since the top can reorganise",
    )
    p.add_argument("--timeout", type=float, default=20.0, help="per-request timeout")
    p.add_argument(
        "--tolerate-outage",
        type=float,
        default=300.0,
        help="seconds a node may stay unreachable before giving up",
    )
    p.add_argument(
        "--report",
        default="dual-run-divergence.txt",
        help="where to write the divergence report (empty to disable)",
    )
    p.add_argument(
        "--max-per-poll",
        type=int,
        default=500,
        help="most blocks to compare in one poll, so catching up stays responsive",
    )
    args = p.parse_args()

    print(f"ours      : {args.ours}")
    print(f"reference : {args.reference}")

    next_index = args.start
    compared = 0
    outage_since = None

    try:
        while True:
            try:
                ours_count = height_of(args.ours, args.timeout)
                ref_count = height_of(args.reference, args.timeout)
                outage_since = None
            except NodeError as e:
                now = time.monotonic()
                if outage_since is None:
                    outage_since = now
                    print(f"unreachable: {e}")
                elif now - outage_since > args.tolerate_outage:
                    print(
                        f"giving up: a node was unreachable for "
                        f"{now - outage_since:.0f}s ({e})",
                        file=sys.stderr,
                    )
                    return 2
                time.sleep(args.interval)
                continue

            # /info reports a count; the top index is one less. Compare only
            # what both nodes have, and stay --confirmations behind.
            safe_top = min(ours_count, ref_count) - 1 - args.confirmations

            if next_index is None:
                next_index = max(0, safe_top)
                print(f"starting at block index {next_index}")

            if safe_top < next_index:
                print(
                    f"[{time.strftime('%H:%M:%S')}] ours {ours_count - 1} "
                    f"reference {ref_count - 1}  compared {compared}  waiting"
                )
                time.sleep(args.interval)
                continue

            last = min(safe_top, next_index + args.max_per_poll - 1)
            for index in range(next_index, last + 1):
                try:
                    ours_h = header_at(args.ours, index, args.timeout)
                    ref_h = header_at(args.reference, index, args.timeout)
                except NodeError as e:
                    print(f"header fetch failed at {index}: {e}")
                    break

                mismatches = compare(index, ours_h, ref_h)
                if mismatches:
                    report_divergence(index, ours_h, ref_h, mismatches, args.report)
                    return 1

                next_index = index + 1
                compared += 1

                if args.until is not None and index >= args.until:
                    print(
                        f"reached --until {args.until}; {compared} blocks agreed"
                    )
                    return 0

            print(
                f"[{time.strftime('%H:%M:%S')}] ours {ours_count - 1} "
                f"reference {ref_count - 1}  agreed through {next_index - 1}  "
                f"total {compared}"
            )
            time.sleep(args.interval)
    except KeyboardInterrupt:
        print(f"\nstopped; {compared} blocks agreed, next would be {next_index}")
        return 0


if __name__ == "__main__":
    sys.exit(main())
