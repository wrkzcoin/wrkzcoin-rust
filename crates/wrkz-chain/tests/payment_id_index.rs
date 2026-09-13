// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The payment-id index under schema 4: reuse appends one entry instead of
//! rewriting the list, a reorganisation takes back exactly what its blocks
//! added, and a schema-3 state — whose lists are one value each — is read and
//! unwound as it is, with no re-import.
//!
//! The chain is the harness's real chain; see `chainbuild`.

#[allow(dead_code, reason = "the harness is shared by several test binaries; each uses a subset")]
mod chainbuild;

use chainbuild::*;
use wrkz_chain::{keys, records, AddStatus, ChainState, Checkpoints, Config};
use wrkz_primitives::tx::Transaction;
use wrkz_primitives::Hash;
use wrkz_storage::{KvStore, MemStore};

const ID: Hash = [0x5a; 32];

/// The daemon's configuration: bodies kept and every block unwindable.
fn serving() -> Config {
    Config { store_raw_blocks: true, unwind_history: u32::MAX, recent_window: 256, ..Config::default() }
}

/// A spend of seeded outputs `real` and `decoy` that carries [`ID`].
fn pay(chain: &Chain, real: usize, decoy: usize) -> Transaction {
    build_spend_with_payment_id(
        chain.tip(),
        &chain.outputs[real],
        &chain.outputs[decoy],
        &chain.payee,
        FEE,
        &[b'p', real as u8],
        ID,
    )
}

/// Apply `tx` in the next block and return its hash.
fn mine(chain: &mut Chain, tx: Transaction) -> Hash {
    let hash = tx.hash().unwrap();
    chain.push(&[tx]);
    hash
}

fn index_of(chain: &Chain) -> Vec<Hash> {
    chain.state.transaction_hashes_by_payment_id(&ID).unwrap()
}

fn le32(store: &MemStore, key: &[u8]) -> Option<u32> {
    store.get(key).unwrap().map(|raw| u32::from_le_bytes(raw[..].try_into().expect("4 bytes")))
}

/// Swap a chain's store for what `f` makes of it, reopening the state on it —
/// the way a restarted daemon would find it.
fn reopen(chain: Chain, f: impl FnOnce(&mut MemStore)) -> Chain {
    let Chain { state, built, outputs, miner, payee, seed_tip } = chain;
    let mut store = state.into_store();
    f(&mut store);
    let mut state = ChainState::open(store, serving(), Checkpoints::mainnet()).expect("reopens");
    state.set_clock(Some(NOW));
    Chain { state, built, outputs, miner, payee, seed_tip }
}

/// Apply a branch of `blocks` empty blocks from index `from` on top of
/// `previous`, and check that its last block — and only its last — makes it
/// the main chain.
fn overtake(chain: &mut Chain, mut previous: Hash, from: u32, blocks: u32) {
    for i in 0..blocks {
        let index = from + i;
        let (blob, hash) = chain.branch_block(previous, index, &[b'B', i as u8]);
        let status = chain.state.add_block(&blob, &[]).unwrap_or_else(|e| panic!("branch block {index}: {e}")).status;
        let expected = if i + 1 == blocks { AddStatus::AlternativeAndSwitched } else { AddStatus::Alternative };
        assert_eq!(status, expected, "branch block {index}");
        previous = hash;
    }
}

