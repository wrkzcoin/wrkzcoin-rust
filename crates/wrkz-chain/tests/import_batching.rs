// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Batched state writes: that they are faster, and that they change nothing.
//!
//! The import writes one RocksDB batch per block, so a 4.2-million-block replay
//! asks the engine for 4.2 million batches. `wrkz_storage::batch::BatchStore`
//! puts N blocks in one, which only works if a block inside the batch sees the
//! state the blocks before it wrote — it may spend their outputs, name them as
//! ring members, take the next global index from a counter they bumped, and
//! must be refused if it respends a key image they spent.
//!
//! These tests build a real chain at 4,400,000 — above the last checkpoint, so
//! the proof of work, the ring signatures and every state rule run — export it
//! as the C++ database a replay reads, and then replay it twice: once
//! unbatched, once batched. Everything about the two runs has to match: the
//! verdicts, the rules a rejection names, and the resulting state byte for byte.
//!
//! The measurement at the bottom is `#[ignore]`d; it prints, it asserts nothing
//! about speed.

#[allow(dead_code, reason = "the harness is shared by several test binaries; each uses a subset")]
mod chainbuild;

use chainbuild::{Chain, COINBASE_LOCK, TIP};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use wrkz_chain::replay::{replay, ReplayCost, ReplayOptions};
use wrkz_chain::{ChainState, Checkpoints, Rule, TxRule};
use wrkz_storage::batch::BatchStore;
use wrkz_storage::counting::{CountingStore, Counts};
use wrkz_storage::reader::ChainReader;
use wrkz_storage::{KvStore, MemStore};

/// The store the replay writes through: a batching overlay over a counting
/// wrapper over the real map, so that a test can see both what the chain state
/// asked for and what actually reached the engine.
type ImportStore = BatchStore<CountingStore<MemStore>>;

fn import_store(batch_blocks: u32) -> ImportStore {
    let (seed, _) = chainbuild::seed_state_store();
    BatchStore::with_limits(CountingStore::new(seed), batch_blocks, 1 << 30)
}

fn quiet() -> impl FnMut(&str) {
    |_: &str| {}
}

/// What one replay left behind: the top index it reached (or the failure), the
/// records that were actually committed, and what the engine underneath saw.
type RunOutcome = (Result<u32, String>, BTreeMap<Vec<u8>, Vec<u8>>, Counts);

/// Replay `source` into a fresh seeded state batching `batch_blocks` blocks at a
/// time, and hand back the committed records and what the engine saw.
///
/// `batch_blocks == 1` is the unbatched path: one engine batch per block, which
/// is exactly what the code did before batching existed.
fn run(source: &ChainReader<MemStore>, batch_blocks: u32) -> RunOutcome {
    let mut chain = chainbuild::open_state(import_store(batch_blocks));
    let outcome =
        replay(source, &mut chain, &ReplayOptions { progress: 1_000_000, ..Default::default() }, &mut quiet())
            .map(|r| r.top);
    let store = chain.into_store();
    // `replay` syncs before it returns, on the failure path as well as the
    // success one, so everything applied is committed by now.
    assert_eq!(store.pending_keys(), 0, "the replay must leave nothing uncommitted");
    let counts = store.base().counts();
    let records = store.base().inner().map.clone();
    (outcome, records, counts)
}

/// The verdict of an `add_block` into a chain that is being batched, without
/// going through the replay: the rule it named, if it failed.
fn direct_verdict(chain: &mut ChainState<ImportStore>, blob: &[u8], txs: &[Vec<u8>]) -> Result<(), Rule> {
    match chain.add_block(blob, txs) {
        Ok(_) => Ok(()),
        Err(e) => Err(e.rule().cloned().unwrap_or_else(|| panic!("expected a consensus rule, got {e}"))),
    }
}

// ---------------------------------------------------------------------------
// read-your-own-writes
// ---------------------------------------------------------------------------

