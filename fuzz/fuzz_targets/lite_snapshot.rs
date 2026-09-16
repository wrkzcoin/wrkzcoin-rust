// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The lite node snapshot container (`wrkz_chain::snapshot::container`), which
//! `--import-lite-snapshot` reads from a file an operator downloaded.
//!
//! One target covers the whole path a hostile file travels: the 128-byte
//! header, the frame lengths, the zstd decompression of each frame, the
//! ascending-key check, and the per-record decode of every table a snapshot
//! may carry. All of it runs before a single byte reaches the chain state.
//!
//! The property is that reading is total and bounded: a truncated, reordered,
//! over-declared or zstd-bomb frame is an `Err`, never a panic and never an
//! allocation past `MAX_FRAME_PAYLOAD`.
#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;
use wrkz_chain::snapshot::container::Reader;
use wrkz_chain::snapshot::records;

fuzz_target!(|data: &[u8]| {
    let Ok(mut reader) = Reader::new(Cursor::new(data), "fuzz") else { return };
    let mut previous: Option<Vec<u8>> = None;
    loop {
        // Copied out so the reader is free for the next call; a record is a
        // borrow of its frame.
        let record = match reader.next_record() {
            Ok(Some((key, value))) => (key.to_vec(), value.to_vec()),
            Ok(None) | Err(_) => return,
        };
        let (key, value) = record;
        if let Some(previous) = &previous {
            assert!(previous.as_slice() < key.as_slice(), "records must arrive with ascending keys");
        }
        // The importer decodes every record it is handed; a record the reader
        // accepted must not panic on the way into a table.
        let _ = records::decode(&key, &value);
        previous = Some(key);
    }
});