#[test]
fn a_reused_payment_id_is_appended_to_not_rewritten() {
    let mut chain = Chain::with_config(24, serving());
    let mut mined = Vec::new();
    for i in 0..8 {
        let tx = pay(&chain, 2 * i, 2 * i + 1);
        mined.push(mine(&mut chain, tx));
    }
    // Two in one block: the second is numbered after the first.
    let (a, b) = (pay(&chain, 16, 17), pay(&chain, 18, 19));
    mined.extend([a.hash().unwrap(), b.hash().unwrap()]);
    chain.push(&[a, b]);
    assert_eq!(index_of(&chain), mined, "every transaction, in the order it was mined");

    let store = chain.state.into_store();
    assert_eq!(store.get(&keys::payment_id(&ID)).unwrap(), None, "schema 4 never writes the legacy list");
    assert_eq!(le32(&store, &keys::payment_id_count(&ID)), Some(10));
    for (n, hash) in mined.iter().enumerate() {
        assert_eq!(store.get(&keys::payment_id_entry(&ID, n as u32)).unwrap().as_deref(), Some(&hash[..]), "entry {n}");
    }
    assert_eq!(le32(&store, &keys::meta(keys::META_VERSION)), Some(keys::STATE_SCHEMA_VERSION));
}

#[test]
fn a_reorganisation_takes_back_exactly_what_its_blocks_added() {
    let mut chain = Chain::with_config(9, serving());
    let fork = chain.tip_hash();
    let a = pay(&chain, 0, 1);
    mine(&mut chain, a);
    let (b, c) = (pay(&chain, 2, 3), pay(&chain, 4, 5));
    chain.push(&[b, c]);
    assert_eq!(index_of(&chain).len(), 3);

    overtake(&mut chain, fork, TIP + 1, 3);
    assert_eq!(index_of(&chain), Vec::<Hash>::new(), "the whole index went with its blocks");

    // The branch spent nothing, so the same outputs can be spent again; the
    // entries start from zero once more.
    let again = pay(&chain, 0, 1);
    let again = mine(&mut chain, again);
    assert_eq!(index_of(&chain), vec![again]);

    let store = chain.state.into_store();
    assert_eq!(le32(&store, &keys::payment_id_count(&ID)), Some(1));
    assert_eq!(store.get(&keys::payment_id_entry(&ID, 1)).unwrap(), None, "no stale entry past the count");
}

/// The operator's case: a state imported by a schema-3 build, holding its
/// lists in [`keys::TAG_PAYMENT_ID`], picked up by this one.
#[test]
fn a_schema_3_state_is_read_extended_and_unwound_without_a_reimport() {
    let mut chain = Chain::with_config(9, serving());
    let a = pay(&chain, 0, 1);
    let a = mine(&mut chain, a);
    let after_a = chain.tip_hash();
    let b = pay(&chain, 2, 3);
    let b = mine(&mut chain, b);

    // Rewrite the index into the schema-3 layout: one list, no entries, and
    // the version a schema-3 build records.
    let chain = reopen(chain, |store| {
        store
            .write_batch(vec![
                (keys::payment_id(&ID), Some(records::encode_hashes(&[a, b]))),
                (keys::payment_id_entry(&ID, 0), None),
                (keys::payment_id_entry(&ID, 1), None),
                (keys::payment_id_count(&ID), None),
                (keys::meta(keys::META_VERSION), Some(3u32.to_le_bytes().to_vec())),
            ])
            .unwrap();
    });
    assert_eq!(index_of(&chain), vec![a, b], "the legacy list is read as it is");

    // A block applied by this build appends an entry after the list.
    let mut chain = chain;
    let c = pay(&chain, 4, 5);
    let c = mine(&mut chain, c);
    assert_eq!(index_of(&chain), vec![a, b, c]);

    // A branch from after `a` overtakes `b` and `c`: `c` comes off the
    // entries, `b` off the legacy list.
    overtake(&mut chain, after_a, TIP + 2, 3);
    assert_eq!(index_of(&chain), vec![a]);

    let store = chain.state.into_store();
    assert_eq!(store.get(&keys::payment_id(&ID)).unwrap(), Some(records::encode_hashes(&[a])));
    assert_eq!(store.get(&keys::payment_id_count(&ID)).unwrap(), None, "no entries left, no counter");
    assert_eq!(le32(&store, &keys::meta(keys::META_VERSION)), Some(keys::STATE_SCHEMA_VERSION), "upgraded in place");
}
