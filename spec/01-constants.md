# 01 - Network parameters and constants

Source: `src/config/CryptoNoteConfig.h` unless stated. Line numbers refer to
commit `8d89d7bf`. Every value here is consensus unless the table says
otherwise. Values are decimal unless prefixed `0x`.

## Identity

| Name | Value | Line | Notes |
| --- | --- | --- | --- |
| `CRYPTONOTE_NAME` | `WRKZCoin` | 442 | Used in UPnP mapping names and logs only |
| `CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX` | `999730` | 32 | varint `b2823d`; makes addresses start with `Wrkz` |
| `CRYPTONOTE_NETWORK` (network id) | `b50c4a6ccf52574165f991a4b6c143e9` | 607 | 16 bytes, sent in every handshake; a mismatch closes the connection |
| `GENESIS_COINBASE_TX_HEX` | see line 90 | 90 | 157-byte serialized coinbase; the genesis block is built from it (`07-blocks-consensus.md`) |
| `GENESIS_BLOCK_TIMESTAMP` | `1529831318` | 103 | Timestamp of block 1, used only by wallets to convert a scan timestamp to a height; the genesis block itself has timestamp 0 |
| `CRYPTONOTE_DISPLAY_DECIMAL_POINT` | `2` | 117 | 100 atomic units = 1.00 WRKZ |
| `P2P_DEFAULT_PORT` | `17855` | 528 | |
| `RPC_DEFAULT_PORT` | `17856` | 530 | |
| `ZMQ_PUB_DEFAULT_PORT` | `17857` | 532 | Not consensus |
| `SERVICE_DEFAULT_PORT` | `7856` | 539 | wrkz-service, not consensus |
| `SEED_NODES` | `node-fin.wrkz.work:17855`, `node-wrkz.btipz.com:17855` | 610 | |
| `DNS_SEED_NODES` | `seeds.wrkz.work` | 618 | A and AAAA records, port 17855 |
| `P2P_NET_DATA_FILENAME` | `p2pstate.wrkz.bin` | 424 | Peer state file, `08-p2p-protocol.md` |

## Supply and reward

| Name | Value | Line |
| --- | --- | --- |
| `MONEY_SUPPLY` | `50000000000000` (5×10¹³ atomic = 500,000,000,000.00 WRKZ) | 66 |
| `EMISSION_SPEED_FACTOR` | `22` | 80 |
| `GENESIS_BLOCK_REWARD` | `MONEY_SUPPLY * 3 / 100` = `1500000000000` | 88 |
| `FIXED_REWARD_V1` | `1000000` | 84 |
| `FIXED_REWARD_V1_HEIGHT` | `1500000` | 86 |
| `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` | `40` | 34 |
| `CRYPTONOTE_REWARD_BLOCKS_WINDOW` | `100` | 105 |
| `CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE` (v3 and later) | `100000` | 107 |
| `CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V2` (block v2) | `20000` | 109 |
| `CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V1` (block v1) | `10000` | 111 |
| `CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE` | `600` | 115 |

Base reward before height 1,500,000 is `(MONEY_SUPPLY - alreadyGeneratedCoins) >> 22`;
from 1,500,000 it is a flat `1000000` atomic (10,000.00 WRKZ). The size
penalty applies on top. See `07-blocks-consensus.md`.

## Block versions and proof of work

| Constant | Value | Line |
| --- | --- | --- |
| `UPGRADE_HEIGHT_V2` | `1` | 360 |
| `UPGRADE_HEIGHT_V3` | `2` | 362 |
| `UPGRADE_HEIGHT_V4` | `3` | 364 |
| `UPGRADE_HEIGHT_V5` | `302400` | 366 |
| `UPGRADE_HEIGHT_V6` | `600000` | 368 |
| `UPGRADE_HEIGHT_V7` | `1000000` | 370 |
| `BLOCK_MINOR_VERSION_0/1` | `0`, `1` | 458-460 |
| `UPGRADE_VOTING_THRESHOLD` / `UPGRADE_VOTING_WINDOW` / `UPGRADE_WINDOW` | `90`, `1440`, `1440` | 374-376 |

