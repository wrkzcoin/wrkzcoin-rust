// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Transaction blob parser: no panics, exact round trip, hashing total.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_primitives::ser::Reader;
use wrkz_primitives::tx::{BaseTransaction, Transaction, TransactionPrefix};

fuzz_target!(|data: &[u8]| {
    if let Ok(t) = Transaction::from_bytes(data) {
        if let Ok(bytes) = t.to_bytes() {
            assert_eq!(bytes, data, "transaction did not round-trip");
        }
        let _ = t.hash();
    }
    if let Ok(p) = TransactionPrefix::from_bytes(data) {
        assert_eq!(p.to_bytes(), data, "prefix did not round-trip");
        let _ = p.hash();
    }
    let mut r = Reader::new(data);
    if let Ok(b) = BaseTransaction::read(&mut r) {
        if r.finish().is_ok() {
            assert_eq!(b.to_bytes(), data, "base transaction did not round-trip");
        }
        let _ = b.hash();
    }
});
