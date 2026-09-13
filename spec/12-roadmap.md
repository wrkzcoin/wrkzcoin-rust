# 12 - Roadmap for the reimplementation

This turns the documents into a sequence of deliverables, each with an
acceptance test that runs against the C++ code or the live network. Work
in this order; each stage is only worth starting after the previous one's
acceptance passes.

## Principles

1. **Vectors before code.** Every stage starts by making the relevant
   vectors in `vectors/` pass, and by extending them where the documents
   say "link the C++ library in a test". The harnesses `vectors/primitives.cpp`
   and `vectors/blocks.cpp` show how to call the C++ libraries from a small
   program; `vectors/README.md` has the build recipe.
2. **The C++ code is the oracle.** When the document and the C++ code
   disagree, the code is right and the document gets fixed. Cite the
   commit (`8d89d7bf`) and the line.
3. **No consensus change lands in the port before it lands in C++**, and
   vice versa. Both implementations must always agree on every block, or
   there are two networks.
4. **Keep the C hashing code** until a native replacement passes the
   vectors, the mainnet blocks, and a long fuzz comparison
   (`02-hashing.md`).

## Stage 1: primitives library

Deliverable: a library crate/package with no network and no storage.

| Component | Document | Acceptance |
| --- | --- | --- |
| varint, base58, KV binary, binary serializer | 04, 05 | vectors; round-trip all mainnet blobs |
| Keccak, tree hash, the five PoW functions (C via FFI), `check_hash` | 02 | vectors; PoW of the 9 mainnet blocks satisfies difficulty |
| curve ops, derivations, key images, ring signatures, plain signatures | 03 | vectors; sign/verify round trips; cross-verify with C++ |
| addresses, integrated addresses, mnemonics, subwallets | 05 | vectors |
| tx/block structures, hashes, hashing blobs, extra parsing | 04 | vectors; block ids of the mainnet blocks |
| wallet file cipher | 03 | vectors |
| constants | 01 | a single constants module mirroring `CryptoNoteConfig.h`, with the derived tables as unit tests |

Also build, as the last item of stage 1, a **conformance harness**: a
small program linked against the C++ static libraries (as the two harness
sources are) that takes random inputs, runs both implementations, and
compares. Keep it in the port's repository and run it in CI against a
pinned build of the C++ libraries. This is what makes later native
rewrites of the hashing code safe.

## Stage 2: wallet library and C API

Deliverable: a wallet library exposing the 57-function C API of
`10-wallet.md`, usable by the existing Flutter apps.

Order inside the stage:

1. daemon client: `/info`, `/getwalletsyncdata`, `/getrawblocks`,
   `/get_global_indexes_for_range`, `/getrandom_outs`,
   `/sendrawtransaction`, `/get_transactions_status` (09);
2. wallet file open/save and the JSON schema (10), tested against files
   the C++ wallet writes;
3. sync: request construction, scanning, fork handling, locked
   transactions (10); test by syncing a view wallet for a known address
   from a known height in both implementations and diffing;
4. transaction construction (10, 06); test with deterministic randomness
   against the C++ validator, then on a private test network, then on
   mainnet with a small amount;
5. the C API surface, then the Flutter apps unmodified.

Acceptance is the list at the end of `10-wallet.md`. The apps are the
first consumers to switch; the C++ wallet library stays available as a
fallback for one release.

## Stage 3: daemon

Deliverable: a node that syncs the real chain from real peers, validates
it, stays at the tip, and serves the RPC of `09-rpc-and-wallet-sync.md`.

Order inside the stage:

1. storage (11): open an existing C++ database read-only and answer
   `getblockheaderbyheight` from it; this validates the key layout
   before any writing happens;
2. block validation offline: replay the chain from a C++ export
   (`--export-blockchain`, `docs/docs/guides/other-tools.md`) or from
   `/getrawblocks` pulls, using the checkpoint zone; then again with
   checkpoints disabled from a chosen height, verifying proof of work and
   signatures (07, 06). The replay test is the single most important test
   of the whole project: it exercises every historical rule with real
   data;
3. P2P (08): handshake, peer lists, chain sync from peers, relay;
4. mempool and block templates (06, 07);
5. RPC (09), then xmrig and the C++ wallets against the port;
6. **dual run**: keep a port node and a C++ node on the same machine for
   several weeks, compare their top hashes every block, alert on any
   divergence. Only after a clean dual run should anyone mine on or point
   a public wallet endpoint at the port.

## Things that look like bugs and are not

These are consensus. A port MUST reproduce them; several are marked in the
documents with "do not fix".

- The fee-per-byte rate switch compares a height to a rate
  (`Utilities.cpp:334`) and therefore applied the V2 rate from height 2
  (06). Blocks 832,000–1,499,999 depend on it.
- The mixin minimum is judged on the largest ring below 4,300,000 (06).
- Block ids are Keccak over a varint-length-prefixed hashing blob; the
  proof-of-work input has no prefix (04).
