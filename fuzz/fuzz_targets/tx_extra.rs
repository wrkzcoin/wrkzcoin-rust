// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Both tx_extra parsers (consensus and wallet) are total functions.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_primitives::tx::{parse_extra, parse_extra_wallet};

fuzz_target!(|data: &[u8]| {
    let _ = parse_extra(data);
    let _ = parse_extra_wallet(data);
});
