// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Every network parameter, mirroring `src/config/CryptoNoteConfig.h` at
//! commit 8d89d7bf (spec/01-constants.md). Line numbers in comments refer
//! to that file. All values are consensus unless marked "local policy".

// ---- identity ---------------------------------------------------------------
pub const CRYPTONOTE_NAME: &str = "WRKZCoin";
/// Base58 address prefix; varint `b2823d`, makes addresses start with `Wrkz`. (line 32)
pub const CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX: u64 = 999730;
/// 16-byte network id sent in every handshake. (line 607)
pub const CRYPTONOTE_NETWORK: [u8; 16] =
    [0xb5, 0x0c, 0x4a, 0x6c, 0xcf, 0x52, 0x57, 0x41, 0x65, 0xf9, 0x91, 0xa4, 0xb6, 0xc1, 0x43, 0xe9];
/// Serialized genesis coinbase (157 bytes). (line 90)
pub const GENESIS_COINBASE_TX_HEX: &str = "012801ff00038090cad2c60e02484ab563a5ec4cb8aa159b878e4ca0a417e7258ec4fd338128059f2b7193dcaa8090cad2c60e02655ed6ab140ef3ca45d8d913125b8bc8917c590af4d1b9d7b4a67396e4a764088090cad2c60e020e06bf1587f9768cfd735a95e8254e98c68604f690e699f8403058422ede04282101c47eee4cfef6f30b5368d0251ad66a5800e2f0b2b70a4a3034c7bba3c5d0d6e0";
/// Genesis block nonce (`Currency.cpp:57`).
pub const GENESIS_NONCE: u32 = 70;
/// Timestamp of block 1; wallets convert a scan timestamp to a height with it. (line 103)
pub const GENESIS_BLOCK_TIMESTAMP: u64 = 1529831318;
pub const CRYPTONOTE_DISPLAY_DECIMAL_POINT: u32 = 2;
pub const P2P_DEFAULT_PORT: u16 = 17855;
pub const RPC_DEFAULT_PORT: u16 = 17856;
pub const SEED_NODES: &[&str] = &["node-fin.wrkz.work:17855", "node-wrkz.btipz.com:17855"];
pub const DNS_SEED_NODES: &[&str] = &["seeds.wrkz.work"];
pub const P2P_NET_DATA_FILENAME: &str = "p2pstate.wrkz.bin";

// ---- supply and reward ------------------------------------------------------
pub const MONEY_SUPPLY: u64 = 50_000_000_000_000;
pub const EMISSION_SPEED_FACTOR: u32 = 22;
pub const GENESIS_BLOCK_REWARD: u64 = MONEY_SUPPLY * 3 / 100;
pub const FIXED_REWARD_V1: u64 = 1_000_000;
pub const FIXED_REWARD_V1_HEIGHT: u64 = 1_500_000;
pub const CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW: u64 = 40;
pub const CRYPTONOTE_REWARD_BLOCKS_WINDOW: usize = 100;
pub const CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE: usize = 100_000;
pub const CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V2: usize = 20_000;
pub const CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V1: usize = 10_000;
pub const CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE: usize = 600;
/// `constructMinerTx` merges coinbase outputs while more than this many remain (`Core.cpp:2461`).
pub const COINBASE_MAX_OUTPUTS: usize = 11;

// ---- block versions ---------------------------------------------------------
pub const UPGRADE_HEIGHT_V2: u64 = 1;
pub const UPGRADE_HEIGHT_V3: u64 = 2;
pub const UPGRADE_HEIGHT_V4: u64 = 3;
pub const UPGRADE_HEIGHT_V5: u64 = 302_400;
pub const UPGRADE_HEIGHT_V6: u64 = 600_000;
pub const UPGRADE_HEIGHT_V7: u64 = 1_000_000;
pub const UPGRADE_HEIGHT_CURRENT: u64 = UPGRADE_HEIGHT_V7;

