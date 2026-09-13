// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Live checks against a mainnet seed node. `#[ignore]`d, so
//! `cargo test -p wrkz-wallet` stays offline; run with
//! `cargo test -p wrkz-wallet --test transfer_live -- --ignored`.
//!
//! **Nothing here sends a transaction.** They read `/info` and
//! `/getrandom_outs` and check that the ring assembly this crate performs on
//! the real answer is the one the daemon will resolve back.
//!
//! `WRKZ_DAEMON` overrides the node.

use wrkz_primitives::mixins::mixin_allowable_range;
use wrkz_primitives::tx::{absolute_to_relative_offsets, relative_offsets_to_absolute};
use wrkz_wallet::daemon::Daemon;
use wrkz_wallet::file::Hex32;

fn daemon() -> Daemon {
    let url = std::env::var("WRKZ_DAEMON").unwrap_or_else(|_| "http://node-fin.wrkz.work:17856".to_string());
    Daemon::new(&url).expect("daemon url")
}

/// The amounts this test asks for: "pretty" denominations with a long history,
/// so the chain certainly holds enough outputs of each.
const AMOUNTS: [u64; 3] = [100, 1000, 10_000];

#[test]
#[ignore = "needs the network"]
fn getrandom_outs_has_the_shape_the_ring_assembly_assumes() {
    let daemon = daemon();
    let info = daemon.info().expect("/info");
    let height = info.network_height;
    let range = mixin_allowable_range(height);

    // The wallet asks for the tier default at the *current* height, and for one
    // more output than the mixin so it can drop its own if the daemon hands it
    // back (`getRingParticipants`, `Transfer.cpp:830`).
    let mixin = range.default;
    let outs_count = mixin + 1;
    assert!(mixin >= range.min && mixin <= range.max, "the tier default is inside its own tier");
    if height >= 4_300_000 {
        assert_eq!((range.min, range.max, range.default), (1, 7, 7), "mixin tier V6");
    }

    let response = daemon.random_outs(&AMOUNTS, outs_count).expect("/getrandom_outs");
    assert_eq!(response.status, "OK");
    assert_eq!(response.outs.len(), AMOUNTS.len(), "one entry per requested amount");

    for (entry, expected_amount) in response.outs.iter().zip(AMOUNTS) {
        assert_eq!(entry.amount, expected_amount, "entries come back in request order");
        assert!(
            entry.outs.len() as u64 >= mixin,
            "amount {} returned {} outputs, fewer than the mixin {mixin}",
            entry.amount,
            entry.outs.len()
        );
        assert!(entry.outs.len() as u64 <= outs_count, "never more than asked for");

        let mut indexes = Vec::new();
        for out in &entry.outs {
            assert_eq!(out.out_key.len(), 64, "the output key is 32 hex-encoded bytes");
            let key = Hex32::from_hex(&out.out_key).expect("hex output key");
            assert!(wrkz_pow::curve::check_key(key.as_bytes()), "every ring member decompresses (check_key)");
            indexes.push(out.global_amount_index);
        }

        // Everything the offset arithmetic rests on. `setupInputs` computes the
        // deltas on `uint32_t`, so an unsorted or duplicated answer would wrap;
        // the daemon has never been observed to return one.
        assert!(indexes.windows(2).all(|w| w[0] < w[1]), "ascending and distinct: {indexes:?}");
        assert!(indexes.iter().all(|i| *i <= u64::from(u32::MAX)), "global indexes fit in the uint32 on the wire");
    }
}

#[test]
#[ignore = "needs the network"]
fn ring_assembly_and_relative_offsets_round_trip_on_live_decoys() {
    // Take a real answer, place a synthetic "our output" among the decoys the
    // way `prepareRingParticipants` does, convert to relative offsets, and
    // convert back: the daemon resolves the ring by exactly that addition.
    let daemon = daemon();
    let info = daemon.info().expect("/info");
    let mixin = mixin_allowable_range(info.network_height).default;

    let response = daemon.random_outs(&AMOUNTS, mixin + 1).expect("/getrandom_outs");

    for entry in &response.outs {
        let decoys: Vec<u64> = entry.outs.iter().map(|o| o.global_amount_index).take(mixin as usize).collect();
        assert_eq!(decoys.len() as u64, mixin, "amount {} has enough decoys", entry.amount);

        // Our own output at three positions: below every decoy, in the middle,
        // and above every decoy.
        let candidates =
            [decoys[0].saturating_sub(1), (decoys[0] + decoys[decoys.len() - 1]) / 2, decoys[decoys.len() - 1] + 1];

        for real in candidates {
            if decoys.contains(&real) {
                continue;
            }

            let mut ring = decoys.clone();
            let position = ring.iter().position(|i| *i >= real).unwrap_or(ring.len());
            ring.insert(position, real);

            assert_eq!(ring.len() as u64, mixin + 1, "ring size is mixin + 1");
            assert!(ring.windows(2).all(|w| w[0] < w[1]), "inserting at the sorted position keeps it ascending");
            assert_eq!(ring[position], real, "and the recorded position is where our output landed");

            let offsets = absolute_to_relative_offsets(&ring).expect("ascending offsets");
            assert_eq!(offsets[0], ring[0], "the first offset is absolute");
            assert!(offsets[1..].iter().all(|o| *o != 0), "no zero offset: the daemon rejects those");
            assert_eq!(relative_offsets_to_absolute(&offsets).expect("resolves"), ring, "the daemon recovers the ring");
        }
    }
}

#[test]
#[ignore = "needs the network"]
fn the_tier_at_the_live_height_is_what_the_wallet_would_ask_for() {
    // The floor semantics change at 4,300,000 (spec/06 rule 8): below it the
    // minimum is judged on the largest ring, from it on every input's own.
    let daemon = daemon();
    let height = daemon.info().expect("/info").network_height;
    let range = mixin_allowable_range(height);

    let uniform = vec![(range.default + 1) as usize; 3];
    assert_eq!(wrkz_primitives::mixins::validate_ring_sizes(&uniform, height), Ok(()));

    if height >= 4_300_000 {
        // One thin ring among full ones is refused from the fork height.
        let mixed = vec![(range.default + 1) as usize, 1];
        assert!(wrkz_primitives::mixins::validate_ring_sizes(&mixed, height).is_err());
    }
}