The rule (`src/cryptonotecore/UpgradeManager.cpp:25`): a block at index `i`
has major version `V` where `V` is the highest version whose upgrade height is
strictly less than `i`; if none, version 1. Voting is compiled in but every
upgrade height is fixed, so voting never runs on this chain.

**Derived table: block major version by block index**

| Block index range | Major version | Proof of work (`HASHING_ALGORITHMS_BY_BLOCK_VERSION`, line 462) |
| --- | --- | --- |
| 0 – 1 | 1 | `cn_slow_hash_v0` (CryptoNight, 2 MiB) |
| 2 | 2 | `cn_slow_hash_v0` |
| 3 | 3 | `cn_slow_hash_v0` |
| 4 – 302,400 | 4 | `cn_lite_slow_hash_v1` (CryptoNight-Lite variant 1, 1 MiB) |
| 302,401 – 600,000 | 5 | `cn_turtle_lite_slow_hash_v2` (CryptoNight-Turtle-Lite variant 2, 256 KiB) |
| 600,001 – 1,000,000 | 6 | `chukwa_slow_hash` (argon2id) |
| 1,000,001 and up | 7 | `cn_upx` (CryptoNight-UPX2, 128 KiB) |

Verified against the live chain: block 1 is v1, block 2 is v2, block 3 is v3,
block 4 is v4, block 302,401 is v5, block 600,001 is v6, block 1,000,001 is v7
(`09-rpc-and-wallet-sync.md` has the headers).

Blocks of major version 2 and above carry a merge-mining parent block whose
own major version MUST be 1 (`Core::validateBlock`). The proof of work of a
v2+ block is computed over the *parent block hashing blob*, not the block
header. See `04-serialization.md` and `07-blocks-consensus.md`.

## Difficulty

| Name | Value | Line |
| --- | --- | --- |
| `DIFFICULTY_TARGET` | `60` seconds | 24 |
| `ZAWY_DIFFICULTY_BLOCK_INDEX` | `20160` | 68 |
| `ZAWY_DIFFICULTY_V2` | `0` | 70 |
| `ZAWY_DIFFICULTY_DIFFICULTY_BLOCK_VERSION` | `3` | 72 |
| `LWMA_2_DIFFICULTY_BLOCK_INDEX` | `100000` | 74 |
| `LWMA_2_DIFFICULTY_BLOCK_INDEX_V2` | `100000` (same) | 76 |
| `LWMA_2_DIFFICULTY_BLOCK_INDEX_V3` | `128800` | 78 |
| `DIFFICULTY_WINDOW` | `17` | 220 |
| `DIFFICULTY_WINDOW_V1`, `_V2` | `2880`, `2880` | 222-224 |
| `DIFFICULTY_WINDOW_V3` (LWMA N) | `60` | 226 |
| `DIFFICULTY_BLOCKS_COUNT_V3` | `61` | 228 |
| `DIFFICULTY_CUT`, `_V1`, `_V2` | `0`, `60`, `60` | 230-234 |
| `DIFFICULTY_LAG`, `_V1`, `_V2` | `0`, `15`, `15` | 236-240 |
| `CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT` | `7200` s | 55 |
| `CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3` | `180` s (3×60) | 57 |
| `CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4` | `360` s (6×60) | 59 |
| `BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW` | `60` | 61 |
| `BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3` | `11` | 63 |

**Derived table: which difficulty algorithm and timestamp rules apply**
(`Currency::getNextDifficulty`, `Currency.cpp:540`; `Currency.h:51-77`)

| Next block index | Algorithm | Future time limit | Median window |
| --- | --- | --- | --- |
| < 20,160 | legacy CryptoNote (`Currency::nextDifficulty`) with per-version window/cut/lag | 7200 s | 60 |
| 20,160 – 99,999 | legacy with the "zawy" 17-block override (window 17, cut 0, floor 100) | 7200 s | 60 |
| 100,000 – 128,799 | LWMA-2 `nextDifficultyV4` | 180 s (V3) then 360 s (V4) from 100,000 | 11 from 128,800 |
| 128,800 and up | LWMA-2 `nextDifficultyV5` | 360 s | 11 |