/// `(major version, upgrade height)` pairs, ascending. A block at index `i`
/// has the highest version whose upgrade height is strictly below `i`,
/// else 1 (`UpgradeManager.cpp:25`). A future fork adds one row here and one
/// arm in `wrkz_pow::pow_hash_for_block_version`.
pub const UPGRADE_HEIGHTS: &[(u8, u64)] = &[
    (2, UPGRADE_HEIGHT_V2),
    (3, UPGRADE_HEIGHT_V3),
    (4, UPGRADE_HEIGHT_V4),
    (5, UPGRADE_HEIGHT_V5),
    (6, UPGRADE_HEIGHT_V6),
    (7, UPGRADE_HEIGHT_V7),
];

/// `UpgradeManager::getBlockMajorVersion`.
pub fn block_major_version_for_index(index: u64) -> u8 {
    for &(version, height) in UPGRADE_HEIGHTS.iter().rev() {
        if height < index {
            return version;
        }
    }
    1
}

/// `blockGrantedFullRewardZoneByBlockVersion` (`Currency.cpp:141`).
pub fn full_reward_zone(major_version: u8) -> usize {
    match major_version {
        v if v >= 3 => CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE,
        2 => CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V2,
        _ => CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V1,
    }
}

// ---- difficulty -------------------------------------------------------------
pub const DIFFICULTY_TARGET: u64 = 60;
pub const ZAWY_DIFFICULTY_BLOCK_INDEX: u64 = 20_160;
pub const ZAWY_DIFFICULTY_V2: bool = false;
pub const ZAWY_DIFFICULTY_DIFFICULTY_BLOCK_VERSION: u8 = 3;
pub const LWMA_2_DIFFICULTY_BLOCK_INDEX: u64 = 100_000;
pub const LWMA_2_DIFFICULTY_BLOCK_INDEX_V2: u64 = 100_000;
pub const LWMA_2_DIFFICULTY_BLOCK_INDEX_V3: u64 = 128_800;
pub const DIFFICULTY_WINDOW: usize = 17;
pub const DIFFICULTY_WINDOW_V1: usize = 2880;
pub const DIFFICULTY_WINDOW_V2: usize = 2880;
pub const DIFFICULTY_WINDOW_V3: usize = 60;
pub const DIFFICULTY_BLOCKS_COUNT_V3: usize = 61;
pub const DIFFICULTY_CUT: usize = 0;
pub const DIFFICULTY_CUT_V1: usize = 60;
pub const DIFFICULTY_CUT_V2: usize = 60;
pub const DIFFICULTY_LAG: usize = 0;
pub const DIFFICULTY_LAG_V1: usize = 15;
pub const DIFFICULTY_LAG_V2: usize = 15;
pub const CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT: u64 = 7200;
pub const CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3: u64 = 3 * DIFFICULTY_TARGET;
pub const CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4: u64 = 6 * DIFFICULTY_TARGET;
pub const BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW: usize = 60;
pub const BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3: usize = 11;

/// `Currency.h:63`: future time limit by next block index.
pub fn block_future_time_limit(index: u64) -> u64 {
    if index >= LWMA_2_DIFFICULTY_BLOCK_INDEX_V3 {
        CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4
    } else if index >= LWMA_2_DIFFICULTY_BLOCK_INDEX {
        CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3
    } else {
        CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT
    }
}

/// `Currency.h:51`: timestamp median window by next block index.
pub fn timestamp_check_window(index: u64) -> usize {
    if index >= LWMA_2_DIFFICULTY_BLOCK_INDEX_V3 {
        BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3
    } else {
        BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW
    }
}

