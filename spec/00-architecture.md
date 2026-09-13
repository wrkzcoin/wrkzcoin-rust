# 00 - Architecture of the reference implementation

This document maps the C++ code base so the other documents can be located in
it, separates consensus code from everything else, and describes the three
data flows a port has to reproduce.

## Lineage

WrkzCoin is a classic CryptoNote chain in the TurtleCoin lineage. Amounts are
plaintext, outputs are one-time keys, inputs are ring signatures over
same-denomination decoys, and there is no RingCT, no commitments and no range
proofs. Anything written about Monero after 2017 (RingCT, Bulletproofs, CLSAG,
subaddresses, view tags) does not apply. Anything written about Bytecoin,
TurtleCoin or Monero's pre-RingCT era largely does, with the deviations this
folder lists.

The chain launched 2018-06-24 and has gone through seven block major versions
and five proof-of-work functions. A node must validate all of them.

## Module map

Line counts are for `src/` at the pinned commit (155k lines of C and C++).

| Directory | Lines | Role | Consensus? |
| --- | --- | --- | --- |
| `src/crypto` | 18,349 | ed25519 operations (`crypto-ops.c`, ref10 lineage), Keccak, the CryptoNight family (`slow-hash-*.c`), Blake/Groestl/JH/Skein finalizers, tree hash, ChaCha8, the wallet file cipher | Yes, all of it except `WalletCrypto` and `chacha8` |
| `src/config` | 5,436 | `CryptoNoteConfig.h` (every parameter), `CryptoNoteCheckpoints.h` (built-in checkpoints), `Constants.h`, `WalletConfig.h` | Yes |
| `src/serialization` | 3,457 | The binary serializer (varint based), the KV binary "portable storage" format, JSON serializers, and the field order of every CryptoNote structure in `CryptoNoteSerialization.cpp` | Yes |
| `src/common` | 9,862 | Base58, varint, `TransactionExtra`, hashing helpers (`CryptoNoteTools`), difficulty check, string tools, the notifier | Base58, varint, tx extra, `CheckDifficulty` are consensus |
| `src/cryptonotecore` | 24,301 | `Core` (chain state machine), `Currency` (reward, difficulty entry points, genesis), `ValidateTransaction`, `CachedBlock` (hashing blobs), `Difficulty` (LWMA), `TransactionPool`, `Checkpoints`, `UpgradeDetector`, `BlockchainCache` and `DatabaseBlockchainCache` (storage), `TransactionPoW` | Yes, except the pool policy, which is local |
| `src/cryptonoteprotocol` | 2,663 | The block and transaction sync protocol on top of Levin (commands 2001-2010) | Wire format yes; the state machine is behaviour peers depend on |
| `src/p2p` | 5,504 | `LevinProtocol` framing, `NetNode` (connections, handshake, peer lists, bans), `PeerListManager`, the peer state file | Wire format yes |
| `src/rpc` | 5,728 | HTTP and JSON-RPC server (cpp-httplib), request definitions | No, but wallets, pools and miners depend on it |
| `src/walletbackend`, `src/subwallets` | 9,325 | The wallet used by every current front end: file format, sync, transaction construction | Wallet file format and transaction construction must match |
| `src/wallet`, `src/transfers` | 12,739 | `WalletGreen`, the older wallet stack behind `wrkz-service` | Legacy; only its file format matters, and only for upgrading old files |
| `src/walletapi`, `src/walletservice`, `src/zedwallet++`, `src/walletcapi` | 15,682 | Front ends: HTTP wallet API, JSON-RPC wallet service, CLI wallet, the C API used by the Flutter apps | No; API contracts |
| `src/nigel`, `src/noderpcproxy` | 3,554 | Daemon RPC clients used by the wallets | Client side of the RPC contract |
| `src/system`, `src/platform` | 9,873 | A green-thread dispatcher over ucontext, Windows fibers and hand-written macOS context switching, plus per-platform TCP | No. Do not port this; use the target language's async runtime |
| `src/miner`, `src/txpowserver`, `src/netmon`, `src/daemon` | 17,810 | Solo miner, the external transaction PoW server, the network monitor, daemon startup and configuration | No |
| `src/mnemonics`, `src/utilities`, `src/errors`, `src/logging`, `src/logger` | 7,142 | Mnemonic words, mixin and fee ladders, address helpers, error codes | Mnemonics, mixin ladder, fee ladder and address helpers are consensus or wallet compatible |

External dependencies that matter to a port: RocksDB (storage, `11-storage.md`),
argon2 (the Chukwa proof of work, `02-hashing.md`), nlohmann-json (JSON),
cpp-httplib (RPC). ZeroMQ, miniupnpc and zstd are optional conveniences.

## What is consensus

A rule is consensus if two nodes that disagree on it will disagree on which
chain is valid. In this code base that set is:

- everything under `src/crypto` that is reachable from block or transaction
  validation: Keccak, the five slow hashes, tree hash, curve operations, ring
  signature verification, key image checks;
- the binary serialization and hashing blob construction in
  `src/serialization/CryptoNoteSerialization.cpp` and
  `src/cryptonotecore/CachedBlock.cpp`;