/// The headline requirement: a block that spends an output an earlier block of
/// the **same batch** created must validate identically batched and unbatched.
///
/// The spend is of a coinbase output, which is the earliest an output of a
/// block on this chain can legally be spent: `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW`
/// is 40, so the coinbase of block `B` is spendable from block `B + 40`. Both
/// blocks sit far inside one batch of 200, so under batching neither the output
/// record nor the per-amount counter the ring reads has reached the engine when
/// the spend is validated — they exist only in the overlay.
#[test]
fn a_block_spending_an_output_made_earlier_in_the_same_batch_validates_the_same_either_way() {
    let mut chain = Chain::new();
    let created = chain.push_empty().coinbase_output;
    assert_eq!(created.global_index, 9, "the coinbase continues the seeded amount's indexes");
    // Fill up to the unlock height.
    for _ in 1..COINBASE_LOCK {
        chain.push_empty();
    }
    // The spend, at TIP + 41: the coinbase of block TIP + 1 carries
    // `unlockTime = (TIP + 1) + 40`, and `isTransactionSpendTimeUnlocked` asks
    // for `blockIndex + 1 >= unlockTime` with `blockIndex` the *parent's* index,
    // so the first block that may spend it is TIP + 41.
    let decoy = chain.outputs[0];
    let tx = chain.spend(&created, &decoy, b"batch spend");
    chain.push(&[tx]);
    let spend_index = chain.tip();
    assert_eq!(spend_index, TIP + COINBASE_LOCK + 1, "the earliest legal height for a coinbase output");

    let source = ChainReader::new(chain.export());
    let (unbatched, unbatched_records, _) = run(&source, 1);
    let (batched, batched_records, _) = run(&source, 200);

    assert_eq!(unbatched.as_ref().unwrap(), &spend_index, "the unbatched run applies the spend");
    assert_eq!(batched, unbatched, "the batched run must reach the same height");
    assert_eq!(batched_records, unbatched_records, "and leave byte-identical state");
}

/// The same output named by a ring one block after it was created.
///
/// It cannot be spent there — a coinbase is locked for 40 blocks — but the
/// *record* must still be found, and that is the point: a validator that could
/// not see the overlay would report `INPUT_INVALID_GLOBAL_INDEX` ("no such
/// output") instead of `INPUT_SPEND_LOCKED_OUT` ("that output is locked"). The
/// two are different rules, so the assertion distinguishes an overlay that
/// works from one that is not consulted at all.
#[test]
fn an_output_created_by_the_previous_block_is_visible_to_the_next_one() {
    let mut chain = Chain::new();
    let created = chain.push_empty().coinbase_output;
    let decoy = chain.outputs[0];
    let tx = chain.spend(&created, &decoy, b"too soon");

    let rule_at = |batch_blocks: u32| -> Rule {
        let mut state = chainbuild::open_state(import_store(batch_blocks));
        // Replay the first block into this state so that the batched run really
        // is holding it back when the second is validated.
        let first = chain.at(TIP + 1);
        state.add_block(&first.blob, &first.tx_blobs).expect("the first block applies");
        state.block_boundary().expect("a block boundary");
        assert_eq!(
            state.store().pending_keys() > 0,
            batch_blocks > 1,
            "batching must actually be holding the first block back"
        );
        let built = chain.build_only(std::slice::from_ref(&tx));
        direct_verdict(&mut state, &built.0, &built.1).expect_err("locked")
    };

    let batched = rule_at(200);
    let unbatched = rule_at(1);
    assert_eq!(batched, unbatched, "the rule must not depend on the batch size");
    match batched {
        Rule::Transaction { rule: TxRule::InputSpendLockedOut { global_index, .. }, .. } => {
            assert_eq!(global_index, created.global_index as u64, "the ring resolved to the new output");
        }
        other => panic!("expected INPUT_SPEND_LOCKED_OUT (which proves the record was found), got {other}"),
    }
}

/// A key image spent inside the batch must be seen as spent by a later block of
/// the same batch.
///
/// Without the overlay the second block's `checkIfSpent` would read the engine,
/// find nothing — the first block's `7` record is still in the batch — and let
/// a double spend onto the chain.
#[test]
fn a_key_image_spent_inside_the_batch_is_spent_for_the_rest_of_it() {
    let mut chain = Chain::new();
    let created = chain.push_empty().coinbase_output;
    for _ in 1..COINBASE_LOCK {
        chain.push_empty();
    }
    let decoy = chain.outputs[0];
    let first = chain.spend(&created, &decoy, b"first spend");
    chain.push(std::slice::from_ref(&first));

    // The same key image again, one block later. `build_spend` is deterministic
    // in the output it spends, so respending `created` produces the same image.
    let respend = chain.spend(&created, &decoy, b"second spend");
    assert_eq!(
        key_image(&respend),
        key_image(&first),
        "the two transactions must carry the same key image for this to be a double spend"
    );
    let built = chain.build_only(&[respend]);

    for batch_blocks in [1u32, 200] {
        let mut state = chainbuild::open_state(import_store(batch_blocks));
        for b in &chain.built {
            state.add_block(&b.blob, &b.tx_blobs).unwrap_or_else(|e| panic!("block {}: {e}", b.index));
            state.block_boundary().expect("a block boundary");
        }
        assert_eq!(
            state.store().pending_keys() > 0,
            batch_blocks > 1,
            "batching must still be holding the first spend back"
        );
        let rule = direct_verdict(&mut state, &built.0, &built.1).expect_err("a double spend");
        assert!(
            matches!(rule, Rule::Transaction { rule: TxRule::InputKeyImageAlreadySpent { .. }, .. }),
            "batch of {batch_blocks}: expected INPUT_KEYIMAGE_ALREADY_SPENT, got {rule}"
        );
    }
}

