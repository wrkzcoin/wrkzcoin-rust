# 06 - Transactions: structure, validation, fees, mixins, proof of work, mempool

Source files: `src/cryptonotecore/ValidateTransaction.cpp` (the validator,
read it in full), `src/cryptonotecore/Mixins.h`, `src/utilities/Mixins.cpp`,
`src/utilities/Utilities.cpp` (fees), `src/cryptonotecore/Currency.cpp`
(fusion), `src/cryptonotecore/TransactionPoW.{h,cpp}`,
`src/cryptonotecore/TransactionPool.cpp`, `src/cryptonotecore/Core.cpp`
(`validateTransaction`, `addTransactionToPool`, `isTransactionValidForPool`,
`checkAndRemoveInvalidPoolTransactions`, `fillBlockTemplate`).

The wire encoding of a transaction is in `04-serialization.md`; how a wallet
builds one is in `10-wallet.md`. This document is what the network accepts.

## Structure

    Transaction
      prefix:
        version        uint8      MUST be 1 (CURRENT_TRANSACTION_VERSION)
        unlock_time    uint64     block index or unix time (see "Unlock time")
        vin[]          inputs     each a KeyInput { amount, key_offsets[], k_image } or, only in a coinbase, one BaseInput { height }
        vout[]         outputs    each { amount, KeyOutput { key } }
        extra          bytes      tag-length-value fields, 04-serialization.md
      signatures[][]              one vector per input, one 64-byte signature per ring member

`key_offsets` are **relative**: the first is an absolute global output index
for that amount, each following one is the difference from the previous.
Ring size is `key_offsets.size()`; mixin is ring size − 1. A `version` of
2 exists in the serializer (an extra ignored `uint64`) but is rejected on
input because `CURRENT_TRANSACTION_VERSION` is 1 (`CryptoNoteSerialization.cpp:211`).

## The height a transaction is judged at

`ValidateTransaction` takes a `blockHeight` and a `blockTimestamp`:

- inside a block: `blockHeight` = the index of the **previous** block
  (`Core::addBlock`, `Core.cpp:1586`, passes `previousBlockIndex`) and
  `blockTimestamp` = the timestamp of the block being added;
- at pool admission: `blockHeight` = the current top block index and
  `blockTimestamp` = the top block's timestamp (`Core.cpp:2229-2240`),
  with `isPoolTransaction = true`.

So every "from height H" rule below fires for transactions in block `H + 1`
onward. The checkpoint tests use `blockHeight + 1`, which is the containing
block's own index.

## Validation order (`ValidateTransaction::validate`, line 48)

The checks run in this order and stop at the first failure. Each is
consensus for block transactions; pool-only differences are marked.

### 1. Size (`validateTransactionSize`, line 183)

    serialized size <= blockMedianSize · 2 − 600

`blockMedianSize` is `max(median of the last 100 main-chain block sizes,
granted full reward zone for the next block's version)`
(`Core::updateBlockMedianSize`, `Core.cpp:5097`), recomputed after every
main-chain block. It is a single node-wide value; alternative chains are
validated against the main chain's median. This is the existing behaviour.

### 2. Inputs (`validateTransactionInputs`, line 200)

- at least one input;
- every input MUST be a `KeyInput` (a `BaseInput` here is rejected; coinbase
  transactions never reach this validator);
- no two inputs may share a key image (within the transaction, and against
  the `TransactionValidatorState` of the block or pool being built, which
  catches two transactions in one block spending the same output);
- `key_offsets` non-empty;
- key image in the prime-order subgroup: `l·I == identity`
  (`03-crypto-primitives.md`);
- no `key_offsets[i] == 0` for `i >= 1` (a zero relative offset would be a
  duplicate ring member);
- input amounts sum without overflowing `uint64`.

### 3. Outputs (`validateTransactionOutputs`, line 318)

- every amount non-zero;
- from block index 800,000: every amount `<= 12,500,000,000,000`
  (`MAX_OUTPUT_SIZE_NODE`);
- every target is a `KeyOutput` whose key decompresses (`check_key`);
- output amounts sum without overflow.

### 4. Fee (`validateTransactionFee`, line 391)

`sum(inputs) >= sum(outputs)`, `fee = difference`. Then:

If the transaction is a **fusion** transaction (definition below):

| Block index `H` (previous block) | Required |
| --- | --- |
| 864,864 ≤ H < 1,123,000 | `fee >= 10000` |
| otherwise | any fee, including 0 |

Otherwise (normal transaction), `fee != 0` and, by height:

| Condition on `H` | Minimum fee |
| --- | --- |
| `H >= 832000` | `Utilities::getMinimumTransactionFee(size, H)` (below) |
| `H > 678501` | `50000` |
| `H <= 678501` | `5` |

