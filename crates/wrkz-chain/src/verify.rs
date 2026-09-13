// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Rebuild a chain state from its own block bodies and check it record for
//! record (`wrkz-verify-state`).
//!
//! A state someone hands you — a snapshot, a copy of another operator's
//! `DIR/state` — is only as trustworthy as whoever built it. Its outputs, key
//! images and per-amount counts are what a node reads to accept or reject the
//! next block, and nothing inside the state proves them. [`verify_state`]
//! re-applies every block body the state holds, genesis to its tip, through
//! this crate's own validation into a fresh state, and after each block
//! compares every record that block wrote, byte for byte, against the same key
//! in the given state:
//!
//! - the block info and the hash → index entry;
//! - the transaction list and each transaction's index entry;
//! - each output record, at the `(amount, global index)` pairs the rebuilt
//!   block created, and the given state's per-block list of them where it
//!   kept one;
//! - the block's spent key images and each key-image entry;
//! - the block's payment-id entries and its body.
//!
//! and at the end, the output count of every amount the run touched.
//!
//! Equal all the way up means the given state holds exactly what this code
//! would have written for that chain. The rebuilt state is trustworthy
//! whatever the given one was, since nothing but block bodies went into it —
//! and the bodies are themselves checked, by the validation they go through
//! and by the checkpoints. Either state can be served afterwards.
//!
//! Not compared: the payment-id lists themselves, an explorer index that grows
//! across blocks and decides nothing (the per-block entries that build them
//! are compared).

use crate::replay::{ReplayOptions, TAG_LINEAR, TAG_WINDOWS};
use crate::{keys, records, ChainState};
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::time::Instant;
use wrkz_storage::KvStore;

/// What a [`verify_state`] run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// The rebuilt state's top index.
    pub top: u32,
    /// Blocks this run applied and compared.
    pub applied: u32,
    /// The given state's top index.
    pub source_top: u32,
    /// Whether [`ReplayOptions::stop`] ended the run early. The rebuilt state
    /// is committed at a block boundary either way, and a later run resumes.
    pub stopped: bool,
}

/// Rebuild `source` into `target` and compare them; see the module
/// documentation. Resumable: `target` continues from its own tip. Of `opts`,
/// `to`, `progress`, `sync_every` and `stop` are read.
///
/// `target` must keep block bodies (`Config::store_raw_blocks`), since the
/// bodies are compared too.
pub fn verify_state<Src: KvStore, Dst: KvStore>(
    source: &ChainState<Src>,
    target: &mut ChainState<Dst>,
    opts: &ReplayOptions,
    log: &mut dyn FnMut(&str),
) -> Result<VerifyReport, String> {
    if source.tag().map_err(|e| format!("reading the given state's tag: {e}"))?.as_deref() == Some(TAG_WINDOWS) {
        return Err("the given state was written by a windowed replay and has holes in it; there is no chain \
                    in it to verify"
            .into());
    }
    let source_top = source.tip_index().ok_or("the given state has no genesis block")?;
    if source.lite_start_height() > 0 {
        return Err(format!(
            "the given state is a lite node with no block bodies below {}; it cannot be rebuilt from genesis",
            source.lite_start_height()
        ));
    }
    match target.tag().map_err(|e| format!("reading our state's tag: {e}"))?.as_deref() {
        None => target.set_tag(TAG_LINEAR).map_err(|e| format!("tagging our state: {e}"))?,
        Some(TAG_LINEAR) => {}
        Some(other) => return Err(format!("our state is tagged {other:?}; rebuild into an empty directory")),
    }
    let applied = target.tip_index().ok_or("our state has no genesis block")?;
    let start = applied + 1;
    let end = opts.to.map_or(source_top, |to| to.min(source_top));
    if start == 1 {
        // Genesis is constructed, never applied, so the loop below would never
        // compare it.
        compare_block(source, target, 0)?;
    }
    if start > end {
        target.sync().map_err(|e| format!("committing our state: {e}"))?;
        log(&format!("nothing to do: our state is at {applied}, the given state at {source_top}"));
        return Ok(VerifyReport { top: applied, applied: 0, source_top, stopped: false });
    }
    log(&format!("rebuilding {start} to {end} from the given state's own block bodies, comparing every record"));

    let mut amounts = BTreeSet::new();
    let outcome = rebuild(source, target, opts, log, start, end, &mut amounts);
    // Commit whatever the outcome, so that everything compared before a
    // failure or an interrupt survives and a later run resumes from it.
    let committed = target.sync().map_err(|e| format!("committing our state: {e}"));
    let stopped = match outcome {
        Ok(stopped) => {
            committed?;
            stopped
        }
        Err(e) => {
            if let Err(commit) = committed {
                return Err(format!("{e} (and {commit})"));
            }
            return Err(e);
        }
    };
    let top = target.tip_index().expect("at least genesis");
    // Only at the given state's own tip do the two counts describe the same
    // chain; a run cut short by `to` or an interrupt compares them next time.
    if !stopped && top == source_top {
        for amount in &amounts {
            same(source, target, top, &format!("the output count of amount {amount}"), &keys::output_count(*amount))?;
        }
    }
    if stopped {
        log(&format!("interrupted after block {top}: committed; a later run resumes from {}", top + 1));
    } else if top == source_top {
        log(&format!(
            "VERIFIED: blocks 0 to {top} rebuilt from their bodies and every record matches the given state, \
             and the output counts of {} amounts",
            amounts.len()
        ));
    }
    Ok(VerifyReport { top, applied: top.saturating_sub(applied), source_top, stopped })
}

