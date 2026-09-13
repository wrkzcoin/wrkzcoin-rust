// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The blockchain dump end to end, on the harness's real chain: an export laid
//! out as the C++ writes it, an import through the peer path that rebuilds the
//! same state record for record, a resume, and every way a file can be wrong
//! stopping at the right block with every block before it kept.
//!
//! The chain is seeded at 4,400,000 (see `chainbuild`), above the last
//! checkpoint, so the import below validates proof of work, ring signatures
//! and every state rule — which is the point: the file is never trusted.

#[allow(dead_code, reason = "the harness is shared by several test binaries; each uses a subset")]
mod chainbuild;

use chainbuild::*;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::ops::Range;
use wrkz_chain::dump::{self, ImportFailure, ImportReport};
use wrkz_chain::{keys, records, ChainState, Checkpoints, Config};
use wrkz_primitives::Hash;
use wrkz_storage::batch::BatchStore;
use wrkz_storage::{KvStore, MemStore};

const SEEDED: u32 = 40;
const BLOCKS: u32 = 60;
const ID: Hash = [0x71; 32];

fn serving() -> Config {
    Config { store_raw_blocks: true, unwind_history: u32::MAX, recent_window: 256, ..Config::default() }
}

/// The chain an export is taken from: spends of seeded outputs, some with a
/// reused payment id, and a spend of a coinbase output the chain created.
fn source() -> Chain {
    let mut chain = Chain::with_config(SEEDED, serving());
    for n in 1..=BLOCKS {
        let mut txs = Vec::new();
        if n % 4 == 1 {
            let i = (n / 4) as usize;
            let (real, decoy) = (chain.outputs[i], chain.outputs[i + 1]);
            let tag = format!("dump spend {n}");
            txs.push(if n % 8 == 1 {
                build_spend_with_payment_id(chain.tip(), &real, &decoy, &chain.payee, FEE, tag.as_bytes(), ID)
            } else {
                chain.spend(&real, &decoy, tag.as_bytes())
            });
        }
        if n == 50 {
            // Block 2 was empty, so its coinbase is of the seeded amount; it
            // unlocked at 42.
            let real = chain.at(TIP + 2).coinbase_output;
            let decoy = chain.outputs[SEEDED as usize - 1];
            txs.push(build_spend(chain.tip(), &real, &decoy, &chain.payee, FEE, b"dump coinbase spend"));
        }
        chain.push(&txs);
    }
    chain
}

/// A state seeded exactly as the source was, with nothing built on it yet.
fn fresh() -> ChainState<MemStore> {
    Chain::with_config(SEEDED, serving()).state
}

fn export_all(chain: &Chain) -> Vec<u8> {
    let mut out = Vec::new();
    let written = dump::export(&chain.state, &mut out, TIP + 1, chain.tip() + 1, &|| false, &mut |_| {}).unwrap();
    assert_eq!(written, u64::from(BLOCKS));
    out
}

fn import<S: KvStore>(state: &mut ChainState<S>, bytes: &[u8]) -> Result<ImportReport, ImportFailure> {
    dump::import(state, Cursor::new(bytes.to_vec()), &|| false, &mut |_| {})
}

/// Each record's height and its byte range in the file, walked by hand from
/// the layout rather than with the reader under test.
fn record_ranges(bytes: &[u8]) -> Vec<(u64, Range<usize>)> {
    let token = |from: usize| -> (u64, usize) {
        let space = from + bytes[from..].iter().position(|b| *b == b' ').expect("a space after every token");
        (std::str::from_utf8(&bytes[from..space]).unwrap().parse().unwrap(), space + 1)
    };
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let (height, after_height) = token(at);
        let (length, body) = token(after_height);
        let end = body + length as usize;
        assert_eq!(bytes[end], b' ', "a space after every body");
        out.push((height, at..end + 1));
        at = end + 1;
    }
    out
}

fn only(bytes: &[u8], keep: impl Fn(u64) -> bool) -> Vec<u8> {
    record_ranges(bytes).into_iter().filter(|(h, _)| keep(*h)).flat_map(|(_, r)| bytes[r].to_vec()).collect()
}

fn assert_same_records(got: &BTreeMap<Vec<u8>, Vec<u8>>, want: &BTreeMap<Vec<u8>, Vec<u8>>, what: &str) {
    let differing = want.iter().filter(|(k, v)| got.get(*k) != Some(*v)).count()
        + got.keys().filter(|k| !want.contains_key(*k)).count();
    assert_eq!(differing, 0, "{what}: {differing} records differ");
}