`getMinimumTransactionFee` (`Utilities.cpp:328`) is:

    rate = 1.953125                     (MINIMUM_FEE_PER_BYTE_V1)
    if H > 1.953125: rate = 0.078125    (MINIMUM_FEE_PER_BYTE_V2)   ← compares a height to a rate
    return getTransactionFee(size, H, rate)

and `getTransactionFee` (`Utilities.cpp:293`):

    if H <= 678501:          return 5
    if H <  832000:          return 50000
    if H <  1500000:         chunks = ceil(size / 256); return floor(chunks · rate · 256)
    else:                    chunks = ceil(size / 128); return floor(chunks · rate · 128)

**Do not fix the comparison on the second line.** Because every height is
greater than 1.953125, the V2 rate (0.078125) has applied since height 2,
so blocks 832,000 through 1,499,999 were validated at `20` atomic per
started 256-byte chunk, not the `500` the constant names suggest. Every
node on the network enforces the low rate; a port that "corrects" it will
reject a large part of the chain and cannot sync. The intended switch at
1,500,000 only changes the chunk size to 128 (`10` atomic per chunk).

The arithmetic is done in `double` and truncated to `uint64`. With these
rates every product is an exact integer, so integer math gives the same
result: fee = `20 · ceil(size/256)` below 1,500,000 and `10 · ceil(size/128)`
from it.

### 5. Extra size (`validateTransactionExtra`, line 477)

For pool transactions always, and for block transactions from index
543,000 + 40 = 543,040: `extra.size() < 1024` (`MAX_EXTRA_SIZE_V2`; note
strict). Below that height the only limit is what fits in a block.

### 6. Unlock time (`validateTransactionUnlockTime`, line 812)

Applies when `H > 1,200,000`:

- if `unlock_time > 500,000,000` it is a unix time and MUST be
  `>= blockTimestamp + 15 · 60`;
- otherwise it is a block index and MUST be `>= H + 15`
  (`MINIMUM_UNLOCK_TIME_BLOCKS`).

A transaction therefore cannot be spent for at least 15 blocks after it is
mined, which is why wallets show new inputs as locked. Wallets set
`unlock_time = networkHeight + 20 + 15` (or `+ 40 + 15` below 1,500,000) so
the transaction stays valid while it waits in the pool (`10-wallet.md`).

### 7. Output count (`validateInputOutputRatio`, line 500)

For pool transactions always, and in blocks from index 777,777:
`outputs.size() <= 90`.

### 8. Mixin (`validateTransactionMixin`, line 518, and `Mixins::validate`)

Take `(min, max)` from the tier in force at `H` (`01-constants.md`, mixin
table; `Utilities::getMixinAllowableRange`, `Mixins.cpp:18`). Over all key
inputs let `largestRing` be the largest `key_offsets.size()` and
`smallestRing` the smallest (clamped to at least 1; if there are no key
inputs both are 1).

    largestMixin  = largestRing − 1
    smallestMixin = (H >= 4300000) ? smallestRing − 1 : largestMixin
    reject if largestMixin  > max
    reject if smallestMixin < min

So below 4,300,000 the *minimum* is judged on the largest ring, which means
one large ring satisfies the floor for every input (this was the original
upstream behaviour and blocks below that height depend on it). From
4,300,000 every input must individually meet the floor.

The pool applies the same tier as the block at `H`; there is no grace
window even though the code computes one and discards it
(`ValidateTransaction.cpp:523-556`). Wallets may build rings of different
sizes per input; nothing forbids it, and current wallets never do.

Current tier (from 4,300,000): min 1, max 7, default 7.

### 9. Transaction proof of work (`validateTransactionPoW`, line 561)

Applies when `H >= 1,123,000`. Skipped for block transactions when
`H + 1` is inside the checkpoint zone (the transaction hash is committed
by the checkpointed block hash); never skipped for the pool.

    hash = cn_upx(serialized TransactionPrefix)          (02-hashing.md)
    difficulty:
      1,123,000 <= H <= 1,200,000:  fusion ? 60000 : 20000
      H > 1,200,000:                fusion ? 320000
                                           : 40000 + (inputs + 4·outputs) · 1000
    pass if check_hash(hash, difficulty)
    else, if H >= 1,500,000 and not fusion and fee >= 10000: pass
    else reject

The prefix that is hashed includes `extra`, and the wallet places the
8-byte nonce field (`0x04` tag + 8 bytes) at the very end of `extra`, so
the nonce is the last 8 bytes of the hashed prefix (`TransactionPoW.h:58`).
The validator does not care where the nonce is; it hashes whatever prefix
it was given. A transaction with a large enough fee needs no nonce at all
from 1,500,000. `TransactionPoW.cpp` shows the search
(`generateTransactionPoWHeight`): threads step the nonce by the thread
count, hashing the same serialized prefix with the tail bytes rewritten.