**`nextDifficultyV3` is the unreachable arm, not V4 — do not fix.**
`LWMA_2_DIFFICULTY_BLOCK_INDEX_V2` is *defined as*
`LWMA_2_DIFFICULTY_BLOCK_INDEX` (line 76), so the two middle arms of
`getNextDifficulty` share the constant 100,000, and the V4 arm — written
first — catches every parent index at or above it. `nextDifficultyV4`
therefore serves 100,000 – 128,799 and `nextDifficultyV3` is never reached.
Getting this round the wrong way changes the chain: block 100,001 is the first
block whose difficulty differs, which is how the error was found on
2026-09-10. V4 also applies **no upper bound to a solvetime**. The exact code
and the reason are reproduced in `07-blocks-consensus.md`.

## Block size

| Name | Value | Line |
| --- | --- | --- |
| `CRYPTONOTE_MAX_BLOCK_NUMBER` | `500000000` | 26 |
| `CRYPTONOTE_MAX_BLOCK_BLOB_SIZE` | `500000000` | 28 |
| `CRYPTONOTE_MAX_TX_SIZE` | `1000000000` | 30 |
| `MAX_BLOCK_SIZE_INITIAL` | `100000` | 244 |
| `MAX_BLOCK_SIZE_GROWTH_SPEED_NUMERATOR` | `102400` | 246 |
| `MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR` | `525600` (blocks per year) | 248 |
| `MAX_EXTRA_SIZE` | `140000` | 250 |
| `MAX_EXTRA_SIZE_V2` | `1024` | 252 |
| `MAX_EXTRA_SIZE_V2_HEIGHT` | `543000` | 254 |
| `BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT` | `600000` | 301 |
| `TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT` | `543000` | 299 |
| `TRANSACTION_INPUT_BLOCKTIME_VALIDATION_HEIGHT` | `600000` | 303 |

## Fees

| Name | Value | Line |
| --- | --- | --- |
| `MINIMUM_FEE` | `5` | 119 |
| `MINIMUM_FEE_V1` | `50000` | 122 |
| `MINIMUM_FEE_V1_HEIGHT` | `678500` | 124 |
| `FEE_PER_BYTE_CHUNK_SIZE` | `256` | 129 |
| `FEE_PER_BYTE_CHUNK_SIZE_V2` | `128` | 132 |
| `MINIMUM_FEE_PER_BYTE_V1` | `500.0 / 256` = `1.953125` | 139 |
| `MINIMUM_FEE_PER_BYTE_V2` | `10.0 / 128` = `0.078125` | 141 |
| `MINIMUM_FEE_PER_BYTE_V1_HEIGHT` | `832000` | 144 |
| `MINIMUM_FEE_PER_BYTE_V2_HEIGHT` | `1500000` | 147 |
| `FUSION_FEE_V1_HEIGHT` | `864864` | 344 |
| `FUSION_FEE_V1` | `10000` | 346 |
| `FUSION_ZERO_FEE_V2_HEIGHT` | `1123000` | 348 |

**Derived fee ladder for a normal (non-fusion) transaction, as the daemon
enforces it** (`ValidateTransaction::validateTransactionFee`,
`src/utilities/Utilities.cpp:293-343`). Note the third row: the rate switch
happens at height 2, not 1,500,000, because of a comparison against the wrong
constant that MUST be preserved. Details in `06-transactions.md`.

| Block index | Minimum fee |
| --- | --- |
| ≤ 678,501 | `5` atomic |
| 678,502 – 831,999 | `50000` atomic |
| 832,000 – 1,499,999 | `ceil(size / 256) * 256 * 0.078125` (chunk 256, rate V2) |
| ≥ 1,500,000 | `ceil(size / 128) * 128 * 0.078125` = `10` atomic per started 128-byte chunk |

## Mixins (ring size − 1)

| Tier | From block index | Min | Max | Default | Lines |
| --- | --- | --- | --- | --- | --- |
| V0 | 0 | 0 | unlimited | 3 | 196 |
| V1 | 10,000 | 0 | 30 | 3 | 150-152, 182 |
| V2 | 302,400 | 3 | 7 | 3 | 154-156, 184 |
| V3 | 430,000 | 0 | 7 | 3 | 158-160, 186 |
| V4 | 658,500 | 1 | 3 | 3 | 162-164, 188 |
| V5 | 1,000,000 | 1 | 1 | 1 | 166-168, 190 |
| V6 | 4,300,000 | 1 | 7 | 7 | 177-179, 192 |