#[test]
fn an_export_is_the_cpp_layout_and_imports_into_the_same_state_record_for_record() {
    let src = source();
    let bytes = export_all(&src);

    // The layout: heights in order from the first block built, each body the
    // stored block and transactions in the `RawBlock` encoding.
    let ranges = record_ranges(&bytes);
    assert_eq!(
        ranges.iter().map(|(h, _)| *h).collect::<Vec<_>>(),
        ((TIP + 1)..=src.tip()).map(u64::from).collect::<Vec<_>>()
    );
    for (height, range) in &ranges {
        let (block, txs) = src.state.raw_block(*height as u32).unwrap().expect("stored");
        let body = dump::encode_body(&block, &txs);
        let mut record = format!("{height} {} ", body.len()).into_bytes();
        record.extend_from_slice(&body);
        record.push(b' ');
        assert_eq!(&bytes[range.clone()], &record[..], "record {height}");
    }

    // Into a batched store, as the daemon has, committing every seven blocks.
    let (store, _) = seed_state_store_with_outputs(SEEDED);
    let mut dest =
        open_state_with_config(BatchStore::with_limits(store, 7, 1 << 30), serving(), Checkpoints::mainnet());
    let report = import(&mut dest, &bytes).expect("the export imports");
    assert_eq!(report, ImportReport { imported: u64::from(BLOCKS), skipped: 0, top: src.tip(), stopped: false });
    assert!(dest.store().stats().flushes >= u64::from(BLOCKS / 7), "committed in batches, not once at the end");
    assert_eq!(dest.pending_bytes(), 0, "and nothing left pending");
    assert_same_records(&dest.store().base().map, &src.state.store().map, "imported state");
}

#[test]
fn an_import_resumes_where_the_state_stops() {
    let src = source();
    let bytes = export_all(&src);
    let half = TIP + 25;

    let mut dest = fresh();
    let first = import(&mut dest, &only(&bytes, |h| h <= u64::from(half))).unwrap();
    assert_eq!((first.imported, first.skipped, first.top), (25, 0, half));
    let rest = import(&mut dest, &bytes).unwrap();
    assert_eq!((rest.imported, rest.skipped, rest.top), (35, 25, src.tip()), "the first 25 are stepped over");
    assert_same_records(&dest.store().map, &src.state.store().map, "resumed import");

    // A dump that begins exactly at the state's next block skips nothing and
    // chains onto the state's own top.
    let mut dest = fresh();
    import(&mut dest, &only(&bytes, |h| h <= u64::from(half))).unwrap();
    let tail = import(&mut dest, &only(&bytes, |h| h > u64::from(half))).unwrap();
    assert_eq!((tail.imported, tail.skipped), (35, 0));
    assert_same_records(&dest.store().map, &src.state.store().map, "tail import");

    // Everything already there: nothing to do, and that is a success.
    let again = import(&mut dest, &bytes).unwrap();
    assert_eq!((again.imported, again.skipped), (0, u64::from(BLOCKS)));
}

#[test]
fn a_file_cut_short_fails_at_that_block_and_keeps_every_block_before_it() {
    let src = source();
    let bytes = export_all(&src);
    let ranges = record_ranges(&bytes);
    let (height, range) = ranges[6].clone();
    assert_eq!(height, u64::from(TIP + 7));

    let mut dest = fresh();
    let failure = import(&mut dest, &bytes[..range.end - 10]).unwrap_err();
    assert!(failure.message.contains(&format!("ends inside the block at height {height}")), "{}", failure.message);
    assert_eq!((failure.report.imported, failure.report.top), (6, TIP + 6));
    assert_eq!(dest.tip_index(), Some(TIP + 6));
    assert_eq!(dest.tip_info().unwrap().block_hash, src.state.block_info(TIP + 6).unwrap().unwrap().block_hash);

    // Cut inside the next record's header instead.
    let mut other = fresh();
    let failure = import(&mut other, &bytes[..range.start + height.to_string().len()]).unwrap_err();
    assert!(failure.message.contains("ends inside the header of a record"), "{}", failure.message);
    assert_eq!(other.tip_index(), Some(TIP + 6));

    // The six kept are whole: the full file resumes from them to the same state.
    let rest = import(&mut dest, &bytes).unwrap();
    assert_eq!((rest.skipped, rest.imported), (6, 54));
    assert_same_records(&dest.store().map, &src.state.store().map, "resumed after a truncation");
}

#[test]
fn records_out_of_order_fail_at_the_first_one_out_of_place() {
    let src = source();
    let bytes = export_all(&src);

    let mut dest = fresh();
    let failure = import(&mut dest, &only(&bytes, |h| h != u64::from(TIP + 5))).unwrap_err();
    assert!(
        failure.message.contains(&format!(
            "found block height of {} after previous block height of {}",
            TIP + 6,
            TIP + 4
        )),
        "{}",
        failure.message
    );
    assert_eq!(dest.tip_index(), Some(TIP + 4));

    // A record repeated after it was applied.
    let ranges = record_ranges(&bytes);
    let mut repeated = bytes[..ranges[2].1.end].to_vec();
    repeated.extend_from_slice(&bytes[ranges[2].1.clone()]);
    let mut dest = fresh();
    let failure = import(&mut dest, &repeated).unwrap_err();
    assert!(
        failure.message.contains(&format!(
            "found block height of {} after previous block height of {}",
            TIP + 3,
            TIP + 3
        )),
        "{}",
        failure.message
    );
    assert_eq!(dest.tip_index(), Some(TIP + 3));
}