### 10. Expensive input checks (`validateTransactionInputsExpensive`, line 654)

Skipped entirely when `H + 1` is inside the checkpoint zone. Otherwise, for
each input (in parallel in the C++ code, order does not matter):

- the key image MUST NOT be spent at or below `H`
  (`checkIfSpent(keyImage, H)`);
- convert `key_offsets` to absolute global indexes and look up the output
  public keys for that amount (`11-storage.md`): an unknown index rejects
  (`INPUT_INVALID_GLOBAL_INDEX`); a referenced output that is still locked
  rejects (`INPUT_SPEND_LOCKED_OUT`). Locked means
  `isTransactionSpendTimeUnlocked(unlockTime, H)` is false
  (`DatabaseBlockchainCache.cpp:1696`):

      if unlockTime < 500000000:  unlocked iff H + 1 >= unlockTime          (block index)
      elif H >= 600000:           unlocked iff topBlockTimestamp + 60 >= unlockTime
      else:                       unlocked iff now + 60 >= unlockTime

  where `topBlockTimestamp` is the timestamp of the node's **current main
  chain tip**, not of block `H` (`getLastTimestamps(1)` without an index),
  and `now` is the node's clock. A coinbase output (unlock `index + 40`)
  is therefore spendable in the block at index `index + 39`... precisely,
  in a block whose previous index `H` satisfies `H + 1 >= index + 40`, i.e.
  from block `index + 40` onward;
- for pool transactions always, and in blocks from index 543,000:
  `signatures[i].size() == ring size`;
- `checkRingSignature(prefixHash, keyImage, ringKeys, signatures[i])`
  MUST pass (`03-crypto-primitives.md`).

Below 543,000 a transaction with the wrong number of signatures per input
is accepted as long as the ring signature check over the given signatures
passes; there are such blocks on chain.

## Fusion transactions (`Currency::isFusionTransaction`, `Currency.cpp:353`)

A transaction is a fusion transaction at height `H` iff all of:

- serialized size `<= 30000` bytes;
- at least 12 inputs;
- `inputs >= 4 · outputs`;
- every input amount `>= fusionDustThreshold(H)` (10 below 400,000, then 0,
  so effectively "non-zero");