/// Batching must not change the state one byte, whatever the batch size, and
/// whatever mixture of empty and spending blocks the chain carries.
#[test]
fn every_batch_size_leaves_the_same_state() {
    let source = ChainReader::new(mixed_chain().export());
    let (reference_top, reference, _) = run(&source, 1);
    let reference_top = reference_top.expect("the unbatched run replays");
    for batch_blocks in [2u32, 3, 7, 16, 64, 1_000, 100_000] {
        let (top, records, _) = run(&source, batch_blocks);
        assert_eq!(top.as_ref().unwrap(), &reference_top, "batch of {batch_blocks}");
        assert_eq!(records, reference, "batch of {batch_blocks} left different state");
    }
}

/// The per-block cross-check against the source records still runs for every
/// block, batched or not: break one `6` record and the replay must stop at
/// exactly that block either way.
#[test]
fn the_per_block_cross_check_still_runs_for_every_block() {
    use wrkz_storage::codec::{self, KeyPart};
    use wrkz_storage::records::CachedBlockInfo;

    let chain = mixed_chain();
    let broken_at = TIP + 12;
    let mut db = chain.export();
    let key = codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(broken_at));
    let mut info = CachedBlockInfo::decode(&db.get(&key).unwrap().unwrap()).unwrap();
    info.already_generated_coins += 1;
    db.put(key, info.encode()).unwrap();
    let source = ChainReader::new(db);

    for batch_blocks in [1u32, 5, 1_000] {
        let (outcome, records, _) = run(&source, batch_blocks);
        let e = outcome.expect_err("the mismatch must stop the run");
        assert!(e.starts_with(&format!("block {broken_at}")), "batch of {batch_blocks}: {e}");
        assert!(e.contains("already-generated coins"), "batch of {batch_blocks}: {e}");
        // Everything applied before the failure is committed, so a fixed build
        // resumes just below it rather than starting again from the seed.
        let tip = records.get(&wrkz_chain::keys::meta(wrkz_chain::keys::META_TIP)).expect("a tip record");
        let tip = u32::from_le_bytes(tip[..4].try_into().unwrap());
        assert_eq!(tip, broken_at, "batch of {batch_blocks}: the failing block is applied, and no more");
    }
}

// ---------------------------------------------------------------------------
// interruption and resume
// ---------------------------------------------------------------------------