Enforcement (`src/cryptonotecore/Mixins.h`): the maximum is judged on the
largest ring in the transaction; the minimum is judged on the largest ring
below index 4,300,000 and on the smallest ring from 4,300,000. Both bounds
come from the tier in force at the block's index. See `06-transactions.md`.

## Unlock times and dust

| Name | Value | Line |
| --- | --- | --- |
| `UNLOCK_TIME_HEIGHT` | `1200000` | 50 |
| `UNLOCK_TIME_HEIGHT_V2` | `1500000` | 53 |
| `MINIMUM_UNLOCK_TIME_BLOCKS` | `15` | 48 |
| `UNLOCK_TIME_TRANSACTION_POOL_WINDOW` | `40` | 41 |
| `UNLOCK_TIME_TRANSACTION_POOL_WINDOW_V2` | `20` | 44 |
| `CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS` | `1` | 308 |
| `CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS` | `60` | 310 |
| `DEFAULT_DUST_THRESHOLD` | `10` | 210 |
| `DEFAULT_DUST_THRESHOLD_V2` | `0` | 212 |
| `DUST_THRESHOLD_V2_HEIGHT` | `302400` | 214 |
| `FUSION_DUST_THRESHOLD_HEIGHT_V2` | `400000` | 216 |

## Outputs and fusion

| Name | Value | Line |
| --- | --- | --- |
| `MAX_OUTPUT_SIZE_NODE` | `12500000000000` (daemon rejects larger outputs) | 287 |
| `MAX_OUTPUT_SIZE_CLIENT` | `500000000000` (wallets never create larger outputs) | 292 |
| `MAX_OUTPUT_SIZE_HEIGHT` | `800000` | 294 |
| `NORMAL_TX_MAX_OUTPUT_COUNT_V1` | `90` | 356 |
| `NORMAL_TX_MAX_OUTPUT_COUNT_V1_HEIGHT` | `777777` | 358 |
| `FUSION_TX_MAX_SIZE` | `100000 * 30 / 100` = `30000` bytes | 338 |
| `FUSION_TX_MIN_INPUT_COUNT` | `12` | 340 |
| `FUSION_TX_MIN_IN_OUT_COUNT_RATIO` | `4` | 342 |
| `FUSION_TX_MAX_POOL_COUNT` | `60` (local policy) | 354 |

## Transaction proof of work

| Name | Value | Line |
| --- | --- | --- |
| `TRANSACTION_POW_HEIGHT` | `1123000` | 256 |
| `TRANSACTION_POW_DIFFICULTY` | `20000` | 264 |
| `FUSION_TRANSACTION_POW_DIFFICULTY` | `60000` | 267 |
| `TRANSACTION_POW_HEIGHT_DYN_V1` | `1200000` | 270 |
| `TRANSACTION_POW_DIFFICULTY_DYN_V1` | `40000` | 273 |
| `MULTIPLIER_TRANSACTION_POW_DIFFICULTY_PER_IO_V1` | `1000` | 276 |
| `MULTIPLIER_TRANSACTION_POW_DIFFICULTY_FACTORED_OUT_V1` | `4` | 279 |
| `FUSION_TRANSACTION_POW_DIFFICULTY_V2` | `320000` | 282 |
| `TRANSACTION_POW_PASS_WITH_FEE_HEIGHT` | `1500000` | 259 |
| `TRANSACTION_POW_PASS_WITH_FEE` | `10000` | 261 |

## Mempool (local policy, not consensus)

| Name | Value | Line |
| --- | --- | --- |
| `CRYPTONOTE_MEMPOOL_MAX_SIZE_BYTES` | 64 MiB | 320 |
| `CRYPTONOTE_MEMPOOL_EVICT_TO_PERCENT` | `90` | 326 |
| `CRYPTONOTE_MEMPOOL_TX_LIVETIME` | 86400 s | 328 |
| `CRYPTONOTE_MEMPOOL_TX_FROM_ALT_BLOCK_LIVETIME` | 604800 s | 329 |
| `CRYPTONOTE_MAX_ALT_BLOCK_DEPTH` | `180` | 331 |
| `CRYPTONOTE_MAX_ALT_CHAIN_COUNT` | `50` | 332 |
| `CRYPTONOTE_MAX_ALT_BLOCK_COUNT` | `100` | 333 |
| `MAX_BLOCK_ALLOWED_TO_REWIND` | `4320` (3 days) | 429 |