- let `total = sum(inputs)`, minus `10000` when `864,864 <= H < 1,123,000`;
  decompose `total` into decimal digit amounts (`decompose_amount_into_digits`
  with the dust threshold: each non-zero decimal digit × its power of ten
  becomes one amount, and the low digits whose running total stays at or
  below the threshold are merged into one dust amount — see "Amount
  decomposition" below) and sort ascending; the transaction's output
  amounts, in their on-wire order, MUST equal that sorted list exactly.
  The sort makes the position of the dust amount irrelevant here, but its
  value is not: the fold uses `<=`, not `<`.

That last rule means fusion outputs are always emitted in ascending
"pretty" denominations and a fusion transaction can carry no change and no
payment. Fusion transactions pay no fee (from 1,123,000) and need a
320,000-difficulty proof of work; the pool holds at most 60 of them.

## Amount decomposition and "pretty amounts"

`decompose_amount_into_digits(amount, dustThreshold, chunkHandler, dustHandler)`
(`CryptoNoteFormatUtils.h:74`): walk the decimal digits from least
significant; `digit · 10^k` for each non-zero digit. The exact loop is

    if amount == 0: emit nothing
    dustHandled = false; dust = 0; order = 1
    while amount != 0:
        chunk = (amount % 10) · order;  amount /= 10;  order · = 10
        if dust + chunk <= dustThreshold:            // NOTE: <=, not <
            dust += chunk
        else:
            if not dustHandled and dust != 0: dustHandler(dust); dustHandled = true
            if chunk != 0: chunkHandler(chunk)
    if not dustHandled and dust != 0: dustHandler(dust)

Two details a port must copy exactly. The comparison is `<=`, so a chunk
that brings the running total to exactly `dustThreshold` is still dust.
And the dust is emitted **in place** — immediately before the first chunk
that is not dust — not appended at the end; it only comes last when every
chunk was dust. The C++ header states the shape with its own example:
62,387,455,827 at a dust threshold of 455,827 gives
`455827 + 7000000 + 80000000 + 300000000 + 2000000000 + 60000000000`,
dust first. With threshold 0 (all current heights) nothing is ever dust
and every non-zero digit is its own amount, least significant first.
Coinbase
outputs use the same function: `constructMinerTx` (`Currency.cpp:288`)
decomposes the reward and, while more than `maxOuts` amounts remain, adds
the last (largest) amount into the one before it. The daemon's block
template passes `maxOuts = 11` (`Core.cpp:2461`). Block 1's reward
11563301 therefore has 7 outputs (1, 300, 3000, 60000, 500000, 1000000,
10000000, in that order) and every block from 1,500,000 has exactly one
output, because the fixed reward 1000000 is a single digit. Both are
visible in the mainnet samples.

Wallets only create outputs from `Constants::PRETTY_AMOUNTS`
(`src/config/Constants.h:15`: 1..9 × 10^k for k = 0..18, 171 values). The
daemon does not enforce this for normal transactions; fusion checks do
enforce the decomposition above.

## Coinbase transactions

Validated in `Core::validateBlock`, not here (`07-blocks-consensus.md`):
exactly one `BaseInput` whose `height` equals the block index,
`unlock_time == index + 40`, no signatures (from 543,000), outputs non-zero
with valid keys, and total equal to the computed reward.

## Mempool policy (local, not consensus; `TransactionPool.cpp`)

Admission (`Core::isTransactionValidForPool`, `Core.cpp:2214`): reject if
the pool already holds 60 fee-less transactions and this one is fee-less;
run the validator with `isPoolTransaction = true` at the top index; reject
if any key image is already used by a pooled transaction
(`hasIntersections`); if the pool is over 64 MiB and the candidate is the
least profitable, reject; otherwise insert and evict the least profitable
until the pool is at 90% of the budget (`evictToFitLocked`, line 237).

Priority (`TransactionPriorityComparator`, line 23): higher fee per byte
first (compared as `fee_a · size_b` vs `fee_b · size_a` in 128-bit); then
larger total output amount; then larger inputs/outputs ratio; then smaller
size; then older receive time.

After every main-chain block the pool is re-checked
(`checkAndRemoveInvalidPoolTransactions`): transactions spending a key
image the block spent, and transactions no longer valid for the new height
(`revalidateAfterHeightChange`, `ValidateTransaction.cpp:120`: size, extra,
mixin, inputs, outputs, unlock, fee; proof of work only within 100 blocks of
1,123,000) are removed. Transactions older than 24 h are dropped.

There is **no** separate seven-day lifetime for transactions that came from
an alternative block, although `CRYPTONOTE_MEMPOOL_TX_FROM_ALT_BLOCK_LIVETIME`
(`CryptoNoteConfig.h:329`) suggests one. That constant reaches
`Currency` (`Currency.cpp:870`) and is never read again: the getter
`Currency::mempoolTxFromAltBlockLiveTime()` has no callers anywhere in the
tree. `Core.cpp:289` builds the one `TransactionPoolCleanWrapper` with a
single `mempoolTxLiveTime()` of 24 h, and the cleaner uses that one value
for the age test, the recently-deleted refusal and its expiry
(`TransactionPoolCleaner.cpp:129`, `:170`, `:177`).

Block templates (`Core::fillBlockTemplate`, `Core.cpp:4353`) take pooled
transactions in priority order, fee-paying ones first, then fusion, while
the total stays under `min(1.25 · median, maxCumulativeSize) − 600` bytes
and no two spend the same key image, revalidating each at the template
height and dropping any that fail.

## Reference values

- The transaction in `vectors/mainnet_rawblocks_302401_v5.json` is a
  normal transaction at block 302,401 (v5): 4 inputs at mixin 3, 7 outputs,
  fee 50000... derive the values from the blob as an exercise; the node
  accepted it, so every rule above holds for it at `H = 302,400`.
- Transaction proof of work difficulty for a 2-input, 4-output transaction
  above 1,200,000: `40000 + (2 + 16) · 1000 = 58000`.

## Acceptance for this document

1. A validator that, given the block at index 302,401 and the chain state
   below it, accepts its transaction and rejects it with each field
   mutated (one input duplicated, one signature zeroed, fee reduced, ring
   shrunk to 2 below the tier minimum... the tier at 302,400 is min 3, max 7).
2. Fee vectors: sizes 100, 256, 257, 1000 at `H` = 700000, 900000, 1600000
   give 50000/50000/50000/50000; 20/20/40/80; 10/20/30/80.
3. Fusion detection agrees with the C++ `isFusionTransaction` on a corpus
   of fee-less mainnet transactions (there are many; filter
   `/getrawblocks` output for transactions whose input sum equals output
   sum).
4. Transaction proof of work verifies on every mainnet transaction above
   1,123,000 that pays less than 10000 fee.
5. Stage 3: the full replay (`12-roadmap.md`) accepts every block.