/// `difficultyWindowByBlockVersion`, `difficultyCutByBlockVersion`, `difficultyLagByBlockVersion`.
pub fn difficulty_window(major_version: u8) -> usize {
    match major_version {
        v if v >= 3 => DIFFICULTY_WINDOW,
        2 => DIFFICULTY_WINDOW_V2,
        _ => DIFFICULTY_WINDOW_V1,
    }
}
pub fn difficulty_cut(major_version: u8) -> usize {
    match major_version {
        v if v >= 3 => DIFFICULTY_CUT,
        2 => DIFFICULTY_CUT_V2,
        _ => DIFFICULTY_CUT_V1,
    }
}
pub fn difficulty_lag(major_version: u8) -> usize {
    match major_version {
        v if v >= 3 => DIFFICULTY_LAG,
        2 => DIFFICULTY_LAG_V2,
        _ => DIFFICULTY_LAG_V1,
    }
}
/// `difficultyBlocksCountByBlockVersion(version, parentIndex)` (`Currency.cpp:131`).
pub fn difficulty_blocks_count(major_version: u8, parent_index: u64) -> usize {
    if parent_index >= LWMA_2_DIFFICULTY_BLOCK_INDEX {
        DIFFICULTY_BLOCKS_COUNT_V3
    } else {
        difficulty_window(major_version) + difficulty_lag(major_version)
    }
}

// ---- block size -------------------------------------------------------------
pub const CRYPTONOTE_MAX_BLOCK_NUMBER: u64 = 500_000_000;
pub const CRYPTONOTE_MAX_BLOCK_BLOB_SIZE: usize = 500_000_000;
pub const CRYPTONOTE_MAX_TX_SIZE: usize = 1_000_000_000;
pub const MAX_BLOCK_SIZE_INITIAL: u64 = 100_000;
pub const MAX_BLOCK_SIZE_GROWTH_SPEED_NUMERATOR: u64 = 100 * 1024;
pub const MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR: u64 = 365 * 24 * 60 * 60 / DIFFICULTY_TARGET;
pub const MAX_EXTRA_SIZE: usize = 140_000;
pub const MAX_EXTRA_SIZE_V2: usize = 1024;
pub const MAX_EXTRA_SIZE_V2_HEIGHT: u64 = 543_000;
pub const BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT: u64 = 600_000;
pub const TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT: u64 = 543_000;
pub const TRANSACTION_INPUT_BLOCKTIME_VALIDATION_HEIGHT: u64 = 600_000;

/// `Currency::maxBlockCumulativeSize(height)` (`Currency.cpp:232`).
pub fn max_block_cumulative_size(height: u64) -> u64 {
    MAX_BLOCK_SIZE_INITIAL + height * MAX_BLOCK_SIZE_GROWTH_SPEED_NUMERATOR / MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR
}

// ---- fees -------------------------------------------------------------------
pub const MINIMUM_FEE: u64 = 5;
pub const MINIMUM_FEE_V1: u64 = 50_000;
pub const MINIMUM_FEE_V1_HEIGHT: u64 = 678_500;
pub const FEE_PER_BYTE_CHUNK_SIZE: u64 = 256;
pub const FEE_PER_BYTE_CHUNK_SIZE_V2: u64 = 128;
/// `500.0 / 256`. Never applied on chain: see `fees::minimum_transaction_fee`.
pub const MINIMUM_FEE_PER_BYTE_V1: f64 = 500.0 / FEE_PER_BYTE_CHUNK_SIZE as f64;
/// `10.0 / 128`.
pub const MINIMUM_FEE_PER_BYTE_V2: f64 = 10.0 / FEE_PER_BYTE_CHUNK_SIZE_V2 as f64;
pub const MINIMUM_FEE_PER_BYTE_V1_HEIGHT: u64 = 832_000;
pub const MINIMUM_FEE_PER_BYTE_V2_HEIGHT: u64 = 1_500_000;
pub const FUSION_FEE_V1_HEIGHT: u64 = 864_864;
pub const FUSION_FEE_V1: u64 = 10_000;
pub const FUSION_ZERO_FEE_V2_HEIGHT: u64 = 1_123_000;