/// The loop. Returns whether [`ReplayOptions::stop`] ended it.
fn rebuild<Src: KvStore, Dst: KvStore>(
    source: &ChainState<Src>,
    target: &mut ChainState<Dst>,
    opts: &ReplayOptions,
    log: &mut dyn FnMut(&str),
    start: u32,
    end: u32,
    amounts: &mut BTreeSet<u64>,
) -> Result<bool, String> {
    let progress = opts.progress.max(1);
    let mut window = Instant::now();
    let mut window_start = start;
    let mut since_sync = 0u32;
    for index in start..=end {
        // Between two blocks, so the batch below is whole either way.
        if opts.stop.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Ok(true);
        }
        let (block, txs) = source.raw_block(index).map_err(|e| format!("block {index}: reading its body: {e}"))?.ok_or_else(
            || format!("block {index}: the given state holds no body for it; a pruned state cannot be rebuilt from genesis"),
        )?;
        target.add_block(&block, &txs).map_err(|e| format!("block {index}: {e}"))?;
        amounts.extend(compare_block(source, target, index)?);
        target.block_boundary().map_err(|e| format!("block {index}: committing: {e}"))?;
        since_sync += 1;
        if opts.sync_every > 0 && since_sync >= opts.sync_every {
            target.sync().map_err(|e| format!("block {index}: making our state durable: {e}"))?;
            since_sync = 0;
        }
        if index % progress == 0 || index == end {
            let rate = (index - window_start + 1) as f64 / window.elapsed().as_secs_f64().max(1e-9);
            log(&format!("{index}/{end}  {rate:.0} blocks/s, every record matches"));
            window = Instant::now();
            window_start = index + 1;
        }
    }
    Ok(false)
}

/// One key in both states; the two must hold the same bytes, or neither any.
/// Returns ours.
fn same<Src: KvStore, Dst: KvStore>(
    source: &ChainState<Src>,
    target: &ChainState<Dst>,
    index: u32,
    what: &str,
    key: &[u8],
) -> Result<Option<Vec<u8>>, String> {
    let theirs =
        source.store().get(key).map_err(|e| format!("block {index}: reading {what} from the given state: {e}"))?;
    let ours = target.store().get(key).map_err(|e| format!("block {index}: reading {what} from our state: {e}"))?;
    if theirs != ours {
        return Err(format!(
            "block {index}: {what} differs — the given state holds {}, rebuilding gives {}",
            show(&theirs),
            show(&ours)
        ));
    }
    Ok(ours)
}

fn show(value: &Option<Vec<u8>>) -> String {
    match value {
        None => "nothing".to_string(),
        Some(b) if b.len() <= 24 => format!("{} bytes {}", b.len(), hex::encode(b)),
        Some(b) => format!("{} bytes {}…", b.len(), hex::encode(&b[..24])),
    }
}

/// Every record block `index` wrote, compared. Returns the amounts of its
/// outputs, whose counts are compared at the end.
fn compare_block<Src: KvStore, Dst: KvStore>(
    source: &ChainState<Src>,
    target: &ChainState<Dst>,
    index: u32,
) -> Result<Vec<u64>, String> {
    let decode = |e: crate::ChainError| format!("block {index}: {e}");
    same(source, target, index, "the block info", &keys::block_info(index))?
        .ok_or_else(|| format!("block {index}: neither state has a block info record"))?;
    let info = target.block_info(index).map_err(decode)?.ok_or_else(|| format!("block {index}: no block info"))?;
    same(source, target, index, "the hash-to-index entry", &keys::hash_to_index(&info.block_hash))?;

    if let Some(raw) = same(source, target, index, "the transaction list", &keys::block_tx_hashes(index))? {
        for hash in records::decode_hashes(&raw).map_err(|e| format!("block {index}: {e}"))? {
            let what = format!("the index entry of transaction {}", hex::encode(hash));
            same(source, target, index, &what, &keys::transaction_index(&hash))?;
        }
    }

    // The outputs the block created, as the rebuilt state recorded them: it
    // keeps every per-block list. Where the given state kept its own list,
    // the two must be equal. Where it dropped it — every `wrkz-replay` import
    // before 2026-09-10 kept only the last 512 — nothing is rebuilt on its
    // side: each output record at these positions is compared byte for byte
    // below (the record names its block, transaction and output number), and
    // the per-amount counts at the end rule out an output we never created.
    // Rebuilding the given state's list instead costs a binary search per
    // amount per block over a cold store, which held a full run under 20
    // blocks a second.
    let refs = target.block_output_refs(index).map_err(decode)?.unwrap_or_default();
    let listed = source
        .store()
        .get(&keys::block_outputs(index))
        .map_err(|e| format!("block {index}: reading the given state's output list: {e}"))?;
    if let Some(raw) = listed {
        let theirs = records::decode_output_refs(&raw).map_err(|e| format!("block {index}: {e}"))?;
        if theirs != refs {
            return Err(format!(
                "block {index}: the outputs it created differ — the given state lists {theirs:?}, rebuilding \
                 gives {refs:?}"
            ));
        }
    }
    for (amount, global_index) in &refs {
        let what = format!("the output record of amount {amount} at global index {global_index}");
        same(source, target, index, &what, &keys::output(*amount, *global_index))?;
    }

    if let Some(raw) = same(source, target, index, "the block's spent key images", &keys::block_key_images(index))? {
        for image in records::decode_hashes(&raw).map_err(|e| format!("block {index}: {e}"))? {
            let what = format!("the spent key image {}", hex::encode(image));
            same(source, target, index, &what, &keys::key_image(&image))?;
        }
    }
    same(source, target, index, "the block's payment-id entries", &keys::block_payment_ids(index))?;
    same(source, target, index, "the block body", &keys::raw_block(index))?;
    Ok(refs.into_iter().map(|(amount, _)| amount).collect())
}
