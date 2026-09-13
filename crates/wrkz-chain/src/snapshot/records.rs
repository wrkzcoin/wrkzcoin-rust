// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The three C++ tables a snapshot carries, as this crate's records.
//!
//! | C++ table | record in the file | here |
//! | --- | --- | --- |
//! | `6` block info by index | `CachedBlockInfo`, verbatim | [`crate::BlockInfo`] under `W b` and `W h` |
//! | `7` key image → spending block | `uint32_t` | the block index under `W k` |
//! | `j` output by `(amount, global index)` | `KeyOutputInfo`, hash zeroed | [`crate::OutputRecord`] under `W o` |
//!
//! A record is filed under a table by the twelve key bytes every key of that
//! table shares, the C++ `tableKeyPrefix` (`LiteSnapshotImporter.cpp:108`), and
//! then decoded through [`wrkz_storage::fixed`], which accepts only the exact
//! layout the C++ serializer writes.

use std::sync::OnceLock;

use wrkz_primitives::Hash;
use wrkz_storage::fixed;
use wrkz_storage::records::{CachedBlockInfo, KeyOutputInfo};

use crate::records::{BlockInfo, OutputRecord};

/// The tables a snapshot may carry, in the order their keys sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Table {
    BlockInfo,
    KeyImage,
    KeyOutput,
}

impl Table {
    /// The name the C++ export's progress and errors use for the table.
    pub fn name(self) -> &'static str {
        match self {
            Table::BlockInfo => "block info",
            Table::KeyImage => "spent key images",
            Table::KeyOutput => "key output info",
        }
    }
}

/// One record of a snapshot, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotRecord {
    /// `6`: the block info at `index`.
    BlockInfo { index: u32, info: BlockInfo },
    /// `7`: `image` was spent in block `spent_at`.
    KeyImage { image: Hash, spent_at: u32 },
    /// `j`: the output at `(amount, global_index)`. Its `transaction_hash` is
    /// whatever the file says, which for a C++ export is all zeros.
    KeyOutput { amount: u64, global_index: u32, output: OutputRecord },
}

struct Prefixes {
    block_info: Vec<u8>,
    key_image: Vec<u8>,
    key_output: Vec<u8>,
}

fn prefixes() -> &'static Prefixes {
    static P: OnceLock<Prefixes> = OnceLock::new();
    P.get_or_init(|| Prefixes {
        block_info: fixed::table_prefix(wrkz_storage::codec::BLOCK_INDEX_TO_BLOCK_INFO),
        key_image: fixed::table_prefix(wrkz_storage::codec::KEY_IMAGE_TO_BLOCK_INDEX),
        key_output: fixed::table_prefix(wrkz_storage::codec::KEY_OUTPUT_KEY),
    })
}

/// The table a key belongs to, or `None` for one a snapshot may not carry.
pub fn classify(key: &[u8]) -> Option<Table> {
    let p = prefixes();
    if key.starts_with(&p.block_info) {
        Some(Table::BlockInfo)
    } else if key.starts_with(&p.key_image) {
        Some(Table::KeyImage)
    } else if key.starts_with(&p.key_output) {
        Some(Table::KeyOutput)
    } else {
        None
    }
}

/// Decode one record, refusing a table a snapshot may not carry and any layout
/// but the C++ serializer's.
pub fn decode(key: &[u8], value: &[u8]) -> Result<SnapshotRecord, String> {
    let Some(table) = classify(key) else {
        return Err("The snapshot holds a record belonging to no table a snapshot may carry. Refusing it.".into());
    };
    let bad = || {
        format!(
            "The snapshot holds a {} record that is not in the layout the C++ node writes. Refusing it.",
            table.name()
        )
    };
    Ok(match table {
        Table::BlockInfo => {
            let index = fixed::decode_block_info_key(key).ok_or_else(bad)?;
            let c = fixed::decode_block_info_value(value).ok_or_else(bad)?;
            SnapshotRecord::BlockInfo {
                index,
                info: BlockInfo {
                    block_hash: c.block_hash,
                    timestamp: c.timestamp,
                    block_size: c.block_size,
                    cumulative_difficulty: c.cumulative_difficulty,
                    already_generated_coins: c.already_generated_coins,
                    already_generated_transactions: c.already_generated_transactions,
                },
            }
        }
        Table::KeyImage => SnapshotRecord::KeyImage {
            image: fixed::decode_key_image_key(key).ok_or_else(bad)?,
            spent_at: fixed::decode_key_image_value(value).ok_or_else(bad)?,
        },
        Table::KeyOutput => {
            let (amount, global_index) = fixed::decode_key_output_key(key).ok_or_else(bad)?;
            let o = fixed::decode_key_output_value(value).ok_or_else(bad)?;
            SnapshotRecord::KeyOutput {
                amount,
                global_index,
                output: OutputRecord {
                    public_key: o.public_key,
                    unlock_time: o.unlock_time,
                    transaction_hash: o.transaction_hash,
                    output_index: o.output_index,
                    block_index: o.block_index,
                },
            }
        }
    })
}