// ---- mixins -----------------------------------------------------------------
pub const MINIMUM_MIXIN_V1: u64 = 0;
pub const MAXIMUM_MIXIN_V1: u64 = 30;
pub const MINIMUM_MIXIN_V2: u64 = 3;
pub const MAXIMUM_MIXIN_V2: u64 = 7;
pub const MINIMUM_MIXIN_V3: u64 = 0;
pub const MAXIMUM_MIXIN_V3: u64 = 7;
pub const MINIMUM_MIXIN_V4: u64 = 1;
pub const MAXIMUM_MIXIN_V4: u64 = 3;
pub const MINIMUM_MIXIN_V5: u64 = 1;
pub const MAXIMUM_MIXIN_V5: u64 = 1;
pub const MINIMUM_MIXIN_V6: u64 = 1;
pub const MAXIMUM_MIXIN_V6: u64 = 7;
pub const MIXIN_LIMITS_V1_HEIGHT: u64 = 10_000;
pub const MIXIN_LIMITS_V2_HEIGHT: u64 = 302_400;
pub const MIXIN_LIMITS_V3_HEIGHT: u64 = 430_000;
pub const MIXIN_LIMITS_V4_HEIGHT: u64 = 658_500;
pub const MIXIN_LIMITS_V5_HEIGHT: u64 = 1_000_000;
/// The next scheduled rule change (mixin tier V6). Also the height from which
/// the mixin floor is judged per input (`Mixins.h`).
pub const MIXIN_LIMITS_V6_HEIGHT: u64 = 4_300_000;
pub const DEFAULT_MIXIN_V0: u64 = 3;
pub const DEFAULT_MIXIN_V1: u64 = MINIMUM_MIXIN_V2;
pub const DEFAULT_MIXIN_V2: u64 = MINIMUM_MIXIN_V2;
pub const DEFAULT_MIXIN_V3: u64 = MINIMUM_MIXIN_V2;
pub const DEFAULT_MIXIN_V4: u64 = MAXIMUM_MIXIN_V4;
pub const DEFAULT_MIXIN_V5: u64 = MAXIMUM_MIXIN_V5;
pub const DEFAULT_MIXIN_V6: u64 = MAXIMUM_MIXIN_V6;

// ---- unlock times and dust --------------------------------------------------
pub const UNLOCK_TIME_HEIGHT: u64 = 1_200_000;
pub const UNLOCK_TIME_HEIGHT_V2: u64 = 1_500_000;
pub const MINIMUM_UNLOCK_TIME_BLOCKS: u64 = 15;
pub const UNLOCK_TIME_TRANSACTION_POOL_WINDOW: u64 = 40;
pub const UNLOCK_TIME_TRANSACTION_POOL_WINDOW_V2: u64 = 20;
pub const CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS: u64 = 1;
pub const CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS: u64 =
    DIFFICULTY_TARGET * CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS;
pub const DEFAULT_DUST_THRESHOLD: u64 = 10;
pub const DEFAULT_DUST_THRESHOLD_V2: u64 = 0;
pub const DUST_THRESHOLD_V2_HEIGHT: u64 = MIXIN_LIMITS_V2_HEIGHT;
pub const FUSION_DUST_THRESHOLD_HEIGHT_V2: u64 = 400_000;

pub fn default_dust_threshold(height: u64) -> u64 {
    if height >= DUST_THRESHOLD_V2_HEIGHT {
        DEFAULT_DUST_THRESHOLD_V2
    } else {
        DEFAULT_DUST_THRESHOLD
    }
}
pub fn default_fusion_dust_threshold(height: u64) -> u64 {
    if height >= FUSION_DUST_THRESHOLD_HEIGHT_V2 {
        DEFAULT_DUST_THRESHOLD_V2
    } else {
        DEFAULT_DUST_THRESHOLD
    }
}