- every parameter in `src/config/CryptoNoteConfig.h`;
- `Core::addBlock`, `Core::validateBlock`, `ValidateTransaction`,
  `Currency::getBlockReward`, `Currency::getNextDifficulty`, the LWMA code in
  `Difficulty.cpp`, `Currency::checkProofOfWork`, `Mixins::validate`, the fee
  functions in `src/utilities/Utilities.cpp`, and the merge-mining checks;
- the checkpoints, because blocks inside the checkpoint zone skip proof of
  work and signature verification entirely (`07-blocks-consensus.md`).

Everything else is either a wire contract (peers or wallets depend on it but
disagreement does not fork the chain) or local policy.

## What to keep in C

The proof-of-work functions in `src/crypto/slow-hash-*.c`, `oaes_lib.c`,
`aesb.c`, the four finalizer hashes and `keccak.c` are small, self contained,
portable C with no dependencies beyond libc and, for Chukwa, argon2. They are
also the code where an off-by-one produces a hash that is wrong on one input
in a million, which no unit test finds and which forks a node months later.

Recommendation: a port links these C files unchanged behind a thin foreign
function interface, passes the vectors in `vectors/`, and only then considers
rewriting them one function at a time against the same vectors plus a
million-block replay. `02-hashing.md` lists the exact entry points.

The ed25519 code in `crypto-ops.c` is the standard ref10 arithmetic with three
CryptoNote additions (`ge_fromfe_frombytes_vartime`, `ge_mul8`,
`ge_double_scalarmult_precomp_vartime`, subgroup check). Mature libraries in
other languages provide the arithmetic; the CryptoNote additions must be
implemented to match bit for bit. `03-crypto-primitives.md` specifies them.

## What not to port

`src/system` and `src/platform` implement a cooperative green-thread
scheduler that the daemon, the P2P layer and the RPC server all run inside.
It is the source of the crash classes on record for this code base and adds
nothing to the protocol. A port should use its language's normal async or
threaded networking and reproduce the observable behaviour described in
`08-p2p-protocol.md`: message ordering per connection, timeouts, and the sync
state machine.

The legacy `WalletGreen` stack (`src/wallet`, `src/transfers`) is only reached
when opening a pre-2019 wallet file, which the current code converts on first
open. A port does not need it unless it wants to open those files.

## The three data flows

Every other document describes a piece of one of these.

### Block acceptance (daemon)

    peer ──NOTIFY_NEW_BLOCK / RESPONSE_GET_OBJECTS──▶ CryptoNoteProtocolHandler
        ──▶ Core::addBlock(CachedBlock, RawBlock)
            1. deserialize block template and transactions      04-serialization
            2. find the segment holding previousBlockHash        07 (chain segments)
            3. cumulative size <= maxBlockCumulativeSize         07 (block size)
            4. Core::validateBlock: version, parent block,
               timestamps, coinbase shape                        07
            5. difficulty for next block (LWMA)                  07 (difficulty)
            6. duplicate / consistency checks on tx hashes       07
            7. ValidateTransaction for every transaction         06
            8. reward == coinbase outputs (with size penalty)    07 (reward)
            9. checkpoint match, or proof of work                02, 07
           10. push to the segment; maybe switch chains          07 (reorg)
        ──▶ relay as lite block                                  08

### Wallet sync (wallet against a daemon)

    wallet ──POST /getwalletsyncdata {checkpoints, startHeight, ...}──▶ daemon
           ◀── {items:[{blockHeight, blockHash, coinbaseTX?, transactions:[...]}]}
        for each output: derivation = 8·a·R, P' = Hs(D‖i)·G + B ... 03
        if P' is ours: key image, store input                    10
        for each key input: if key image is ours, mark spent     10
    wallet ──POST /get_global_indexes_for_range──▶ daemon        09
        fills in global output indexes needed to spend

### Sending (wallet)

    select inputs, split amounts into denominations             10
    POST /getrandom_outs {amounts, outs_count}                   09
    assemble rings, relative offsets                             06, 10
    one-time outputs, tx extra (pubkey, payment id, nonce)       04, 10
    transaction proof of work over the prefix                    06
    ring signatures over the prefix hash                         03
    POST /sendrawtransaction {tx_as_hex}                         09

## Programs and their consumers

| Program | Built from | Consumers that must keep working |
| --- | --- | --- |
| `Wrkzd` | daemon + core + p2p + rpc | every peer on the network, every wallet, mining pools, xmrig (`/getheight`, `getblocktemplate`, `submitblock`), the explorer, netmon |
| `libwallet_capi` | walletcapi + walletbackend | the Flutter desktop, mobile and web wallets; 57 exported functions listed in `10-wallet.md` |
| `wrkz-wallet-api` | walletapi + walletbackend | HTTP integrations; documented at `docs/docs/wallet-api/` |
| `wrkz-service` | walletservice + WalletGreen | legacy JSON-RPC integrations; documented at `docs/docs/wallet-service-json-rpc/` |
| `wrkz-wallet` (source dir `src/zedwallet++`) | CLI + walletbackend | people |
| `wrkz-txpow-server` | crypto + serialization | GUI wallets offloading transaction proof of work; `TXPOWSERVER.md` |

A port that reaches stage 2 replaces `libwallet_capi` first, because its
consumers are the apps and its bugs cannot fork the chain. A port that reaches
stage 3 replaces `Wrkzd`.
