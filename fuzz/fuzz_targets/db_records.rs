// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The record decoders that read a database: the C++ node's own encodings
//! (`wrkz_storage::records`, `::codec`, spec/11) and the state this port
//! writes (`wrkz_chain::records`).
//!
//! `wrkz-replay` and `wrkz-db-inspect` open a RocksDB directory someone hands
//! over — a snapshot from a friend, a copy from a seed node — so every one of
//! these reads bytes the operator did not produce. The property is that they
//! are total: a corrupt or hostile value is an error, never a panic and never
//! an allocation sized from a length the value declared.
//!
//! The first byte selects the decoder so one corpus covers all of them.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_storage::{codec, records};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, doc)) = data.split_first() else { return };
    match sel % 12 {
        // The C++ node's records (spec/11 "Record encodings").
        0 => {
            let _ = records::CachedBlockInfo::decode(doc);
        }
        1 => {
            // `CachedTransactionInfo` has no decoder of its own: it is always
            // read out of the section `ExtendedTransactionInfo` wraps it in.
            if let Ok(section) = codec::decode_object(codec::TRANSACTION_HASH_TO_TRANSACTION_INFO, doc) {
                let _ = records::CachedTransactionInfo::from_section(&section);
            }
        }
        2 => {
            let _ = records::ExtendedTransactionInfo::decode(doc);
        }
        3 => {
            let _ = records::KeyOutputInfo::decode(doc);
        }
        4 => {
            let _ = records::RawBlockRecord::decode(doc);
        }
        // The KV-binary scalar accessors the reader uses around them.
        5 => {
            let _ = codec::decode_u64(codec::BLOCK_HASH_TO_BLOCK_INDEX, doc);
        }
        6 => {
            let _ = codec::decode_hash(codec::BLOCK_INDEX_TO_BLOCK_HASH, doc);
        }
        7 => {
            let _ = codec::decode_hashes(codec::BLOCK_INDEX_TO_TX_HASHES, doc);
        }
        8 => {
            let _ = codec::decode_object(codec::BLOCK_INDEX_TO_BLOCK_INFO, doc);
        }
        // This port's own state records.
        9 => {
            let _ = wrkz_chain::records::BlockInfo::decode(doc);
            let _ = wrkz_chain::records::OutputRecord::decode(doc);
        }
        10 => {
            let _ = wrkz_chain::records::decode_hashes(doc);
            let _ = wrkz_chain::records::decode_payment_id_refs(doc);
        }
        _ => {
            let _ = wrkz_chain::records::decode_output_refs(doc);
            let _ = wrkz_chain::records::decode_raw_block(doc);
        }
    }
});