// ---- outputs and fusion -----------------------------------------------------
pub const MAX_OUTPUT_SIZE_NODE: u64 = 12_500_000_000_000;
pub const MAX_OUTPUT_SIZE_CLIENT: u64 = 500_000_000_000;
pub const MAX_OUTPUT_SIZE_HEIGHT: u64 = 800_000;
pub const NORMAL_TX_MAX_OUTPUT_COUNT_V1: usize = 90;
pub const NORMAL_TX_MAX_OUTPUT_COUNT_V1_HEIGHT: u64 = 777_777;
pub const FUSION_TX_MAX_SIZE: usize = CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE * 30 / 100;
pub const FUSION_TX_MIN_INPUT_COUNT: usize = 12;
pub const FUSION_TX_MIN_IN_OUT_COUNT_RATIO: usize = 4;
/// Local policy.
pub const FUSION_TX_MAX_POOL_COUNT: usize = 60;

// ---- transaction proof of work ----------------------------------------------
pub const TRANSACTION_POW_HEIGHT: u64 = 1_123_000;
pub const TRANSACTION_POW_DIFFICULTY: u64 = 20_000;
pub const FUSION_TRANSACTION_POW_DIFFICULTY: u64 = 60_000;
pub const TRANSACTION_POW_HEIGHT_DYN_V1: u64 = 1_200_000;
pub const TRANSACTION_POW_DIFFICULTY_DYN_V1: u64 = 40_000;
pub const MULTIPLIER_TRANSACTION_POW_DIFFICULTY_PER_IO_V1: u64 = 1000;
pub const MULTIPLIER_TRANSACTION_POW_DIFFICULTY_FACTORED_OUT_V1: u64 = 4;
pub const FUSION_TRANSACTION_POW_DIFFICULTY_V2: u64 = 320_000;
pub const TRANSACTION_POW_PASS_WITH_FEE_HEIGHT: u64 = 1_500_000;
pub const TRANSACTION_POW_PASS_WITH_FEE: u64 = 10_000;

/// Required transaction PoW difficulty at previous-block index `h`
/// (spec 06-transactions.md, rule 9).
///
/// `inputs` and `outputs` are counts taken from a transaction that may not have
/// been validated yet, so the dynamic term saturates rather than overflowing;
/// the C++ computes it in `uint64_t`, where the same values would wrap. Real
/// counts are bounded by the block size limit long before either matters.
pub fn transaction_pow_difficulty(h: u64, is_fusion: bool, inputs: u64, outputs: u64) -> Option<u64> {
    if h < TRANSACTION_POW_HEIGHT {
        return None;
    }
    Some(if h <= TRANSACTION_POW_HEIGHT_DYN_V1 {
        if is_fusion {
            FUSION_TRANSACTION_POW_DIFFICULTY
        } else {
            TRANSACTION_POW_DIFFICULTY
        }
    } else if is_fusion {
        FUSION_TRANSACTION_POW_DIFFICULTY_V2
    } else {
        TRANSACTION_POW_DIFFICULTY_DYN_V1.saturating_add(
            inputs
                .saturating_add(outputs.saturating_mul(MULTIPLIER_TRANSACTION_POW_DIFFICULTY_FACTORED_OUT_V1))
                .saturating_mul(MULTIPLIER_TRANSACTION_POW_DIFFICULTY_PER_IO_V1),
        )
    })
}

// ---- mempool (local policy) -------------------------------------------------
pub const CRYPTONOTE_MEMPOOL_MAX_SIZE_BYTES: usize = 64 * 1024 * 1024;
pub const CRYPTONOTE_MEMPOOL_EVICT_TO_PERCENT: usize = 90;
pub const CRYPTONOTE_MEMPOOL_TX_LIVETIME: u64 = 60 * 60 * 24;
pub const CRYPTONOTE_MEMPOOL_TX_FROM_ALT_BLOCK_LIVETIME: u64 = 60 * 60 * 24 * 7;
pub const CRYPTONOTE_MAX_ALT_BLOCK_DEPTH: u64 = 180;
pub const CRYPTONOTE_MAX_ALT_CHAIN_COUNT: usize = 50;
pub const CRYPTONOTE_MAX_ALT_BLOCK_COUNT: usize = 100;
pub const MAX_BLOCK_ALLOWED_TO_REWIND: u64 = 4320;