/// An interrupted run resumes correctly and ends byte-identical to one that was
/// never interrupted.
///
/// The interruption is the real thing, not a clean stop: the run is stopped
/// mid-batch and the accumulated writes are then **thrown away**
/// (`abandon_pending`), which is what a process that died would leave behind.
/// The state on disk is whatever the last flush put there, and because the
/// resume height flushes inside the same atomic batch as the records it
/// describes, that state is a whole number of blocks.
#[test]
fn an_interrupted_batch_resumes_and_ends_byte_identical() {
    let chain = mixed_chain();
    let top = chain.tip();
    let source = ChainReader::new(chain.export());

    let (uninterrupted, reference, _) = run(&source, 16);
    assert_eq!(uninterrupted.unwrap(), top);

    // Stop after the 20th block — inside the second batch of 16, so the first
    // batch is committed and the four blocks above it are not.
    let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let mut chain_state = chainbuild::open_state(import_store(16));
    let mut seen = 0u32;
    let report = replay(
        &source,
        &mut chain_state,
        &ReplayOptions { progress: 1, stop: Some(stop), ..Default::default() },
        &mut |line: &str| {
            // The progress line is logged after the block is applied and after
            // its batch boundary, so raising the flag here stops the run at the
            // top of the next block.
            if line.starts_with(&format!("{}/", TIP + 20)) {
                seen += 1;
                stop.store(true, Ordering::SeqCst);
            }
        },
    )
    .expect("a stopped run is not a failure");
    assert_eq!(seen, 1, "the trigger line must have been logged exactly once");
    assert!(report.stopped, "the report must say it was interrupted");
    assert_eq!(report.top, TIP + 20, "twenty blocks were applied");

    // The crash: everything the current batch is holding is lost. `replay`
    // already synced, so this is belt and braces — but it is what proves the
    // resume works from a *batch boundary* rather than from wherever the
    // process happened to be.
    let mut store = chain_state.into_store();
    store.abandon_pending();
    let after_crash = store.base().inner().map.clone();
    let resume_height = tip_of(&after_crash);
    assert!(resume_height >= TIP + 16, "the first full batch survived: {resume_height}");
    assert!(resume_height <= TIP + 20, "and nothing above what was applied did: {resume_height}");

    // Resume on the same store and run to the top.
    let mut chain_state =
        ChainState::open(store, chainbuild::config(), Checkpoints::mainnet()).expect("the crashed state reopens");
    chain_state.set_clock(Some(chainbuild::NOW));
    assert_eq!(chain_state.tip_index(), Some(resume_height), "resumes from the committed height");
    let report =
        replay(&source, &mut chain_state, &ReplayOptions { progress: 1_000_000, ..Default::default() }, &mut quiet())
            .expect("the resumed run completes");
    assert_eq!(report.top, top);
    let resumed = chain_state.into_store().base().inner().map.clone();

    assert_eq!(resumed, reference, "an interrupted-and-resumed run must be byte-identical to an uninterrupted one");
}

/// A crash at every possible point of a batch, not only one.
///
/// For each height, the run is stopped there, the pending batch is discarded,
/// the state is reopened and the replay is finished. Every one of them has to
/// land on the same records as a single clean run — which is the property that
/// makes "the resume height flushes with the batch" worth anything.
#[test]
fn a_crash_at_any_height_resumes_to_the_same_state() {
    let chain = mixed_chain();
    let top = chain.tip();
    let source = ChainReader::new(chain.export());
    let (_, reference, _) = run(&source, 8);

    for stop_after in [1u32, 4, 8, 9, 15, 16, 23] {
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let mut state = chainbuild::open_state(import_store(8));
        replay(
            &source,
            &mut state,
            &ReplayOptions { progress: 1, stop: Some(stop), ..Default::default() },
            &mut |line: &str| {
                if line.starts_with(&format!("{}/", TIP + stop_after)) {
                    stop.store(true, Ordering::SeqCst);
                }
            },
        )
        .expect("a stopped run is not a failure");

        let mut store = state.into_store();
        store.abandon_pending();
        let height = tip_of(&store.base().inner().map);
        assert!(height <= TIP + stop_after, "stop after {stop_after}: resumed above what was applied");

        let mut state =
            ChainState::open(store, chainbuild::config(), Checkpoints::mainnet()).expect("the crashed state reopens");
        state.set_clock(Some(chainbuild::NOW));
        let report =
            replay(&source, &mut state, &ReplayOptions { progress: 1_000_000, ..Default::default() }, &mut quiet())
                .expect("the resumed run completes");
        assert_eq!(report.top, top, "stop after {stop_after}");
        assert_eq!(
            state.into_store().base().inner().map,
            reference,
            "stop after {stop_after}: the resumed state differs"
        );
    }
}

// ---------------------------------------------------------------------------
// what batching actually saves
// ---------------------------------------------------------------------------

/// The point of the change, as a number: batching divides the engine batches by
/// the batch size, and the source read-ahead divides the source reads by its
/// window.
#[test]
fn batching_divides_the_engine_batches_it_asks_for() {
    let chain = mixed_chain();
    let blocks = (chain.tip() - TIP) as u64;
    let source = ChainReader::new(chain.export());

    let (_, _, unbatched) = run(&source, 1);
    let (_, _, batched) = run(&source, 1_000);

    // One batch per block, plus the mode tag and the final flush.
    assert!(unbatched.batches >= blocks, "{unbatched:?} for {blocks} blocks");
    assert!(batched.batches <= 3, "a batch of 1,000 over {blocks} blocks should commit once or twice: {batched:?}");
    // Fewer entries as well as fewer batches: a key written by several blocks of
    // one batch — the tip record, the schema record, a per-amount counter —
    // reaches the engine once, carrying its last value. Coalescing is only ever
    // a saving, because within one batch the last write to a key is the value
    // the state would have ended at anyway.
    assert!(
        batched.ops < unbatched.ops,
        "batching must coalesce the keys a run of blocks rewrites: {batched:?} vs {unbatched:?}"
    );
    assert_eq!(
        unbatched.ops - batched.ops,
        3 * (blocks - 1),
        "exactly the tip, schema and coinbase-amount counter records of every block but the last"
    );

    // And the reads the state itself does are answered by the overlay instead
    // of the engine.
    assert!(
        batched.gets + batched.multi_get_keys < unbatched.gets + unbatched.multi_get_keys,
        "batched {batched:?} vs unbatched {unbatched:?}"
    );
}