/// The `6` record for block `index`, as the C++ export writes it: verbatim.
pub fn block_info_record(
    index: u32,
    info: &BlockInfo,
) -> ([u8; fixed::BLOCK_INFO_KEY_LEN], [u8; fixed::BLOCK_INFO_VALUE_LEN]) {
    let cached = CachedBlockInfo {
        block_hash: info.block_hash,
        timestamp: info.timestamp,
        block_size: info.block_size,
        cumulative_difficulty: info.cumulative_difficulty,
        already_generated_coins: info.already_generated_coins,
        already_generated_transactions: info.already_generated_transactions,
    };
    (fixed::block_info_key(index), fixed::block_info_value(&cached))
}

/// The `7` record for a key image spent in block `spent_at`.
pub fn key_image_record(
    image: &Hash,
    spent_at: u32,
) -> ([u8; fixed::KEY_IMAGE_KEY_LEN], [u8; fixed::KEY_IMAGE_VALUE_LEN]) {
    (fixed::key_image_key(image), fixed::key_image_value(spent_at))
}

/// The `j` record for an output, with `transactionHash` **zeroed** whatever the
/// state holds: the normalisation that makes a full node and a lite node at the
/// same height export the same bytes (`DatabaseBlockchainCache.cpp:3544`).
pub fn key_output_record(
    amount: u64,
    global_index: u32,
    output: &OutputRecord,
) -> ([u8; fixed::KEY_OUTPUT_KEY_LEN], [u8; fixed::KEY_OUTPUT_VALUE_LEN]) {
    let info = KeyOutputInfo {
        public_key: output.public_key,
        transaction_hash: [0; 32],
        unlock_time: output.unlock_time,
        output_index: output.output_index,
        block_index: output.block_index,
    };
    (fixed::key_output_key(amount, global_index), fixed::key_output_value(&info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrkz_storage::codec::{self, KeyPart};

    #[test]
    fn each_table_decodes_from_the_bytes_the_cpp_writes() {
        // Built through the generic KV encoder, which the database reader has
        // used against real C++ databases, not through the fixed layouts.
        let info = CachedBlockInfo {
            block_hash: [3; 32],
            timestamp: 1_600_000_000,
            block_size: 400,
            cumulative_difficulty: 9_000_000,
            already_generated_coins: 1_500_000_000_000,
            already_generated_transactions: 42,
        };
        let record = decode(&codec::key("6", KeyPart::U32(70_000)), &info.encode()).unwrap();
        let SnapshotRecord::BlockInfo { index, info: ours } = record else { panic!("{record:?}") };
        assert_eq!(index, 70_000);
        assert_eq!(ours.block_hash, [3; 32]);
        assert_eq!(ours.already_generated_transactions, 42);
        assert_eq!(block_info_record(index, &ours).1.to_vec(), info.encode(), "and back, verbatim");

        let record = decode(&codec::key("7", KeyPart::Hash([9; 32])), &codec::value_u32("7", 12)).unwrap();
        assert_eq!(record, SnapshotRecord::KeyImage { image: [9; 32], spent_at: 12 });

        let output = KeyOutputInfo {
            public_key: [4; 32],
            transaction_hash: [0; 32],
            unlock_time: 60,
            output_index: 2,
            block_index: 20,
        };
        let record = decode(&codec::key("j", KeyPart::AmountIndex(500, 7)), &output.encode()).unwrap();
        let SnapshotRecord::KeyOutput { amount, global_index, output: ours } = record else { panic!("{record:?}") };
        assert_eq!((amount, global_index, ours.block_index, ours.output_index), (500, 7, 20, 2));
        assert_eq!(ours.public_key, [4; 32]);
    }

    #[test]
    fn an_output_is_exported_with_its_transaction_hash_zeroed() {
        let output = OutputRecord {
            public_key: [1; 32],
            unlock_time: 5,
            transaction_hash: [0xEE; 32],
            output_index: 1,
            block_index: 3,
        };
        let (key, value) = key_output_record(10, 11, &output);
        let SnapshotRecord::KeyOutput { output: back, .. } = decode(&key, &value).unwrap() else { panic!() };
        assert_eq!(back, OutputRecord { transaction_hash: [0; 32], ..output });
    }

    #[test]
    fn a_record_of_any_other_table_is_refused() {
        let last = codec::key(codec::BLOCK_INDEX_TO_BLOCK_HASH, KeyPart::Str(codec::LAST_BLOCK_INDEX_KEY));
        assert_eq!(classify(&last), None);
        assert!(decode(&last, &codec::value_u32("8", 1)).unwrap_err().contains("belonging to no table"));
        assert!(decode(b"db_scheme_version", b"4").is_err());
        // The right table and the wrong shape: a `b` count filed as a `j` output.
        let e = decode(&codec::key("j", KeyPart::AmountIndex(1, 1)), &codec::value_u32("j", 1)).unwrap_err();
        assert!(e.contains("key output info record that is not in the layout"), "{e}");
    }
}