// ---- fork heights (advisory) ------------------------------------------------
pub const FORK_HEIGHTS: &[u64] = &[
    1, 40_000, 100_000, 302_400, 430_000, 543_000, 600_000, 678_500, 777_777, 832_000, 864_864, 1_000_000, 1_123_000,
    1_200_000, 1_500_000, 1_800_000, 2_500_000, 2_800_000, 3_500_000, 3_800_000, 4_300_000, 4_500_000,
];
pub const SOFTWARE_SUPPORTED_FORK_INDEX: usize = 21;
pub const PRUNE_CAPABILITY_FORK_HEIGHT: u64 = 4_500_000;
pub const MIN_LITE_FULL_BLOCK_DEPTH: u64 = 20_160;

// ---- P2P --------------------------------------------------------------------
pub const P2P_CURRENT_VERSION: u8 = 19;
pub const P2P_MINIMUM_VERSION: u8 = 16;
pub const P2P_IPV6_CAPABILITY_VERSION: u8 = 19;
pub const P2P_LITE_BLOCKS_PROPOGATION_VERSION: u8 = 4;
pub const P2P_UPGRADE_WINDOW: u8 = 2;
pub const P2P_LOCAL_WHITE_PEERLIST_LIMIT: usize = 1000;
pub const P2P_LOCAL_GRAY_PEERLIST_LIMIT: usize = 5000;
pub const P2P_CONNECTION_MAX_WRITE_BUFFER_SIZE: usize = 32 * 1024 * 1024;
pub const P2P_DEFAULT_CONNECTIONS_COUNT: usize = 15;
pub const P2P_DEFAULT_WHITELIST_CONNECTIONS_PERCENT: usize = 70;
pub const P2P_DEFAULT_HANDSHAKE_INTERVAL: u64 = 60;
pub const P2P_DEFAULT_PACKET_MAX_SIZE: usize = 50_000_000;
pub const P2P_DEFAULT_PEERS_IN_HANDSHAKE: usize = 250;
pub const P2P_DEFAULT_CONNECTION_TIMEOUT_MS: u64 = 5000;
pub const P2P_DEFAULT_PING_CONNECTION_TIMEOUT_MS: u64 = 2000;
pub const P2P_DEFAULT_INVOKE_TIMEOUT_MS: u64 = 120_000;
pub const P2P_DEFAULT_HANDSHAKE_INVOKE_TIMEOUT_MS: u64 = 5000;
pub const BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT: usize = 10_000;
pub const BLOCKS_SYNCHRONIZING_DEFAULT_COUNT: usize = 100;
pub const BLOCKS_SYNCHRONIZING_MAX_COUNT: usize = 10_000;
pub const COMMAND_RPC_GET_BLOCKS_FAST_MAX_COUNT: usize = 1000;
pub const LEVIN_SIGNATURE: u64 = 0x0101010101012101;
pub const LEVIN_MAX_PACKET_SIZE: u64 = 100_000_000;

// ---- storage ----------------------------------------------------------------
pub const DB_SCHEME_VERSION: u32 = 4;