#[test]
fn a_corrupt_block_fails_at_its_height_and_keeps_the_blocks_before_it() {
    let src = source();
    let bytes = export_all(&src);
    let ranges = record_ranges(&bytes);

    // Block 5 carries a spend: flip the last byte of its body, which is the
    // spend's last signature byte. The transaction no longer hashes to what
    // the block names, and the peer path refuses it by rule.
    let (height, range) = ranges[4].clone();
    assert_eq!(src.state.block_transaction_hashes(height as u32).unwrap().len(), 2, "block 5 has a spend");
    let mut corrupt = bytes.clone();
    corrupt[range.end - 2] ^= 0x01;
    let mut dest = fresh();
    let failure = import(&mut dest, &corrupt).unwrap_err();
    assert!(
        failure.message.starts_with(&format!("Blockchain import file is invalid at height {height}, ")),
        "{}",
        failure.message
    );
    assert_eq!(dest.tip_index(), Some(TIP + 4));

    // A block whose parent is not the block before it in the file.
    let seed_top = src.state.block_info(TIP).unwrap().unwrap().block_hash;
    let (stranger, _) = src.branch_block(seed_top, TIP + 1, b"another first block");
    let mut wrong_parent = bytes[ranges[0].1.clone()].to_vec();
    dump::write_record(&mut wrong_parent, u64::from(TIP + 2), &stranger, &[]).unwrap();
    let mut dest = fresh();
    let failure = import(&mut dest, &wrong_parent).unwrap_err();
    assert!(
        failure.message.contains(&format!(
            "the previous block hash of the block at height {} does not match the hash of the block at height {}",
            TIP + 2,
            TIP + 1
        )),
        "{}",
        failure.message
    );
    assert_eq!(dest.tip_index(), Some(TIP + 1));

    // A body that is not a `RawBlock` at all.
    let mut dest = fresh();
    let failure = import(&mut dest, format!("{} 1 \x05 ", TIP + 1).as_bytes()).unwrap_err();
    assert!(
        failure.message.contains(&format!("cannot parse the raw block at height {}", TIP + 1)),
        "{}",
        failure.message
    );
    assert_eq!(dest.tip_index(), Some(TIP));
}

#[test]
fn an_import_asked_to_stop_commits_what_it_applied() {
    let src = source();
    let bytes = export_all(&src);
    let (store, _) = seed_state_store_with_outputs(SEEDED);
    // A batch far larger than the run: without the commit at the end nothing
    // would reach the base store.
    let mut dest =
        open_state_with_config(BatchStore::with_limits(store, 10_000, 1 << 30), serving(), Checkpoints::mainnet());
    let asked = Cell::new(0u32);
    let stop = || {
        asked.set(asked.get() + 1);
        asked.get() > 10
    };
    let report = dump::import(&mut dest, Cursor::new(bytes), &stop, &mut |_| {}).unwrap();
    assert!(report.stopped);
    assert_eq!((report.imported, report.top), (10, TIP + 10));
    let committed = dest.store().base().get(&keys::meta(keys::META_TIP)).unwrap().expect("a tip was committed");
    assert_eq!(committed, (TIP + 10).to_le_bytes().to_vec());
}

#[test]
fn an_export_is_refused_where_the_cpp_refuses_it() {
    // A chain of one block is far below the 1,000 an export needs.
    let genesis = ChainState::open_or_genesis(MemStore::default(), serving(), Checkpoints::mainnet()).unwrap();
    let e = dump::plan_export(&genesis, None).unwrap_err();
    assert_eq!(e, "Top block is too low or too high, not going to create an export. endIndex: 1");

    // A tall chain whose bodies start above height 1, like an import made
    // without them.
    let chain = fresh();
    assert!(dump::plan_export(&chain, None).unwrap_err().starts_with("No block body is stored at height 1"));
    assert!(dump::plan_export(&chain, Some(999)).unwrap_err().ends_with("endIndex: 999"), "the ceiling counts too");

    // A lite node.
    let lite = Chain::with_config(SEEDED, Config { lite_start_height: TIP + 5, ..serving() });
    let e = dump::plan_export(&lite.state, None).unwrap_err();
    assert!(e.starts_with(&format!("This is a lite node: it stores no block bodies below height {}", TIP + 5)), "{e}");

    // And the run itself stops at the first height with no body.
    let e = dump::export(&chain, &mut Vec::new(), TIP, TIP + 1, &|| false, &mut |_| {}).unwrap_err();
    assert!(e.starts_with(&format!("No block body is stored at height {TIP}")), "{e}");

    // With a body at 1, the plan is the whole chain, or the ceiling.
    let (mut store, _) = seed_state_store_with_outputs(SEEDED);
    store.put(keys::raw_block(1), records::encode_raw_block(&[1, 2, 3], &[])).unwrap();
    let chain = open_state_with_config(store, serving(), Checkpoints::mainnet());
    let plan = dump::plan_export(&chain, None).unwrap();
    assert_eq!((plan.start, plan.end, plan.note), (1, TIP + 1, None));
    assert_eq!(dump::plan_export(&chain, Some(5000)).unwrap().end, 5000);
    assert!(dump::plan_export(&chain, Some(u64::from(TIP) + 10)).unwrap().note.is_some());
}
