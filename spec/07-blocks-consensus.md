# 07 - Blocks and chain consensus

Source files: `src/cryptonotecore/Core.cpp` (`addBlock` line 1465,
`validateBlock` line 2699, `getBlockTemplate` line 2313,
`fillBlockTemplate` line 4353, `calculateCumulativeBlocksizeLimit` line
4309), `src/cryptonotecore/Currency.cpp`, `src/cryptonotecore/Difficulty.cpp`,
`src/cryptonotecore/CryptoNoteBasicImpl.cpp` (`getPenalizedAmount`),
`src/cryptonotecore/CachedBlock.cpp`, `src/cryptonotecore/Checkpoints.cpp`,
`src/config/CryptoNoteCheckpoints.h`, `src/cryptonotecore/UpgradeManager.cpp`,
`src/miner/MinerManager.cpp` (`adjustMergeMiningTag`).

Serialization of the block and the exact bytes that are hashed are in
`04-serialization.md`. This document is the rules.

## Block structure

    BlockTemplate
      major_version   uint8
      minor_version   uint8
      timestamp       uint64        (v1 only; for v2+ it lives in the parent block)
      prev_id         hash
      nonce           uint32 raw    (v1 only; for v2+ it lives in the parent block)
      parent_block    ParentBlock   (v2 and later only)
      miner_tx        Transaction   (the coinbase)
      tx_hashes[]     hash          (excluding the coinbase)

    ParentBlock (v2+)
      major_version   uint8         MUST NOT exceed 1 when the block is v2; unchecked otherwise
                                    (0 from daemon templates, 1 from pools, 12 in merge-mined block 600,001)
      minor_version   uint8
      timestamp       uint64
      prev_id         hash          (the same hash as the outer prev_id on this chain)
      nonce           uint32 raw
      merkle root / numberOfTransactions / miner tx branch / minerTx / blockchain branch
                                     (see 04-serialization.md)

For a v2+ block the *block's* `timestamp` and `nonce` fields are
serialized inside the parent block and the outer header carries only the
versions and `prev_id`. In the C++ structures they are the same two fields
(`BlockHeader::timestamp/nonce` are written by the `ParentBlockSerializer`
for v2+). The mainnet blocks in `vectors/` show both layouts.

## Major version by height

`UpgradeManager::getBlockMajorVersion(index)` (`UpgradeManager.cpp:25`):
highest version `V` with `UPGRADE_HEIGHT_V < index`, else 1. A block whose
`major_version` differs from this MUST be rejected (`WRONG_VERSION`). Table
in `01-constants.md`. `minor_version` is not validated; nodes produce 0.

## Genesis

`Currency::generateGenesisBlock` (`Currency.cpp:57`): major 1, minor 0,
timestamp 0, nonce 70, `prev_id` = 32 zero bytes, `miner_tx` parsed from
`GENESIS_COINBASE_TX_HEX` (3 outputs of 500,000,000,000 each to the null
keys, unlock 40, extra = pubkey tag + key), no `tx_hashes`. Its hash is

    877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce

and its full serialization is the first entry of
`vectors/mainnet_rawblocks_0_to_5.json`. A node MUST construct this block
itself; it is never downloaded, and every sparse chain request ends with
its hash.

## Block acceptance (`Core::addBlock`, line 1465), in order

1. **Already known** → `ALREADY_EXISTS` (not an error).
2. **Parent known** in some chain segment, else `REJECTED_AS_ORPHANED`.
3. **Deserialize** every transaction blob; failure → `DESERIALIZATION_FAILED`.
4. **Cumulative size**: `coinbase size + sum(tx sizes) <= maxBlockCumulativeSize(index)`
   where `maxBlockCumulativeSize(h) = 100000 + h · 102400 / 525600`
   (`Currency.cpp:232`; integer division). At height 4.2 M that is about
   918 KB.
5. **`validateBlock`** (below).
6. **Difficulty** for this index from the parent's segment; 0 → reject.
7. **Transaction list consistency** (from index 600,000): `tx_hashes` has no
   duplicates, the supplied transaction blobs hash to no duplicates, every
   blob's hash is in `tx_hashes`, and the two lists are identical in order.
   (Before 600,000 only the count was matched, in the P2P layer.)
8. **Every transaction** passes `ValidateTransaction` at `previousIndex`
   (`06-transactions.md`), accumulating fees. A pooled transaction that
   fails here is evicted from the pool.
