// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Every peer-supplied P2P payload parser is total. The first byte selects
//! the parser so one corpus covers all of them.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_p2p::msg;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, payload)) = data.split_first() else { return };
    match sel % 6 {
        0 => {
            let _ = msg::parse_handshake_response(payload);
        }
        1 => {
            let _ = msg::parse_chain_entry(payload);
        }
        2 => {
            let _ = msg::parse_get_objects_response(payload);
        }
        3 => {
            let _ = msg::parse_lite_block(payload);
        }
        4 => {
            let _ = msg::PeerlistEntry::parse_blob(payload);
        }
        _ => {
            let _ = msg::PeerlistEntry6::parse_blob(payload);
        }
    }
});