`CRYPTONOTE_MAX_ALT_BLOCK_DEPTH` is local policy but it bounds the deepest
reorganisation a node will follow, and the wallet's parallel sync relies on it
(`10-wallet.md`).

## Fork heights (advisory)

`FORK_HEIGHTS` (line 381) lists 22 heights:
1, 40000, 100000, 302400, 430000, 543000, 600000, 678500, 777777, 832000,
864864, 1000000, 1123000, 1200000, 1500000, 1800000, 2500000, 2800000,
3500000, 3800000, 4300000, 4500000. `SOFTWARE_SUPPORTED_FORK_INDEX` is 21,
so `/info` reports `supported_height` 4,500,000.

This array gates nothing. It only feeds the `upgrade_heights` field of `/info`
and the "your software is out of date" warning. The heights that actually
change rules are the individual constants above; 1,800,000 through 3,800,000
correspond to no rule change in the current code.

`PRUNE_CAPABILITY_FORK_HEIGHT` (`4500000`, line 432) changes P2P sync peer
selection only (`08-p2p-protocol.md`); it does not change validation.
`MIN_LITE_FULL_BLOCK_DEPTH` (`20160`, line 439) is a lite-node startup check.

## P2P

| Name | Value | Line |
| --- | --- | --- |
| `P2P_CURRENT_VERSION` | `19` | 547 |
| `P2P_MINIMUM_VERSION` | `16` (peers below are refused) | 553 |
| `P2P_IPV6_CAPABILITY_VERSION` | `19` | 556 |
| `P2P_LITE_BLOCKS_PROPOGATION_VERSION` | `4` | 559 |
| `P2P_UPGRADE_WINDOW` | `2` | 563 |
| `P2P_LOCAL_WHITE_PEERLIST_LIMIT` | `1000` | 541 |
| `P2P_LOCAL_GRAY_PEERLIST_LIMIT` | `5000` | 543 |
| `P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE` | 32 MiB | 565 |
| `P2P_DEFAULT_CONNECTIONS_COUNT` | `15` (out and in) | 566 |
| `P2P_DEFAULT_WHITELIST_CONNECTIONS_PERCENT` | `70` | 568 |
| `P2P_DEFAULT_HANDSHAKE_INTERVAL` | `60` s (timed sync period) | 570 |
| `P2P_DEFAULT_PACKET_MAX_SIZE` | `50000000` | 571 |
| `P2P_DEFAULT_PEERS_IN_HANDSHAKE` | `250` | 572 |
| `P2P_DEFAULT_CONNECTION_TIMEOUT` | `5000` ms | 574 |
| `P2P_DEFAULT_PING_CONNECTION_TIMEOUT` | `2000` ms | 575 |
| `P2P_DEFAULT_INVOKE_TIMEOUT` | `120000` ms | 576 |
| `P2P_DEFAULT_HANDSHAKE_INVOKE_TIMEOUT` | `5000` ms | 577 |
| `P2P_SEED_RETRY_INTERVAL_SECONDS` | `300` | 588 |
| `P2P_SEED_RERESOLVE_INTERVAL_SECONDS` | `3600` | 589 |
| `P2P_SEED_RETRY_OUT_PEERS_FLOOR` | `3` | 590 |
| `P2P_FAILED_PEER_FORGET_SECONDS` | `600` | 591 |
| `P2P_GRAY_PEERLIST_HOUSEKEEPING_INTERVAL` | `60` s | 594 |
| `BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT` | `10000` (ids per chain entry) | 473 |
| `BLOCKS_SYNCHRONIZING_DEFAULT_COUNT` | `100` | 474 |
| `BLOCKS_SYNCHRONIZING_MAX_COUNT` | `10000` | 480 |
| `BLOCKS_SYNCHRONIZING_MAX_RESPONSE_BYTES` | 8 MiB | 489 |
| `BLOCKS_SYNCHRONIZING_SKIP_EMPTY_SCAN_MULTIPLIER` | `20` | 499 |
| `BLOCKS_SYNCHRONIZING_SKIP_EMPTY_MAX_SCAN` | `50000` | 503 |
| `COMMAND_RPC_GET_BLOCKS_FAST_MAX_COUNT` | `1000` | 490 |
| `SyncFeatures` names | `skipEmptyBlocks`, `base64`, `heightRange` | 515-525 |
| Levin signature | `0x0101010101012101` | `src/p2p/LevinProtocol.cpp:15` |
| Levin max packet | `100000000` | `LevinProtocol.cpp:20` |