/// The source read-ahead must fetch the same records, in one batched read per
/// window instead of three point lookups per block.
#[test]
fn the_source_read_ahead_batches_its_lookups() {
    let chain = mixed_chain();
    let blocks = (chain.tip() - TIP) as u64;
    let counted = CountingStore::new(chain.export());
    let source = ChainReader::new(counted);

    let mut state = chainbuild::open_state(import_store(1_000));
    let report =
        replay(&source, &mut state, &ReplayOptions { progress: 1_000_000, ..Default::default() }, &mut quiet())
            .expect("replays");
    let counts = source.store.counts();

    assert_eq!(report.top, chain.tip());
    // Three point lookups per block would be `3 * blocks` gets. The read-ahead
    // turns them into three batched reads per window.
    assert!(counts.gets <= 4, "only the schema and top-index probes should be single gets: {counts:?}");
    assert!(counts.multi_gets < blocks, "{blocks} blocks must cost fewer than {blocks} batched reads: {counts:?}");
    assert_eq!(report.cost.source_reads, counts.multi_gets, "the report's count must be the engine's");
}

// ---------------------------------------------------------------------------
// the measurement
// ---------------------------------------------------------------------------

/// Where a replayed block's time goes, at several batch sizes.
///
/// Not a test of speed — it asserts only that every batch size agrees on the
/// state. Run it with
///
/// ```text
/// cargo test --release -p wrkz-chain --test import_batching -- --ignored --nocapture
/// ```
///
/// On `MemStore` a commit is a `BTreeMap` insert, so the *time* it reports is
/// not the operator's time; what carries over to RocksDB is the engine-operation
/// count on the second table, which is a property of the code rather than of the
/// machine.
#[test]
#[ignore = "a measurement, not a test; run with --release --ignored --nocapture"]
fn where_an_imported_block_spends_its_time() {
    let chain = long_empty_chain(3_000);
    let blocks = (chain.tip() - TIP) as u64;

    // The operator's regime for the first 30,000 blocks: inside the checkpoint
    // zone, where `Core::addBlock` skips the proof of work and
    // `ValidateTransaction` skips the expensive input checks. That is what makes
    // those blocks cost tens of microseconds rather than a CryptoNight hash.
    measure("checkpoint zone ON (the first 4,188,000 blocks)", &chain, blocks, chainbuild::checkpoint_zone());
    // And above the zone, where the proof of work really runs, so that the two
    // can be told apart.
    measure("checkpoint zone OFF (above 4,188,000)", &chain, blocks, Checkpoints::mainnet());
}