9. **Reward**: compute `getBlockReward(majorVersion, medianSize, cumulativeSize,
   alreadyGeneratedCoins(parent), fees, index)` (below) and require the
   coinbase output total from step 5 to equal it exactly.
10. **Checkpoint or proof of work**: if `index <= last checkpoint index`
    the block hash MUST equal the checkpoint at this index if one exists
    (`CHECKPOINT_BLOCK_HASH_MISMATCH`), and no proof of work is checked;
    otherwise `checkProofOfWork(block, difficulty)` MUST pass.
11. **Push** to the parent's segment (main chain, alternative chain, or a
    new fork segment) and possibly **switch** chains (below).

`validateBlock` (line 2699):

- major version matches the height rule;
- v2+: when the block is v2 the parent block major version MUST NOT be
  greater than 1 (`Core.cpp:2714`; 0 passes, and the daemon's own
  templates carry 0 because of the typo at `Core.cpp:2365`); v3+ parent
  versions are unchecked (block 600,001 has 12); the serialized parent
  block MUST be at most 2048 bytes;
- `timestamp <= now + futureTimeLimit(index)` where the limit is 7200 s
  below 100,000, 180 s from 100,000, 360 s from 128,800 (`Currency.h:63`);
  `now` is the validating node's clock;
- if at least `window` previous timestamps exist (`window` = 60 below
  128,800, 11 from 128,800; `Currency.h:51`), `timestamp >= median of the
  last window timestamps`;
- coinbase: exactly one input, of type `BaseInput`, with `height == index`;
  `unlock_time == index + 40`; from 543,000 the coinbase has no signatures;
  every output amount non-zero and a valid key; the sum is the miner reward.

## Reward (`Currency::getBlockReward`, `Currency.cpp:189`)

    baseReward = index >= 1500000 ? 1000000
                                  : (MONEY_SUPPLY − alreadyGeneratedCoins) >> 22
    zone       = fullRewardZone(majorVersion)      // 10000 v1, 20000 v2, 100000 v3+
    median     = max(median of the previous 100 block sizes, zone)
    reject if cumulativeSize > 2 · median
    penalizedBase = penalize(baseReward, median, cumulativeSize)
    penalizedFee  = majorVersion >= 2 ? penalize(fee, median, cumulativeSize) : fee
    reward         = penalizedBase + penalizedFee
    emissionChange = penalizedBase − (fee − penalizedFee)

`penalize(amount, median, size)` (`getPenalizedAmount`, `CryptoNoteBasicImpl.cpp:25`):
`amount` unchanged if `size <= median`; otherwise
`amount · size · (2·median − size) / median²`, computed as a 128-bit product
divided twice (`div128_32`); the result is floor.

`alreadyGeneratedCoins` after a block is the previous value plus
`emissionChange` (stored in `CachedBlockInfo`, `11-storage.md`). The
"previous 100 block sizes" include the genesis block when fewer than 100
exist (`UseGenesis(true)`). Block size here is the cumulative size from
step 4, stored per block.

Live check: block 4,213,000 has reward `1000000`; block 1 has `11563301`
(`09-rpc-and-wallet-sync.md`).

## Block size limits

Two limits exist and both apply:

- `maxBlockCumulativeSize(index)` above (hard cap, grows 100 KiB per year);
- `2 · median` inside `getBlockReward` (the penalty zone ceiling).

Miners use `fillBlockTemplate` (`Core.cpp:4353`):
`maxTotal = min(1.25 · medianSize, maxCumulativeSize) − 600`, where
`medianSize` is `calculateCumulativeBlocksizeLimit(index) / 2`, i.e.
`max(median of last 100, zone)`. So templates never enter the penalty zone.

## Difficulty

`getDifficultyForNextBlock(parentIndex)` (`DatabaseBlockchainCache.cpp:1933`)
collects the last `N` timestamps and cumulative difficulties ending at the
parent (not including genesis; `UseGenesis(false)`), where `N` is
`difficultyBlocksCountByBlockVersion(nextVersion, parentIndex)`:
`61` when `parentIndex >= 100000`, otherwise `window + lag` for the
version (`17 + 0` for v3+, `2880 + 15` for v2 and v1). It then calls
`Currency::getNextDifficulty(nextVersion, parentIndex, timestamps, cumDiffs)`
(`Currency.cpp:540`):

    if parentIndex >= 128800: nextDifficultyV5
    elif parentIndex >= 100000: nextDifficultyV4      // LWMA_2_DIFFICULTY_BLOCK_INDEX_V2
    elif parentIndex >= 100000: nextDifficultyV3      // unreachable, see below
    else: legacy nextDifficulty(version, parentIndex, ...)

