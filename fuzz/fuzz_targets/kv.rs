// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! KV binary (portable storage) decoder: no panics, bounded memory, and
//! whatever decodes must encode back and decode to the same document.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_primitives::kv;

fuzz_target!(|data: &[u8]| {
    if let Ok(section) = kv::decode(data) {
        let bytes = kv::encode(&section);
        let again = kv::decode(&bytes).expect("re-encoded document must decode");
        assert_eq!(again, section, "kv document did not round-trip");
    }
});