/// One table: every batch size and both read-ahead settings, against one chain.
fn measure(what: &str, chain: &Chain, blocks: u64, checkpoints: Checkpoints) {
    println!(
        "
== {blocks} empty blocks into a MemStore — {what} ==
"
    );
    println!(
        "{:>8} {:>10} {:>10} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "batch", "read-ahead", "blocks/s", "read us", "decode", "valid", "commit", "check"
    );
    let mut reference: Option<BTreeMap<Vec<u8>, Vec<u8>>> = None;
    let mut rows = Vec::new();
    // The first row is the code as it stood before this change: one engine
    // batch per block, one point lookup per source record.
    for (batch_blocks, read_ahead) in
        [(1u32, false), (1, true), (10, true), (100, true), (1_000, true), (10_000, true), (1_000, false)]
    {
        let counted = ChainReader::new(CountingStore::new(chain.export()));
        let mut state = chainbuild::open_state_with(import_store(batch_blocks), checkpoints.clone());
        let report = replay(
            &counted,
            &mut state,
            &ReplayOptions { progress: 1_000_000, read_ahead, ..Default::default() },
            &mut quiet(),
        )
        .expect("replays");
        let c = report.cost;
        let source_counts = counted.store.counts();
        let store = state.into_store();
        let counts = store.base().counts();
        let records = store.base().inner().map.clone();
        match &reference {
            None => reference = Some(records),
            Some(r) => assert_eq!(&records, r, "batch of {batch_blocks} left different state"),
        }
        println!(
            "{batch_blocks:>8} {:>10} {:>10.0} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1}",
            read_ahead,
            c.blocks as f64 / c.elapsed.as_secs_f64().max(1e-9),
            c.micros(c.source_read),
            c.micros(c.chain.decode),
            c.micros(c.chain.validate),
            c.micros(c.chain.commit),
            c.micros(c.source_check),
        );
        rows.push((batch_blocks, read_ahead, counts, source_counts, c));
    }

    println!(
        "
{:>8} {:>10} {:>10} {:>10} {:>12} {:>11} {:>13}",
        "batch", "read-ahead", "st.batches", "st.ops", "st.bytes", "st.reads", "src.calls"
    );
    for (batch_blocks, read_ahead, counts, source, _) in &rows {
        println!(
            "{batch_blocks:>8} {read_ahead:>10} {:>10} {:>10} {:>12} {:>11} {:>13}",
            counts.batches,
            counts.ops,
            counts.written_bytes,
            counts.gets + counts.multi_get_keys,
            source.gets + source.multi_gets,
        );
    }
    println!(
        "
per block, before (batch 1, no read-ahead): {}",
        per_block(&rows[0].2, &rows[0].3, blocks)
    );
    println!("per block, after  (batch 1000, read-ahead): {}", per_block(&rows[4].2, &rows[4].3, blocks));
    println!(
        "
before: {}",
        rows[0].4.line()
    );
    println!("        {}", rows[0].4.validate_line());
    println!("        {}", rows[0].4.window_line());
    println!("after:  {}", rows[4].4.line());
    println!("        {}", rows[4].4.validate_line());
    println!(
        "        {}
",
        rows[4].4.window_line()
    );
}

fn per_block(state: &Counts, source: &Counts, blocks: u64) -> String {
    let n = blocks.max(1) as f64;
    format!(
        "{:.3} state batches, {:.1} write ops, {:.0} written bytes, {:.2} state reads, {:.3} source engine calls",
        state.batches as f64 / n,
        state.ops as f64 / n,
        state.written_bytes as f64 / n,
        (state.gets + state.multi_get_keys) as f64 / n,
        (source.gets + source.multi_gets) as f64 / n,
    )
}

// ---------------------------------------------------------------------------
// where the validate phase goes
// ---------------------------------------------------------------------------

/// The operator's import, at the two shapes it actually has, with `validate`
/// broken into the steps of `Core::addBlock`.
///
/// Run it with
///
/// ```text
/// cargo test --release -p wrkz-chain --test import_batching -- --ignored --nocapture where_validate
/// ```
///
/// Both halves go through the real [`ChainState::add_block`] with a
/// 1,000-block [`BatchStore`] active and the checkpoint zone **on**, which is
/// the operator's regime, and both report [`ReplayCost::validate_line`] and
/// [`ReplayCost::curve_line`] — the same lines a running import prints.
///
/// The two shapes:
///
/// * **empty blocks with a seven-output coinbase.** That is what the first
///   150,000 blocks of this chain are: the emission curve puts the reward at
///   11.56 million atomic units falling to 11.16 million, and
///   `decompose_amount_into_digits` turns that into 7 or 8 denominations, so
///   every block pays seven or eight `check_key`s and nothing else.
/// * **blocks carrying spends.** From the height where the chain gets busy,
///   every key input costs a `key_image_in_prime_subgroup` — one full ed25519
///   scalar multiplication — because that check lives in
///   `validateTransactionInputs` and the checkpoint zone does not skip it.
///
/// `threads = 1` is the sequential path and is the cost of the code before the
/// domain check and the output key check were batched: the same checks in the
/// same order, run one at a time on the calling thread.
#[test]
#[ignore = "a measurement, not a test; run with --release --ignored --nocapture"]
fn where_validate_spends_its_time() {
    const BLOCKS: u32 = 5_000;
    const COINBASE_OUTPUTS: usize = 7;
    const BUSY_BLOCKS: u32 = 200;
    const INPUTS_PER_BLOCK: u32 = 32;

    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("\n== {BLOCKS} empty blocks, {COINBASE_OUTPUTS}-output coinbase (heights 1-150,000) ==\n");
    let empty: Vec<(Vec<u8>, Vec<Vec<u8>>)> = {
        let mut chain = Chain::new();
        for _ in 0..BLOCKS {
            chain.push_empty_with_coinbase_outputs(COINBASE_OUTPUTS);
        }
        chain.built.iter().map(|b| (b.blob.clone(), b.tx_blobs.clone())).collect()
    };
    for threads in [1usize, cores] {
        apply_and_report(&empty, threads, 9);
    }

    println!("\n== {BUSY_BLOCKS} blocks of {INPUTS_PER_BLOCK} key inputs each (the busy chain) ==\n");
    let busy = busy_blocks(BUSY_BLOCKS, INPUTS_PER_BLOCK);
    for threads in [1usize, 2, 4, 8, cores] {
        apply_and_report(&busy, threads, BUSY_BLOCKS * INPUTS_PER_BLOCK + 16);
    }
    println!("\nthreads=1 is the pre-change cost: the same checks, one at a time on the calling thread.\n");
}