The middle two share the constant 100,000, because
`LWMA_2_DIFFICULTY_BLOCK_INDEX_V2` is *defined as*
`LWMA_2_DIFFICULTY_BLOCK_INDEX` (`CryptoNoteConfig.h:76`). The V4 arm
therefore catches every parent index at or above 100,000 and **`V3` is the
dead one**; `nextDifficultyV4` serves 100,000 – 128,799. Getting this
backwards changes the chain: block 100,001 is the first block it affects.

Timestamps and cumulative difficulties are the *oldest first* windows the
cache returns; the LWMA code indexes them as such.

### LWMA-2 (`Difficulty.cpp`)

`nextDifficultyV5` (line 14), used from parent index 128,800:

    T = 60, N = 60
    if timestamps.size() < N + 1: return 10000
    L = 0; sum3 = 0
    for i in 1..N:
        ST = clamp(ts[i] − ts[i−1], −4T, 6T)          // signed
        L += ST · i
        if i > N − 3: sum3 += ST
    nextD = (cumDiff[N] − cumDiff[0]) · T · (N + 1) · 99 / (100 · 2 · L)     // signed 64-bit integer math
    prevD = cumDiff[N] − cumDiff[N − 1]
    nextD = max(prevD · 67 / 100, min(nextD, prevD · 150 / 100))
    if sum3 < 8T / 10: nextD = max(nextD, prevD · 108 / 100)
    return nextD

`nextDifficultyV4` (line 60), used for parent index 100,000 – 128,799:
same shape as V5, with the band `[prevD · 67 / 100, prevD · 150 / 100]`,
the fast-block rule `nextD = max(nextD, prevD · 110 / 100)`, `1000` as the
startup value when `timestamps.size() <= N`, and one thing that must be
copied rather than corrected:

> **The solvetime has no upper bound.** Line 73 reads
> `ST = clamp(-6 * T, ts[i] - ts[i-1], 6 * T)`, which looks like
> `(low, value, high)`. The helper is
> `clamp(n, lower, upper) = max(lower, min(n, upper))` (`Difficulty.h:17`),
> so the arguments land as `n = -6T`, `lower = ST`, `upper = 6T`, and the
> expression collapses to `max(ST, min(-6T, 6T))` = **`max(ST, -6T)`**: a
> lower bound of `-6T` and no upper bound whatsoever. Do not fix it. Block
> 100,001's window contains a 414 second interval, above `6T = 360`;
> clamping it yields 14,866,321 where the chain records 14,767,992, and the
> port forks. Pinned by
> `crates/wrkz-primitives/tests/live_difficulty_switch.rs`.

`nextDifficultyV3` (line 101) is **unreachable** on this chain, as above.
Were it reached it would be the same shape with the solvetime clamp
`[−FTL, 6T]` where `FTL = 180` (a real clamp, written out with
`std::max`/`std::min`), the band `[prevD · 70 / 100, prevD · 107 / 100]`,
the fast-block rule `nextD = prevD · 110 / 100` (assignment, not max), and
`1000` as the startup value when `timestamps.size() <= N`.

All arithmetic is `int64_t` with C division (truncation toward zero); `L`
can be negative in principle, in which case the division result is
negative and then clamped by the band. A port must reproduce signed
truncating division exactly, including the wrap of
`(cumDiff[N] − cumDiff[0]) · T · (N + 1) · 99` — signed overflow is
undefined in C++ but every deployed build wraps, so a port whose integers
trap must wrap explicitly rather than abort.

Two inputs have no defined result in the C++ and therefore no consensus
meaning; a port MUST NOT invent a value for either, and MUST reject the
block instead:

- only `timestamps.size()` is checked before `cumulativeDifficulties[N]`
  and `[N − 1]` are read (`Difficulty.cpp:41-44`). A shorter cumulative
  vector is an out-of-bounds `std::vector::operator[]`.
- `L == 0` — every one of the `N + 1` timestamps in the window equal, so
  every solvetime clamps to 0 — divides by zero and raises `SIGFPE`. The
  daemon dies; no such block can be on the majority chain, because every
  node that saw it would have crashed rather than accepted it.

`crates/wrkz-primitives/src/difficulty.rs` returns `None` for both.

### Legacy algorithm (parent index below 100,000; `Currency::nextDifficulty`, line 564)

