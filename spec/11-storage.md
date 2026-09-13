# 11 - Storage: the RocksDB layout

Source files: `src/cryptonotecore/DBUtils.{h,cpp}`, `BlockchainWriteBatch.cpp`,
`BlockchainReadBatch.cpp`, `DatabaseBlockchainCache.{h,cpp}`,
`DatabaseCacheData.{h,cpp}`, `BlockchainCache.h` (record structs),
`RocksDBWrapper.cpp`, `IBlockchainCache.h`. Operator docs: `LITENODE.md`,
`LITESNAPSHOT.md`, `docs/docs/guides/other-tools.md` (export/import).

A port is free to choose its own storage. This document exists so that a
port *can* open an existing C++ database, which is the cheapest way to
validate a consensus implementation (replay from a synced node's data,
compare every derived value) and the only way to migrate a running node
without a resync.

## Engine

RocksDB, one database, the **default column family only** (`RocksDBWrapper.cpp:218`),
`create_if_missing`. Compression is ZSTD on all but the first two levels
when enabled (`RocksDBWrapper.cpp:600-636`); a reader must have ZSTD
support compiled in. Options that affect only performance (buffers, bloom
filters, dictionary) are in `NETWORKING.md` under "Database Settings".

Schema version: key `db_scheme_version` (plain ASCII), value the decimal
string `4` (`DatabaseBlockchainCache.cpp:612-668`). A higher version than
the reader knows is fatal; a lower one is a warning.

## Keys and values are KV binary documents

Every key and every value except the raw block is a complete KV binary
document (`04-serialization.md`) with the header, produced by
`DB::serialize` (`DBUtils.h:62`):

    key   = KVdoc( object named <prefix> { first: <prefix>, second: <key value> } )
    value = KVdoc( element named <prefix> : <value> )

precisely: `serializer(value, name)` with `name = prefix` writes one
top-level entry whose name is the prefix string. For a scalar value that
entry is the scalar (type 5/6/8); for a structure it is an object (type 12)
containing the structure's fields; for a `std::pair` key it is an object
`{ "first": prefix-string, "second": key }`. Composite keys
(`pair<amount, globalIndex>`, `pair<paymentId, count>`) nest: `second` is
itself an object `{ first, second }`.

The prefix is a one-character string; the same prefix names the record
type and appears inside the key. Because the KV encoder omits empty
arrays and empty blobs, a record with an empty vector field simply lacks
that entry. This applies to fields written through `binary()`
(`KVBinaryOutputStreamSerializer.cpp:217`, `if (size > 0)`) and to empty
arrays; a `std::string` field goes through
`operator()(std::string&)` (line 207), which writes the entry
unconditionally, so an empty `std::string` is present with length zero.

| Prefix | Key (`second`) | Value | Written by |
| --- | --- | --- | --- |
| `0` | uint32 block index | `unordered_set<KeyImage>` as an array of 32-byte strings: the key images spent in that block (kept for rewind) | `insertSpentKeyImages` |
| `7` | 32-byte key image | uint32 block index where it was spent | `insertSpentKeyImages` |
| `a` | 32-byte transaction hash | `ExtendedTransactionInfo` (below) | `insertCachedTransaction` |
| `a` | the string `txs_count` | uint64 total transaction count | same |
| `6` | uint32 block index | `CachedBlockInfo` (below) | `insertCachedBlock` |
| `1` | uint32 block index | array of 32-byte strings: the block's transaction hashes, coinbase first | `insertCachedBlock` |
| `5` | 32-byte block hash | uint32 block index | `insertCachedBlock` |
| `8` | the string `last_block_index` | uint32 top block index | `insertCachedBlock`, `insertLastBlockIndex` |
| `4` | uint32 block index | raw block (special encoding, below) | `insertRawBlock` |
| `j` | pair(uint64 amount, uint32 global index) | `KeyOutputInfo` (below): the output at that global index for that amount | `insertKeyOutputInfo` |
| `b` | uint64 amount | uint32 number of outputs of that amount so far | `insertKeyOutputCountForAmount` |
| `h` | the string `key_amounts_count` | uint32 number of distinct amounts | `insertKeyOutputAmounts` |
| `h` | uint32 amount ordinal | uint64 amount (an id → amount table; write-only in practice) | `insertKeyOutputAmounts` |
| `e` | uint64 timestamp | uint32 block index (the first block at or after that timestamp; used by wallet timestamp scans) | `insertClosestTimestampBlockIndex` |
| `g` | uint64 timestamp | array of 32-byte strings: block hashes with that timestamp | `insertTimestamp` |
| `f` | 32-byte payment id | uint32 count of transactions with that id | `insertPaymentId` |
| `f` | pair(payment id, uint32 ordinal) | 32-byte transaction hash | `insertPaymentId` |

Global output indexes are per amount and dense: output `n` of amount `A`
is key `j / (A, n)`, and `b / A` holds the next `n`. Ring members in
transactions are resolved through `j`; this table plus `7` (spent key
images) is exactly what a lite node keeps for the region it has no bodies
for (`LITENODE.md`).

## Record structures