/// `blocks` blocks each carrying `inputs` one-input transactions, built on a
/// state seeded with one spendable output per input (a key image may be spent
/// once, so the pool has to be as large as the run).
fn busy_blocks(blocks: u32, inputs: u32) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let pool = blocks * inputs + 16;
    let mut chain = Chain::with_seeded_outputs(pool);
    let mut next = 0usize;
    for b in 0..blocks {
        let txs: Vec<_> = (0..inputs)
            .map(|k| {
                let real = chain.outputs[next];
                next += 1;
                // Any other seeded output of the same amount is a legal ring
                // member; the last one is never spent, so it always resolves.
                let decoy = chain.outputs[pool as usize - 1];
                chain.spend(&real, &decoy, &[b.to_le_bytes().as_slice(), &k.to_le_bytes()].concat())
            })
            .collect();
        chain.push(&txs);
    }
    chain.built.iter().map(|b| (b.blob.clone(), b.tx_blobs.clone())).collect()
}

/// Passes of the whole block list per thread setting. The best is reported.
///
/// A developer machine is shared with whatever else is on it, and this
/// measures single-digit microseconds against curve operations. Two passes of
/// the same 5,000 blocks on this host have differed by more than two to one on
/// phases that do no I/O at all — so the number to publish is the best pass,
/// which is the one that ran with the fewest cores stolen, not the mean, which
/// is a measurement of the rest of the machine.
const PASSES: usize = 3;

/// Apply `blocks` to a fresh seeded state through a 1,000-block batch, inside
/// the checkpoint zone, [`PASSES`] times, and print the breakdown of the pass
/// that spent the least time in `validate`.
fn apply_and_report(blocks: &[(Vec<u8>, Vec<Vec<u8>>)], threads: usize, pool: u32) {
    let mut best: Option<ReplayCost> = None;
    for _ in 0..PASSES {
        let (seed, _) = chainbuild::seed_state_store_with_outputs(pool);
        let store = BatchStore::with_limits(seed, 1_000, 1 << 30);
        let mut state = chainbuild::open_state_with(store, chainbuild::checkpoint_zone());
        state.set_validate_threads(threads);
        state.reset_timings();
        let began = std::time::Instant::now();
        for (blob, txs) in blocks {
            state.add_block(blob, txs).expect("the measurement chain applies");
            state.block_boundary().expect("a block boundary");
        }
        let cost = ReplayCost {
            blocks: blocks.len() as u64,
            elapsed: began.elapsed(),
            chain: state.timings(),
            validate_threads: threads,
            ..Default::default()
        };
        if best.as_ref().is_none_or(|b| cost.chain.validate < b.chain.validate) {
            best = Some(cost);
        }
    }
    let cost = best.expect("at least one pass");
    println!(
        "threads {threads:>3}: {:.0} blocks/s (best of {PASSES})",
        cost.blocks as f64 / cost.elapsed.as_secs_f64().max(1e-9)
    );
    println!("  {}", cost.validate_line());
    println!("  {}", cost.curve_line());
    println!("  {}", cost.window_line());
}