The Bytecoin algorithm with the TurtleCoin "zawy" override:

    window = difficultyWindow(version), cut = difficultyCut(version)   // v1,v2: 2880/60; v3+: 17/0
    truncate timestamps and cumDiffs to their FIRST `window` entries
    if length <= 1: return 1
    sort timestamps
    if length <= window − 2·cut: use all; else drop `cut` from each end (cutBegin = (length − (window − 2cut) + 1) / 2)
    timeSpan = ts[cutEnd−1] − ts[cutBegin], at least 1
    totalWork = cumDiff[cutEnd−1] − cumDiff[cutBegin]
    (low, high) = totalWork · 60 as 128-bit; if high != 0 or low + timeSpan − 1 overflows: return 0
    if version >= 3 (ZAWY_DIFFICULTY_DIFFICULTY_BLOCK_VERSION): return low / timeSpan
    if parentIndex >= 20160: recompute with window 17, cut 0 over the *unsorted-tail* last 17 entries
                             (then sorted), nextD = low / timeSpan, floor 100
    return (low + timeSpan − 1) / timeSpan

The truncation is `std::vector::resize(c_difficultyWindow)`
(`Currency.cpp:576-580`), which drops the **tail**, not the head. That is
where `DIFFICULTY_LAG` is applied: the caller passes
`difficultyBlocksCount = window + lag` entries, oldest first, ending at the
parent (`Currency.h:182-184`; lag is 15 for v1/v2 and 0 from v3), and
keeping the first `window` of them discards the `lag` most recent blocks.
A port that keeps the *last* `window` entries silently computes a
different difficulty for every v1 and v2 block.
`crates/wrkz-primitives/src/difficulty.rs` truncates from the front, which
is correct.

Because every block from index 3 is v4 or later, the `version >= 3` branch
is the one that applied for nearly the whole legacy range: difficulty =
`totalWork · 60 / timeSpan` over the last 17 blocks with no cut. The
20,160 override was never reached on this chain. Block 3 (v3, parent v2)
also takes the `version >= 3` branch. Blocks 1 and 2 use the 2880/60/15
parameters with fewer than two entries and return 1 (live headers confirm
difficulty 1 for blocks 0–2 and 60 for block 3).

Cumulative difficulty of a block = parent's + this block's difficulty; it
is what chain selection compares.

## Proof of work check (`Currency::checkProofOfWork`, `Currency.cpp:764`)

- v1: `check_hash(cn_slow_hash_v0(block hashing blob), difficulty)`.
- v2+: `check_hash(pow(parent block hashing blob, header only), difficulty)`
  with `pow` chosen by the *block's* major version; then the merge-mining
  tag MUST be present in the parent coinbase's extra; the blockchain branch
  MUST be at most 256 entries; and `tree_hash_from_branch(blockchainBranch,
  depth, auxHeaderHash, path = genesis hash bytes)` MUST equal the tag's
  `merkle_root`, where `auxHeaderHash` is the block's own header hashing
  blob hashed as an object (`04-serialization.md`). On this chain every
  producer uses depth 0, so the rule reduces to
  `merkle_root == auxHeaderHash`.

`adjustMergeMiningTag` (`MinerManager.cpp:50`) is what a miner must do to a
template before hashing: set depth 0, `merkle_root = auxHeaderHash`,
replace the parent coinbase's extra with just that tag. The daemon's
`getblocktemplate` returns a template with an empty placeholder tag; a
miner that submits it unmodified is rejected with "proof of work too weak"
(`docs/docs/daemon-rpc/json-rpc.md`).

## Checkpoints

`CryptoNoteCheckpoints.h` lists `{index, hash}` pairs every 100 blocks
from 0 to 4,188,000 (about 42,000 entries). `--load-checkpoints <file>`
replaces them with a CSV of `index,hash` lines (`Checkpoints.cpp:45`);
`gen_checkpoints.sh` in `scripts/checkpoints` produces such a file from a
running node.

Semantics (`Core::addBlock` step 10, `ValidateTransaction`):

- "checkpoint zone" = every index up to and including the highest
  checkpointed index;
- inside the zone a block at a checkpointed index MUST match; a block at a
  non-checkpointed index inside the zone is accepted without proof of
  work, without ring signature checks and without transaction proof of
  work checks (its hash is committed by the next checkpoint);
- outside the zone everything is verified.

A port MUST implement the zone semantics to sync the chain in reasonable
time, and MUST keep verifying outside the zone. Shipping a smaller
checkpoint set is safe (more verification); shipping a wrong one is a
permanent fork. The set can be regenerated from any synced node with
`getblockheaderbyheight`.