`CachedBlockInfo` (`BlockchainCache.cpp:108`), fields in order:
`block_hash` (32 bytes), `timestamp` uint64, `block_size` uint32,
`cumulative_difficulty` uint64, `already_generated_coins` uint64,
`already_generated_transaction_count` uint64. `block_size` is the
cumulative size used by the reward rule; `already_generated_coins` is the
running emission after this block.

`CachedTransactionInfo` (`BlockchainCache.cpp:86`): `block_index` uint32,
`transaction_index` uint32 (position in the block, coinbase 0),
`transaction_hash`, `unlock_time` uint64, `outputs` (array of output
targets: each an object with the variant tag and key, per
`CryptoNoteSerialization.cpp:336` in KV form), `output_amounts` (array of
uint64), `global_indexes` (array of uint32), `key_inputs` (array of
`KeyInput` objects), `tx_public_key` (note the serialized name differs from
the member name `transactionPublicKey`, `BlockchainCache.cpp:104`),
`payment_id` (a `std::string`, so always present and empty when the
transaction carries no payment id: the empty-blob suppression above applies
to `binary()`, not to `std::string`,
`KVBinaryOutputStreamSerializer.cpp:207-227`). `ExtendedTransactionInfo` wraps it as
`{ cached_transaction: {...}, key_indexes: [{ key: amount, value: [global indexes] }] }`
(`DatabaseCacheData.cpp:14`; maps serialize as arrays of `{key, value}`
objects, `SerializationOverloads.h:163`).

`KeyOutputInfo` (`DatabaseCacheData.cpp:20`): `public_key` (32 bytes),
`transaction_hash`, `unlock_time` uint64, `output_index` uint16,
`block_index` uint32.

`PackedOutIndex` (`IBlockchainCache.h:34`), used in memory and in the
in-memory segment's serialization: one uint64 packing `blockIndex`
(low 32), `transactionIndex` (16), `outputIndex` (16).

## The raw block record (`4`)

Not a KV document. `DB::serialize(RawBlock)` (`DBUtils.cpp:20`) uses the
**binary** serializer's generic container path on the two vectors:

    varint(block.size())  then each block byte as a varint
    varint(tx_count)      then per transaction: varint(size) then each byte as a varint

Every byte `>= 0x80` therefore occupies two bytes. This is the existing
on-disk format and a port that reads a C++ database must decode it this
way; it is unrelated to the `RawBlock` wire encoding of
`04-serialization.md`.

## Unlock semantics used by validation

`isTransactionSpendTimeUnlocked(unlockTime, blockIndex)`
(`DatabaseBlockchainCache.cpp:1696`), reproduced in `06-transactions.md`.
Note the time branch reads the *current tip's* timestamp, not the block
being validated.

## Decoy selection (`getRandomOutsByAmount`, `DatabaseBlockchainCache.cpp:2292`)

Given amount, count, and the block index, choose `count` distinct global
indexes uniformly at random among the outputs of that amount that exist
and are unlocked at that index (`ShuffleGenerator` over
`[0, outputsCountForAmount)`), and return each with its public key from
`j`. No recency weighting. This is what `/getrandom_outs` serves and what
every wallet's rings are drawn from; a port that changes the distribution
changes the anonymity set for everyone and should do so only by agreement.

## What else lives in the database

- Alternative chains are **not** stored; they are in-memory segments and
  are lost on restart (`BlockchainCache`, `07-blocks-consensus.md`).
- The mempool is not stored; it is rebuilt from peers on restart.
- Pruned nodes delete `4` records below a depth; lite nodes never write
  `4`, `1`, `a`, `e`, `g`, `f` below their lite height and instead import
  a snapshot of `6`, `5`, `8`, `j`, `b`, `h`, `7` records
  (`LITESNAPSHOT.md` describes the snapshot file format and the two
  counters that travel in its header).
- `--export-blockchain` writes a dump of raw blocks and
  `--import-blockchain` reads it (`Core::exportBlockchain`, line 2935;
  `importBlockchain`, line 3460; format in `docs/docs/guides/other-tools.md`).
  The dump is the most convenient offline source of every block for the
  replay test in `12-roadmap.md`.

## Bulk load contract

`DatabaseBlockchainCache::beginBulkLoad` / `endBulkLoad` (lines 3522-3597)
batch many blocks per write during import. Any read of a record type
written during the bulk phase must first flush the pending batch; the
C++ code keeps memos for timestamps and payment id counts for that
reason. A port doing its own import needs an equivalent rule.

## Acceptance for this document

1. Open a database written by the C++ node read-only and, for the top
   1000 blocks, reproduce `getblockheaderbyheight` output (hash, height,
   difficulty from cumulative differences, reward from `already_generated_coins`
   deltas plus fees, size, timestamp, nonce, major version) exactly.
2. For 100 random transactions, resolve every ring member through `j` and
   verify the ring signatures (`03-crypto-primitives.md`).
3. Decode 100 random `4` records and check their block ids against `5`.
4. Stage 3 write path: a database written by the port from genesis is
   opened by the C++ node, which then syncs forward from it. This proves
   the layout in both directions and is the migration path for operators.