/// The parallel settle on its own, at every thread count, best of twenty.
///
/// The end-to-end figures above are wall clock on a shared machine and this
/// crate's phases are microseconds of pure computation, so on a dev host they
/// measure the rest of the machine as much as this code. This does not: it
/// builds the batch one real block produces, settles it repeatedly and reports
/// the **best** time, which is the one that ran without a core being taken
/// away. It is the number `--threads` actually moves.
#[test]
#[ignore = "a measurement, not a test; run with --release --ignored --nocapture"]
fn the_parallel_settle_scales() {
    use wrkz_chain::records::OutputRecord;
    use wrkz_chain::validate::{validate_transaction_deferred, BlockRingBatch, ChainAccess, TxContext, ValidatorState};
    use wrkz_primitives::Hash;

    /// A chain that answers nothing: inside the checkpoint zone the validator
    /// never asks it anything, which is the regime being measured.
    struct NoChain;
    impl ChainAccess for NoChain {
        fn key_image_spent(&self, _: &Hash, _: u64) -> wrkz_chain::Result<bool> {
            Ok(false)
        }
        fn key_output(&self, _: u64, _: u64) -> wrkz_chain::Result<Option<OutputRecord>> {
            Ok(None)
        }
        fn top_block_timestamp(&self) -> u64 {
            chainbuild::TIP_TIME
        }
        fn now(&self) -> u64 {
            chainbuild::NOW
        }
    }

    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let zone = chainbuild::checkpoint_zone();
    println!("\n== the batch settle alone, best of 20 ({cores} logical cores) ==\n");
    for inputs in [2usize, 8, 32, 128] {
        // One block's worth of one-input transactions, gathered exactly the way
        // `add_to_main` gathers them.
        let chain = Chain::with_seeded_outputs(inputs as u32 + 2);
        let txs: Vec<_> = (0..inputs)
            .map(|k| {
                let real = chain.outputs[k];
                let decoy = chain.outputs[inputs + 1];
                chain.spend(&real, &decoy, &k.to_le_bytes())
            })
            .collect();
        let ctx = TxContext {
            block_height: chainbuild::TIP as u64,
            block_median_size: 100_000,
            block_timestamp: chainbuild::TIP_TIME,
            is_pool_transaction: false,
            checkpoints: &zone,
        };
        let mut state = ValidatorState::new();
        let mut batch = BlockRingBatch::new();
        for (i, tx) in txs.iter().enumerate() {
            let blob = tx.to_bytes().expect("serializes");
            validate_transaction_deferred(tx, &blob, &mut state, &NoChain, &ctx, i, &mut batch)
                .expect("the harness builds valid transactions");
        }
        let mut line = format!("{inputs:>4} key inputs ({} checks):", batch.len());
        let mut sequential = f64::MAX;
        for threads in [1usize, 2, 4, 8, cores] {
            let mut best = f64::MAX;
            for _ in 0..20 {
                let t = std::time::Instant::now();
                assert_eq!(batch.first_invalid(threads), None, "the harness builds valid transactions");
                best = best.min(t.elapsed().as_secs_f64() * 1e6);
            }
            if threads == 1 {
                sequential = best;
            }
            line.push_str(&format!("  {threads}t {best:.0}us ({:.1}x)", sequential / best));
        }
        println!("{line}");
    }
    println!();
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A chain of empty blocks with two real spends across it: one of a coinbase
/// output created 40 blocks earlier, one of the seeded outputs.
fn mixed_chain() -> Chain {
    let mut chain = Chain::new();
    let created = chain.push_empty().coinbase_output;
    for _ in 1..COINBASE_LOCK {
        chain.push_empty();
    }
    let decoy = chain.outputs[0];
    let tx = chain.spend(&created, &decoy, b"mixed a");
    chain.push(&[tx]);
    for _ in 0..5 {
        chain.push_empty();
    }
    let tx = chain.spend(&chain.outputs[1], &chain.outputs[2], b"mixed b");
    chain.push(&[tx]);
    for _ in 0..4 {
        chain.push_empty();
    }
    chain
}

/// `n` empty blocks, for the measurement.
fn long_empty_chain(n: u32) -> Chain {
    let mut chain = Chain::new();
    for _ in 0..n {
        chain.push_empty();
    }
    chain
}

fn key_image(tx: &wrkz_primitives::tx::Transaction) -> wrkz_primitives::Hash {
    for input in &tx.prefix.inputs {
        if let wrkz_primitives::tx::Input::Key { key_image, .. } = input {
            return *key_image;
        }
    }
    panic!("no key input")
}

fn tip_of(records: &BTreeMap<Vec<u8>, Vec<u8>>) -> u32 {
    let raw = records.get(&wrkz_chain::keys::meta(wrkz_chain::keys::META_TIP)).expect("a tip record");
    u32::from_le_bytes(raw[..4].try_into().unwrap())
}
