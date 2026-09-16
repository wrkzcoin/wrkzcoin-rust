// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `p2pstate.wrkz.bin`, the peer state file (spec/08 "Peer state file").
//!
//! It is **not** KV binary: a bare sequence of varints and fixed-width fields
//! with no names and no type tags, so every length in it is a number the file
//! declares and nothing cross-checks. A node reads it at start-up, before it
//! has spoken to anybody, and a file written by an older build — or by
//! something else entirely — must be an error rather than a crash or a
//! multi-gigabyte allocation.
//!
//! The property is that decoding is total and that the lists it produces stay
//! inside `P2P_LOCAL_WHITE/GRAY_PEERLIST_LIMIT`, which is what stops a
//! declared count from sizing an allocation.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_node::peers::PeerManager;

fuzz_target!(|data: &[u8]| {
    let mut pm = PeerManager::new(true);
    if pm.decode(data).is_err() {
        return;
    }
    // Whatever loaded must be within the limits the encoder respects, and the
    // accessors the connection maker calls must not panic on it.
    let _ = pm.white_addresses();
    let _ = pm.gray_addresses();
    assert!(pm.white_count() <= wrkz_primitives::constants::P2P_LOCAL_WHITE_PEERLIST_LIMIT);
    assert!(pm.gray_count() <= wrkz_primitives::constants::P2P_LOCAL_GRAY_PEERLIST_LIMIT);
});