// ---- wallet -----------------------------------------------------------------
pub const IS_A_WALLET_IDENTIFIER: &[u8; 64] = b"If I pull that off, will you die?\nIt would be extremely painful.";
pub const IS_CORRECT_PASSWORD_IDENTIFIER: &[u8; 26] = b"You're a big guy.\nFor you.";
pub const PBKDF2_ITERATIONS: u32 = 500_000;
pub const WALLET_API_PBKDF2_ITERATIONS: u32 = 10_000;
pub const WALLET_FILE_FORMAT_VERSION: u32 = 0;
pub const LAST_KNOWN_BLOCK_HASHES_SIZE: usize = 50;
pub const BLOCK_HASH_CHECKPOINTS_INTERVAL: u64 = 5000;
pub const PRUNE_SPENT_INPUTS_INTERVAL: u64 = 2880;
pub const GLOBAL_INDEXES_OBSCURITY: u64 = 10;
pub const BLOCK_PROCESSING_CHUNK: usize = 500;
/// `WalletConfig::addressPrefix` (`src/config/WalletConfig.h:15`): the four
/// characters every address of this network starts with.
pub const ADDRESS_PREFIX: &str = "Wrkz";
pub const STANDARD_ADDRESS_LENGTH: usize = 98;
pub const INTEGRATED_ADDRESS_LENGTH: usize = 120;
pub const INTEGRATED_ADDRESS_LENGTH_LONG: usize = 186;
pub const MAX_BLOCKS_PER_SYNC_REQUEST: usize = 1000;
pub const SYNC_REQUEST_CONCURRENCY: usize = 4;
pub const MINIMUM_SEND: u64 = 1000;

/// `Constants::UNEXPLAINED_SYNC_START_LIMIT` (`src/walletbackend/Constants.h:51`):
/// how many times a daemon may answer from higher up the chain than the wallet
/// asked, with no lite start height to explain it, before sync stops and
/// reports a gap.
pub const UNEXPLAINED_SYNC_START_LIMIT: u64 = 3;

/// `GLOBAL_INDEX_MAX_RETRIES` (`src/walletbackend/WalletSynchronizer.cpp:282`):
/// how many times `/get_global_indexes_for_range` may fail for one block before
/// the wallet gives up and leaves the index unset.
pub const GLOBAL_INDEX_MAX_RETRIES: usize = 3;

/// `CryptoNote::BLOCKS_SYNCHRONIZING_SKIP_EMPTY_SCAN_MULTIPLIER`
/// (`src/config/CryptoNoteConfig.h:499`).
pub const BLOCKS_SYNCHRONIZING_SKIP_EMPTY_SCAN_MULTIPLIER: u64 = 20;

/// `CryptoNote::BLOCKS_SYNCHRONIZING_SKIP_EMPTY_MAX_SCAN`
/// (`src/config/CryptoNoteConfig.h:503`).
pub const BLOCKS_SYNCHRONIZING_SKIP_EMPTY_MAX_SCAN: u64 = 50_000;

// ---- wallet: transaction construction (spec/10 "Transaction construction") --

/// `WalletConfig::shortPaymentIDLength` (`src/config/WalletConfig.h:53`): the
/// number of *hex characters* of an encrypted short payment id (8 bytes).
pub const SHORT_PAYMENT_ID_LENGTH: usize = 16;

/// `WalletConfig::longPaymentIDLength` (`src/config/WalletConfig.h:57`): hex
/// characters of a plaintext 32-byte payment id.
pub const LONG_PAYMENT_ID_LENGTH: usize = 64;

/// `CryptoNote::TX_POW_NONCE_SIZE` (`src/cryptonotecore/TransactionPoW.h:60`):
/// the transaction proof-of-work nonce is eight bytes, little endian, written
/// at the very end of `extra` behind the `0x04` tag — and therefore at the very
/// end of the serialized prefix that `cn_upx` hashes.
pub const TX_POW_NONCE_SIZE: usize = 8;

/// `Constants::TX_EXTRA_*_IDENTIFIER` (`src/config/Constants.h:190-231`),
/// repeated here as the order the wallet writes them in: pubkey, nonce
/// (payment id and/or arbitrary data), proof-of-work nonce
/// (`Transfer.cpp:1487-1510`, `TransactionPoW.cpp:96`).
pub const WALLET_EXTRA_FIELD_ORDER: [u8; 3] = [0x01, 0x02, 0x04];