Dynamic checkpoints (`Core::addDynamicCheckpoint`, `Core.cpp:3865`): when
the same rejected block arrives from 3 or more distinct peers and is at
least a configured depth below the network height, the node adds a
checkpoint for it and accepts it on the next delivery (assumes a local
database fault). This is local recovery behaviour, not consensus, and a
port may omit it.

## Chain segments, alternative chains and reorganisation

The C++ node keeps the main chain in a database-backed segment and every
alternative branch as an in-memory child segment starting at its fork
index (`IBlockchainCache` tree, `Core::addBlock` steps 11 and
`Core::split`). Rules a port must reproduce:

- a block whose parent is the tip of a segment extends it; a block whose
  parent is inside a segment splits the segment at that parent and starts
  a new child;
- a block extends the main chain iff its parent is the main tip;
- after adding to an alternative segment, if that segment's cumulative
  difficulty exceeds the main chain's, the segments swap roles: the
  alternative becomes main, the old main tail becomes alternative, pooled
  transactions are re-checked against the new chain and transactions from
  the discarded blocks are returned to the pool
  (`ADDED_TO_ALTERNATIVE_AND_SWITCHED`);
- alternative chains deeper than 180 blocks behind the main tip, beyond 50
  leaves or beyond 100 alternative blocks in total are pruned
  (`pruneStaleAlternativeChains`); a reorganisation deeper than 180 blocks is
  therefore never followed automatically;
- on a switch, every wallet-facing index (spent key images, output global
  indexes, payment ids, timestamps) is rebuilt for the new tail; the
  storage layout that makes that possible is in `11-storage.md`.

Ties (equal cumulative difficulty) keep the current main chain.

## Block templates (`Core::getBlockTemplate`, line 2313)

Given the miner's address (spend and view public keys) and an optional
extra nonce:

- `height = top + 1`, `difficulty` for it, `major_version` by the height
  rule, `minor_version = 0`;
- v2+: parent block major **0**, minor 0 (`Core.cpp:2364-2365` assigns
  `BLOCK_MINOR_VERSION_0` to `majorVersion` after setting it to 1),
  `transactionCount = 1`, and a default-constructed parent coinbase
  (version 0, no inputs, no outputs) whose extra is an empty merge-mining
  tag (depth 0, zero root) which the miner replaces; a port MUST emit the
  same bytes so its templates are indistinguishable;
- `prev_id = top hash`, `timestamp = now`, but not below the median of the
  last `window` timestamps (`Core.cpp:2402`);
- transactions from the pool per `fillBlockTemplate`;
- the coinbase from `constructMinerTx` (`Currency.cpp:242`): random tx key
  in extra, then the extra nonce if any, one `BaseInput{height}`, outputs
  = the reward decomposed into its non-zero decimal digits, least
  significant first, merged from the top while more than 11 remain
  (`maxOuts = 11`, `Core.cpp:2461`), `unlock_time = height + 40`, each
  output's one-time key derived to the miner's address
  (`03-crypto-primitives.md`); the reward depends on the block size, which
  depends on the coinbase size, so the coinbase is built once with the
  transactions' size, then rebuilt up to 10 times with the cumulative size
  until the coinbase size stops changing (`Core.cpp:2447-2500`).

`getblocktemplate` returns the serialized template, `reserved_offset` (the
byte offset of the extra nonce inside the blob, found by searching for the
tx public key and skipping 32 + 2 bytes; `RpcServer.cpp:1656`), the height
and the difficulty. `submitblock` runs `Core::submitBlock` → `addBlock` and
relays on success.

## Acceptance for this document

1. Genesis reconstructs to `877e55b4…a6ce`.
2. `getBlockReward` reproduces the `reward` field of the headers in
   `09-rpc-and-wallet-sync.md` for blocks 1, 2, 3, 4, 5, 302,401, 600,001,
   1,000,001 and 4,213,000 given the chain state below each (stage 3), or
   at minimum `baseReward` for 1,500,000+ is `1000000` and for block 1 is
   `(5×10¹³ − 1.5×10¹²) >> 22 = 11563301` with no penalty.
3. `nextDifficultyV5` on a synthetic window of 61 equal 60 s solvetimes
   and equal per-block difficulty `D` returns `D · 61 · 99 / (2 · 100 · 1830)`
   rounded per the integer math (`= 0.99 · D · 61 / 61 · ... `; compute and
   compare with a C++ call, `vectors/` harness pattern).
4. Stage 3 replay: every block from 0 to the tip validates, including every
   reorganisation the live network performs while the node runs.
