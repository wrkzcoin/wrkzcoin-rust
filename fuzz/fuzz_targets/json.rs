// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The JSON parser every RPC body goes through (`wrkz_rpc::json`), which is
//! hand-written and reads bytes straight off a public socket.
//!
//! Two properties: parsing is total within its limits, and what parses must
//! re-serialize to something that parses back to the same value — the writer
//! and the reader agree, which is what keeps a response byte-identical to the
//! C++'s for the same values.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_rpc::json::{self, ParseLimits};

fuzz_target!(|data: &[u8]| {
    // The daemon's own limits, so the depth cap and the byte cap are the ones
    // a request actually meets (`RpcLimits`).
    let limits = ParseLimits::default();
    let Ok(value) = json::parse(data, limits) else { return };
    let text = value.to_string();
    let again = json::parse(text.as_bytes(), limits).expect("what we wrote must parse");
    assert_eq!(again.to_string(), text, "JSON did not round-trip");
});