/// `Constants::PRETTY_AMOUNTS` (`src/config/Constants.h:15`): `1..=9 * 10^k`
/// for `k = 0..=18`, 171 values. The wallet only ever creates outputs of these
/// amounts (`SendTransaction::verifyAmounts`); the daemon does not enforce it
/// for normal transactions.
pub fn is_pretty_amount(amount: u64) -> bool {
    // `9 * 10^18` is the largest entry; nothing above it is a member however
    // few significant digits it has.
    if amount == 0 || amount > 9_000_000_000_000_000_000 {
        return false;
    }
    let mut a = amount;
    while a.is_multiple_of(10) {
        a /= 10;
    }
    // What is left is the single significant digit iff the amount was `d*10^k`.
    a < 10
}

/// The 171 values of `PRETTY_AMOUNTS`, in ascending order.
pub fn pretty_amounts() -> Vec<u64> {
    let mut out = Vec::with_capacity(171);
    let mut order: u64 = 1;
    for _ in 0..19 {
        for d in 1..=9u64 {
            out.push(d * order);
        }
        order = order.wrapping_mul(10);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_table_matches_live_chain() {
        // 01-constants.md "Derived table", verified against live headers.
        let expect = [
            (0, 1),
            (1, 1),
            (2, 2),
            (3, 3),
            (4, 4),
            (302_400, 4),
            (302_401, 5),
            (600_000, 5),
            (600_001, 6),
            (1_000_000, 6),
            (1_000_001, 7),
            (4_213_000, 7),
        ];
        for (i, v) in expect {
            assert_eq!(block_major_version_for_index(i), v, "index {i}");
        }
    }

    #[test]
    fn size_and_pow_tables() {
        assert_eq!(MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR, 525_600);
        assert!(max_block_cumulative_size(4_200_000) > 900_000 && max_block_cumulative_size(4_200_000) < 930_000);
        assert_eq!(transaction_pow_difficulty(1_300_000, false, 2, 4), Some(58_000));
        assert_eq!(transaction_pow_difficulty(1_123_000, true, 20, 3), Some(60_000));
        assert_eq!(transaction_pow_difficulty(1_122_999, false, 1, 1), None);
        assert_eq!(GENESIS_BLOCK_REWARD, 1_500_000_000_000);
        assert_eq!(FUSION_TX_MAX_SIZE, 30_000);
        assert_eq!(FORK_HEIGHTS[SOFTWARE_SUPPORTED_FORK_INDEX], 4_500_000);
        assert_eq!(block_future_time_limit(99_999), 7200);
        assert_eq!(block_future_time_limit(100_000), 180);
        assert_eq!(block_future_time_limit(128_800), 360);
        assert_eq!(difficulty_blocks_count(7, 4_213_649), 61);
        assert_eq!(difficulty_blocks_count(1, 0), 2895);
        assert_eq!(difficulty_blocks_count(4, 50_000), 17);
    }

    #[test]
    fn pretty_amounts_table() {
        let table = pretty_amounts();
        assert_eq!(table.len(), 171, "1..=9 * 10^0..=18");
        assert_eq!(table[0], 1);
        assert_eq!(table[8], 9);
        assert_eq!(table[9], 10);
        assert_eq!(*table.last().unwrap(), 9_000_000_000_000_000_000);
        assert!(table.windows(2).all(|w| w[0] < w[1]), "ascending");
        for a in &table {
            assert!(is_pretty_amount(*a), "{a} is in the table");
        }
        for a in [0u64, 11, 101, 1_000_000_001, u64::MAX, 9_000_000_000_000_000_001] {
            assert!(!is_pretty_amount(a), "{a} is not in the table");
        }
    }
}