- The parent block's merkle root is the hash of a coinbase that lives
  only inside the parent block and is never a transaction (04, 07).
- `nextDifficultyV3` is the dead arm and **`nextDifficultyV4` serves
  100,000–128,799**, because `LWMA_2_DIFFICULTY_BLOCK_INDEX_V2` is defined as
  `LWMA_2_DIFFICULTY_BLOCK_INDEX` and the V4 arm is written first (07).
- `nextDifficultyV4` puts no upper bound on a solvetime: its `clamp`
  arguments land in the wrong order, so the expression collapses to
  `max(ST, -6T)` (07).
- The legacy difficulty "zawy" override at 20,160 is unreachable because
  every block from index 3 takes the `version >= 3` branch (07).
- Transaction rules are evaluated at the previous block's index (06).
- Blocks inside the checkpoint zone skip proof of work and signatures (07).
- `blockMedianSize` for the transaction size limit is a node-wide value
  from the main chain, also applied to alternative chains (06).
- Below 543,000 the per-input signature count is not checked (06).
- Coinbase outputs are the reward's decimal digits, least significant
  first, capped at 11 outputs by merging from the top; a one-output
  coinbase today is a consequence of the single-digit fixed reward (07).
- The wallet's `getMinimumTransactionFee` and the daemon's validator
  share the same code path, so the wallet pays the low rate too (06, 10).
- The daemon's block template carries a parent block with major version 0
  and a version-0 coinbase with no inputs (`Core.cpp:2365` typo); pool
  software carries major 1 and a version-1 coinbase; both are accepted and
  both are on chain (04, 07).
- A parent coinbase may be version 2 with an `ignored` varint after
  `extra`, and may carry several merge-mining tags of which the first
  parsed wins (block 600,001) (04).
- Integrated addresses pack the payment id as its ASCII hex string (05).

## Things that must not be copied

- The green-thread dispatcher (`src/system`, `src/platform`).
- Per-call scratchpad allocation in the hashing code (keep a per-thread
  buffer; output is unaffected).
- Dynamic checkpoints added on peer agreement (`Core::addDynamicCheckpoint`),
  unless the port also wants that recovery behaviour.
- The legacy `WalletGreen` wallet and `wrkz-service`, unless old wallet
  files must be opened.

## Open items in the C++ code that a port will meet

Recorded so a port does not "discover" them and change behaviour
unilaterally. Each needs a coordinated fork if it is ever changed:

- decoy selection is uniform over the whole history of a denomination
  (`DatabaseBlockchainCache::getRandomOutsByAmount`); a recency-weighted
  selection would be a wallet-side change with no consensus impact and is
  the highest-value privacy improvement available;
- the mixin fork grace window is computed and discarded
  (`ValidateTransaction.cpp:523`);
- the minimum mixin is 1 while the default is 7 (tier V6); raising the
  minimum requires a census of decoy availability per denomination first;
- `FORK_HEIGHTS` is advisory and the deployed "out of date" warning cannot
  fire for forks at or below 4,500,000.

## Repository layout

This was written as a suggestion before the port existed. The Rust port in
this repository followed it with two changes worth knowing about, since the
rest of this folder refers to crates by name:

    crates/
      wrkz-primitives     stage 1: constants, varint, base58, serialization, tx_extra, difficulty, fees, mixins, kv binary
      wrkz-pow            hashing and proof of work in Rust; the ed25519 curve still calls the C
      wrkz-pow-ref        the reference C of 8d89d7bf, vendored: the curve, and (tests only) the proof-of-work oracle
      wrkz-wallet         stage 2: file format, sync, transfer, daemon client, and the wallet programs
      wrkz-chain          stage 3: validation, reward, checkpoints, chain segments, reorganisation  (was "wrkz-consensus")
      wrkz-storage        RocksDB layout of 11
      wrkz-mempool        the transaction pool and block templates
      wrkz-p2p            Levin, handshake, the wire messages
      wrkz-node           the peer manager and sync state machine; the daemon binary            (was "wrkzd")
      wrkz-rpc            HTTP/JSON-RPC
      wrkz-service        the JSON-RPC wallet service of src/walletservice
      wrkz-txpow-server   the transaction proof-of-work server of src/txpowserver
    apps/
      pluton              the wallet with a window, on wrkz-wallet; its own workspace
    fuzz/
      libFuzzer targets for every parser that reads bytes from a peer, a daemon or a file

Two items of the original suggestion are not crates:

- **`wrkz-wallet-capi`**, the 57-function C API, is still unbuilt (stage 2,
  step 5). It will be a crate here when it lands.
- **`conformance/`** was demoted (the conformance harness at the end of
  stage 1): replaying real blocks against a C++ database (`wrkz-replay`) is a
  stronger oracle than random inputs diffed against the C++ libraries, and the
  dual-run tool lives in `scripts/`.

The boundaries are what matter, because they match the acceptance tests.
