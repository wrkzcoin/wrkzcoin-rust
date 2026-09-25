# Lite, pruned and explorer

A full node keeps every block body from genesis and can answer anything; the
lite and pruned modes trade that away for disk, and explorer mode trades RPC
surface for caution. **None of them changes consensus**: a lite or pruned node
runs the same validator over the same records and accepts and rejects exactly
the blocks a full node does. What they give up is the ability to hand back
*bodies* — and therefore to serve old blocks to peers, to answer a wallet rescan
or an explorer lookup below their line, and to reorganise across it.

The C++ counterpart is [Lite Node](https://docs.wrkz.work/guides/lite-node/) on docs.wrkz.work.

The rule everywhere is the same: **never answer a question you cannot answer
correctly**. Below the line the daemon returns an error naming the mode, not an
empty result and not a wrong one.

| | full | `--lite --lite-height H` | `--prune --prune-depth N` |
| --- | --- | --- | --- |
| Consensus validation | full | **full** | **full** |
| Block bodies kept | all | from `H` up (plus genesis) | the last `N` blocks |
| Serves blocks to peers | all | from `H` | the last `N` |
| Wallet sync / `/getrawblocks` | all | clamped up to `H` | clamped up to the floor |
| `f_*` explorer methods | yes | **refused at start-up** | within the window only |
| Reorganisation | to 180 blocks | refused below `H` | to 180 blocks (`N` ≥ 10,080) |
| Reversible | — | **no, permanent** | yes, but deleted bodies do not come back |

## Lite node

```sh
wrkz-node --data-dir DIR --lite --lite-height 4000000
```

Full block data is stored from `--lite-height` upward; below it the state keeps
every consensus record — block infos, spent key images, key outputs, per-amount
counts, the transaction index, the per-block unwind records — and no block or
transaction bytes. That is roughly a third of the disk of a full node, and it is
enough to validate and follow the chain across the whole range.

**The choice is permanent for the database.** The C++ help text says so and it
is not a policy: nothing below the line was ever written, and no later run can
conjure it. The daemon records the height in the state the first time and
refuses every contradicting open afterwards:

- reopening without `--lite` — *"this database was created as a lite node with
  full block data from height H … Pass `--lite --lite-height H`"*;
- reopening at a different height — *"lite mode is permanent for a database …
  Pass `--lite-height H`, not X"*;
- `--lite` on a database that already holds a full chain — *"Start a lite node
  in an empty --data-dir"*.

`--lite` requires `--lite-height`; there is no default, because the value
decides what the node can never serve again. `--lite` and `--prune` cannot be
combined, and neither can `--lite` and `--daemon-mode explorer` — all three
refusals are the C++'s.

Pick `H` well below the network top. The C++ requires at least
`MIN_LITE_FULL_BLOCK_DEPTH` (20,160 blocks, two weeks) of full data above the
lite height so a reorganisation can never reach the index-only region; it checks
this over P2P once four peers have handshaked and exits if the height is too
high. This port makes the same check, weighs the same tallest-claimed height
over the same four reports, prints the C++'s message and exits with status 1.
It applies to an explicit `--lite` only: a body-less import advertises a floor
but never chose a lite height.

A lite node here keeps, below its height, transaction records the C++ drops;
see [Follow-ups](internals.md#follow-ups).

A lite node does not have to sync its index-only region at all: it can start
from a C++ lite node snapshot of it, a 5 GiB file instead of days of sync. See
[Lite node snapshots](lite-snapshots.md).

## Pruned node

```sh
wrkz-node --data-dir DIR --prune --prune-depth 10080
```

Block bodies behind `--prune-depth` blocks of the tip are deleted; everything
consensus reads stays. The default and the minimum are both 10,080 blocks
(7 days at a 60-second target), and a smaller value is **raised** to it with the
C++'s own message rather than refused:

```text
The configured prune depth (500) from CLI is below the enforced minimum
(10080, about 7 days). Using the minimum for network health.
```

That minimum is a network-health floor, far above what safety needs. The floor
that matters for correctness is enforced one layer down, in `wrkz-chain`: a
depth below `CRYPTONOTE_MAX_ALT_BLOCK_DEPTH + 1` = **181** is *refused*, not
clamped, because a reorganisation may reach 180 blocks back and needs the body
of every block it unwinds. The C++ has no such guard — its safety rests entirely
on 10,080 > 4,320 > 180 being true and nobody changing the clamp.

Two things do the pruning:

- every block applied drops the one body that has just left the window, so a
  node that has always been pruned stays exactly at its depth for free;
- a periodic catch-up pass, for a database that was full when `--prune` arrived.
  `--auto-prune-min-gap-blocks` (default 120, `0` disables the schedule) sets how
  often it runs, and `--auto-prune-min-free-bytes` (default 4 GiB) sets the free
  space below which a pass is **forced regardless of the gap** — that is what the
  C++ help text means by "low-space mode can still force prune", and it is the
  opposite of how the compaction knob reads. An unreadable data directory is
  treated as infinite free space, so a node that cannot see its disk never starts
  deleting because of it. Unlike the C++, which rescans from height 0 on every
  pass and records nothing, the pass keeps a resume point, so the steady state
  costs one record read.

Database compaction runs on a schedule of its own, with its own two options, and
its free-space option reads the other way round: low space *prevents* a
compaction. See [Database compaction](database.md#database-compaction).

**Peers.** A pruned node advertises `NODE_CAPABILITY_FLAG_PRUNED` and the floor
it can serve from, computed from the live tip. From the prune-capability fork at
height 4,500,000, a **full** node will not pull the chain from a pruned peer at
all; a peer whose floor is above our height is kept for relay and pool duty and
promoted again once we catch up. `print_cn` shows a `Pruned` column.

## Block explorer

```sh
wrkz-node --data-dir DIR --daemon-mode explorer
```

Adds the five `f_*` JSON-RPC methods — `f_blocks_list_json`, `f_block_json`,
`f_transaction_json`, `f_on_transactions_pool_json` and
`f_transactions_by_payment_id_json` — and `/queryblocksdetailed`. In
`standard` mode they answer **403** with the C++'s message; an unknown method
is a bare 404.

The explorer reads block bodies, so:

- it is **refused at start-up on a lite node**, and on any state whose bodies
  begin above genesis (a `wrkz-replay` import made without `--store-raw`), with
  a message saying which and what to do about it. The C++ refuses `--lite
  --daemon-mode explorer` for the same reason;
- on a **pruned** node it is allowed, with a warning: lookups inside the window
  are correct and complete, and below it return an error. The C++ allows this
  combination too, but reaches an unguarded `std::map::at` and returns
  `500 "Internal server error: map::at"`; this returns the same status with a
  message that names the mode and the height its data starts at.

`f_transactions_by_payment_id_json` is backed by a payment-id index this port
writes as blocks are applied. Only **plaintext long** (32-byte) payment ids are
indexed, as in the C++: an encrypted short id is different bytes in every
transaction, so a 16-character query is refused with a message saying so rather
than answered with an empty list that reads as "never used". A reused id is cut
at `--rpc-max-block-count` with `truncated: true` and a `totalCount` from before
the cut.

Each reuse of an id costs one 70-byte entry and a counter update, however many
times the id was used before (state schema 4). A state written by an older
build (schema 3) keeps its lists as they are and needs **no re-import**: this
build opens it, reads the old list followed by the new entries, and records
schema 4 with the first block it applies. After that an older build refuses
the directory rather than miss the new entries, so upgrade every tool that
opens it (daemon, `wrkz-replay`, `wrkz-verify-state`) together.

The explorer methods themselves are described with the rest of the RPC in
[RPC interfaces](../rpc/index.md).

## Checking what a node is in

`/info` reports `lite`, `lite_start_height`, `pruned`, `prune_depth` and
`prune_capability_active` from the state's own configuration. On the console,
`prune_status` covers all three modes in one place and `db_status` says what the
database holds:

```text
> prune_status
Pruned Node: No
Prune Depth: not pruning; every block body is kept
Lite Node: Yes
Lite Height: 4000000 (permanent for this database)
Serves Block Data From: 4000000
Below that height this node holds every consensus record and no block bodies: it
validates and follows the chain exactly as a full node does, and reports an error
rather than an answer for a block, a wallet scan or an explorer lookup there.
Prune Capability Fork Active: Yes

> db_status
...
Block Bodies: from height 4000000 up (lite, permanent)
```

`status` carries the same rows.
