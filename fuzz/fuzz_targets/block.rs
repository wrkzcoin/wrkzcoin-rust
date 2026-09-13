// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Block blob parser: must never panic, and a parsed block must re-serialize
//! to exactly the bytes it was parsed from (the C++ round-trip property).
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_primitives::block::BlockTemplate;

fuzz_target!(|data: &[u8]| {
    if let Ok(b) = BlockTemplate::from_bytes(data) {
        if let Ok(bytes) = b.to_bytes() {
            assert_eq!(bytes, data, "block did not round-trip");
        }
        // Hashing must not panic on anything that parsed.
        let _ = b.hash();
        let _ = b.pow_hash();
    }
});