## Storage defaults (local)

`ROCKSDB_WRITE_BUFFER_MB` 64, `ROCKSDB_READ_BUFFER_MB` 256,
`ROCKSDB_MAX_OPEN_FILES` 4096, `ROCKSDB_BACKGROUND_THREADS` 8 (lines 598-601).
Database schema version is `4` (`DatabaseBlockchainCache.cpp:668`).

## Wallet constants (`src/walletbackend/Constants.h`, `src/config/WalletConfig.h`)

| Name | Value | Where |
| --- | --- | --- |
| `IS_A_WALLET_IDENTIFIER` | 64 bytes, the text "If I pull that off, will you die?\nIt would be extremely painful." | Constants.h:16 |
| `IS_CORRECT_PASSWORD_IDENTIFIER` | 26 bytes, the text "You're a big guy.\nFor you." | Constants.h:26 |
| `PBKDF2_ITERATIONS` | `500000` | Constants.h:32 |
| `WALLET_FILE_FORMAT_VERSION` | `0` | Constants.h:36 |
| `LAST_KNOWN_BLOCK_HASHES_SIZE` | `50` | Constants.h:39 |
| `BLOCK_HASH_CHECKPOINTS_INTERVAL` | `5000` | Constants.h:55 |
| `PRUNE_SPENT_INPUTS_INTERVAL` | `2880` | Constants.h:59 |
| `GLOBAL_INDEXES_OBSCURITY` | `10` | Constants.h:69 |
| `BLOCK_PROCESSING_CHUNK` | `500` | Constants.h:75 |
| `standardAddressLength` | `98` | WalletConfig.h:48 |
| `shortPaymentIDLength` / `longPaymentIDLength` | `16` / `64` hex chars | WalletConfig.h:53-57 |
| `integratedAddressLength` / `integratedAddressLengthLong` | `120` / `186` | WalletConfig.h:63-64 |
| `maxBlocksPerSyncRequest` | `1000` | WalletConfig.h:111 |
| `syncRequestConcurrency` | `4` | WalletConfig.h:139 |

Wallet API password hash iterations are `10000` (`ApiConstants`), which is
different from the wallet file's 500,000 and not interchangeable.

## tx_extra tags (`src/common/TransactionExtra.h`, `src/config/Constants.h:190-231`)

| Tag | Value | Meaning |
| --- | --- | --- |
| `TX_EXTRA_TAG_PADDING` | `0x00` | zero bytes to end (max 255) |
| `TX_EXTRA_TAG_PUBKEY` | `0x01` | followed by a 32-byte transaction public key |
| `TX_EXTRA_NONCE` | `0x02` | followed by a 1-byte length and that many bytes of sub-tagged data |
| `TX_EXTRA_MERGE_MINING_TAG` | `0x03` | followed by a varint length, then varint depth and a 32-byte merkle root |
| `TX_EXTRA_TRANSACTION_POW_NONCE_IDENTIFIER` | `0x04` | followed by 8 nonce bytes; MUST be the last field |
| nonce sub-tag `TX_EXTRA_NONCE_PAYMENT_ID` | `0x00` | 32-byte plaintext payment id |
| nonce sub-tag short payment id (legacy) | `0x01`, `0x02` | 8 bytes, plaintext; parsed and ignored, never created |
| nonce sub-tag `TX_EXTRA_NONCE_ENCRYPTED_SHORT_PAYMENT_ID` | `0x03` | 8 bytes, encrypted |
| nonce sub-tag `TX_EXTRA_ARBITRARY_DATA_IDENTIFIER` | `0x7f` | varint length and bytes |
| `ENCRYPTED_PAYMENT_ID_TAIL` | `0x8d` | domain separator for the payment id keystream |
