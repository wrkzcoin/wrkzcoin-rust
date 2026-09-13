# WrkzCoin protocol specification for a clean-room reimplementation

This folder describes the WrkzCoin network precisely enough that a second
implementation, written in another language in a separate repository, can join
the live network and agree with the existing C++ node on every block, every
transaction and every wallet file. It was extracted from the C++ code at one
pinned commit, and every section names the file and line it was taken from.

The reference implementation is
<https://github.com/wrkzcoin/wrkzcoin>. All links and line numbers in this
folder point at commit `8d89d7bf` on the `development` branch:

    https://github.com/wrkzcoin/wrkzcoin/blob/8d89d7bf/<path>#L<line>

When a document cites `src/crypto/hash.h:19` it means that path and line at
that commit. Line numbers drift as the C++ code changes; the commit does not.

## How to read this folder

Work through the documents in order. Each one ends with an acceptance list:
what a new implementation must reproduce before the next document is worth
starting. The order is the order a port should be built in:

| Stage | Documents | What exists at the end |
| --- | --- | --- |
| 1. Hashing and primitives | `02-hashing.md`, `03-crypto-primitives.md`, `04-serialization.md`, `05-addresses-keys-mnemonics.md` | A library that hashes, signs, serializes and derives keys exactly like the C++ code, proven by the vectors in `vectors/` |
| 2. Wallet | `06-transactions.md`, `09-rpc-and-wallet-sync.md`, `10-wallet.md` | A wallet that syncs against an existing daemon, opens existing wallet files, and builds transactions the existing network accepts |
| 3. Daemon | `07-blocks-consensus.md`, `08-p2p-protocol.md`, `11-storage.md` | A node that syncs the real chain from real peers, validates every block, and serves the RPC the wallet needs |

`00-architecture.md` and `01-constants.md` are read first and referred back to
throughout. `12-roadmap.md` turns the stages into a work plan with acceptance
criteria and lists the things that must not be changed.

## Documents

| File | Contents |
| --- | --- |
| [00-architecture.md](00-architecture.md) | What the C++ code base is made of, which parts are consensus, which parts a port should keep in C, and the three data flows every other document refers to |
| [01-constants.md](01-constants.md) | Every network parameter, fork height and protocol constant, with the derived tables (block version by height, mixin ladder, fee ladder, PoW by version) |
| [02-hashing.md](02-hashing.md) | Keccak as CryptoNote uses it, the tree hash, the five proof-of-work functions and their exact parameters, difficulty checking |
| [03-crypto-primitives.md](03-crypto-primitives.md) | Curve operations, key generation, view-from-spend, output derivation, key images, ring signatures, subwallet derivation, payment id encryption, wallet file encryption |
| [04-serialization.md](04-serialization.md) | The binary wire format for transactions and blocks, the block hashing blobs, tx_extra, the KV binary format used by P2P and the database, the JSON conventions |
| [05-addresses-keys-mnemonics.md](05-addresses-keys-mnemonics.md) | Base58 addresses, integrated addresses, the 25 word mnemonic, deterministic subwallets, view-only wallets |
| [06-transactions.md](06-transactions.md) | Transaction rules in validation order with every height gate, fees, mixins, unlock times, fusion transactions, transaction proof of work, mempool policy |
| [07-blocks-consensus.md](07-blocks-consensus.md) | Block structure per version, merge-mining header, timestamps, coinbase and reward, block size, the difficulty algorithms, chain selection and reorganisation, checkpoints, block templates |
| [08-p2p-protocol.md](08-p2p-protocol.md) | Levin framing, handshake and timed sync, peer lists, the block and transaction notifications, the sync state machine, relay rules, peer state file |
| [09-rpc-and-wallet-sync.md](09-rpc-and-wallet-sync.md) | The daemon HTTP and JSON-RPC surface, the exact JSON a wallet and a miner exchange with a daemon, with real samples |
| [10-wallet.md](10-wallet.md) | Wallet file format and JSON schema, the sync algorithm, output ownership, balance rules, transaction construction end to end, the C API and its consumers |
| [11-storage.md](11-storage.md) | The RocksDB key layout and record encodings, so a new node can open an existing database |
| [12-roadmap.md](12-roadmap.md) | The staged plan, the conformance harness, the replay and dual-run tests, and the list of things that look like bugs but are consensus |
| [vectors/](vectors/) | Generated test vectors and the harness sources that produced them from the C++ libraries |

## Ground rules for an implementer

1. **Bit exactness is the goal, not correctness.** Where the C++ code has a
   quirk, the quirk is the protocol. Several are called out explicitly with a
   "do not fix" marker. A port that "fixes" one forks itself off the network.
2. **Never change a rule below the current height.** Every rule in this folder
   is stated with the height range it applies to. A node must re-validate the
   whole existing chain, so historical rules are as binding as current ones.
3. **Prove each stage with vectors before building on it.** The vectors in
   `vectors/` were produced by the C++ libraries. An implementation that does
   not reproduce them byte for byte is wrong, whatever it believes about the
   specification, and the specification is what should be re-read.
4. **Keep the C hashing code unless you can prove a rewrite.** The five
   proof-of-work functions are the highest-risk part of a port. Linking the
   original C files through a foreign function interface is the recommended
   first step; a native rewrite can replace them later, one at a time, against
   the same vectors. See `02-hashing.md`.
5. **Cross-check against a live node early.** The seed nodes answer RPC over
   plain HTTP. `09-rpc-and-wallet-sync.md` shows how to pull any block, and
   `12-roadmap.md` describes the replay test that validates the whole chain.

## Conventions used in this folder

- **Byte order** is little-endian everywhere: integers inside hashes, packed
  structures, nonces, the Levin header, and KV binary values. Curve points and
  scalars are the standard 32-byte little-endian ed25519 encodings.
- **Hex** is lowercase, no prefix. A 32-byte value is 64 hex characters.
- **Height and index.** The code uses `blockIndex` for the zero-based position
  of a block; genesis is index 0. Most RPC fields named `height` are a *count*
  and therefore `index + 1`, except `getblockheaderbyheight` and the wallet
  sync endpoints, which take and return indexes. Each document says which.
  In this folder "height" without qualification means the block index.
- **Amounts** are `uint64` atomic units. The coin has 2 decimal places, so
  100 atomic units display as `1.00 WRKZ`.
- **varint** means the unsigned LEB128 encoding defined in
  `04-serialization.md`, and nothing else.
- **"MUST"** in a rule means consensus: a block or transaction that breaks it
  is rejected by every node. **"SHOULD"** means every existing wallet or node
  does it and peers may depend on it, but nothing rejects a violation.
