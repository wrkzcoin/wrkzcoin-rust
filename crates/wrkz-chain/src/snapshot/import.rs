// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `--import-lite-snapshot`: `LiteSnapshotImport::importSnapshot`
//! (`LiteSnapshotImporter.cpp:369-708`), into this crate's state.
//!
//! # What is written
//!
//! The C++ ingests the file's `7` and `j` records as SST files and writes the
//! `6` records through `insertCachedBlock`. This port has its own key
//! namespace ([`crate::keys`]), so every record is transcoded instead and
//! written through the store the caller opened, batch by batch — for the daemon
//! that is its [`wrkz_storage::batch::BatchStore`], so memory is bounded by its
//! byte limit and not by the file:
//!
//! | file | written as |
//! | --- | --- |
//! | `6` block info, index ≠ 0 | `W b` block info and `W h` hash → index |
//! | `7` key image | `W k` key image → spending block |
//! | `j` key output, block ≠ 0 | `W o` output record, `transaction_hash` zero as the file has it |
//! | derived from `j` | `W c` per-amount count |
//! | the header | `W M tip` = `H - 1`, and the tag [`crate::keys::TAG_LITE_SNAPSHOT`] |
//!
//! Genesis is left exactly as the state's own construction wrote it — raw
//! block, coinbase, transaction index and real transaction hashes — as the C++
//! leaves it (`LiteSnapshotImporter.cpp:578`); the file's block-0 records are
//! checked against it instead of written.
//!
//! # Refusals
//!
//! In the C++'s order and words, before the file is read twice: a database
//! holding more than genesis, another chain's genesis, another height, a digest
//! not in the list the caller passes. Then the verifying pass, which writes
//! nothing: every record of a table a snapshot may carry, the digest the header
//! claims, exactly `H` distinct block infos all below `H`, monotonic cumulative
//! difficulty, coins and transaction count, every checkpoint below `H`, and the
//! header's counts.
//!
//! # Where this is stricter than the C++, and why
//!
//! Every extra check below is one a file the C++ exporter wrote always passes,
//! and each is done in the verifying pass, so none of them can leave a
//! half-written database behind:
//!
//! - **the distinct-amount count is checked before anything is written.** The
//!   C++ counts amounts while it ingests and compares after, which is why its
//!   message ends "the database is now part written";
//! - **each amount's outputs must be the contiguous global indexes `0 .. n`**.
//!   The C++ derives `n` from the record count and trusts the rest; a gap would
//!   make every output this node writes afterwards land at a wrong index;
//! - **a key image spent, or an output created, at or above `H`** is refused:
//!   the export filters both out, so either means the file describes some
//!   other region;
//! - **block 0 and its outputs must be this node's genesis**, record for
//!   record (the transaction hash aside, which the file zeroes);
//! - **an interrupted import is marked**: the state is tagged
//!   [`crate::keys::TAG_LITE_SNAPSHOT_IMPORTING`] before the first record is
//!   written, so a crash mid-import leaves a directory the daemon refuses to
//!   serve and a second import refuses to finish, instead of one that looks
//!   like a chain at genesis with key images already spent.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use wrkz_primitives::Hash;
use wrkz_storage::KvStore;

use super::container::{self, is_blessed, BlessedDigest, Header, Reader, SnapshotResult};
use super::records::{self, SnapshotRecord};
use crate::checkpoints::Checkpoints;
use crate::records::{BlockInfo, OutputRecord};
use crate::validate::ChainAccess;
use crate::{keys, ChainState};

/// `BLOCKS_PER_BATCH` (`LiteSnapshotImporter.cpp:98`): block infos per write.
pub const BLOCKS_PER_BATCH: usize = 20_000;
/// Key images and outputs per write. Each is well under a hundred bytes, so a
/// batch is a few megabytes; the store's own limits decide when it commits.
pub const RECORDS_PER_BATCH: usize = 50_000;
/// A progress line every this many records, as the C++ emits them.
pub const PROGRESS_EVERY: u64 = 2_000_000;

/// What an import reports while it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportEvent {
    /// A line for the log.
    Info(String),
    /// `emitProgress`: `phase` is `verify`, `blocks`, `write` or `done`.
    Progress { phase: &'static str, done: u64, total: u64 },
}

/// `WRKZ-IMPORT {"phase":…,"done":…,"total":…,"percent":…}`, the line a
/// supervising process parses (`LiteSnapshotImporter.cpp:140-154`).
pub fn progress_line(phase: &str, done: u64, total: u64) -> String {
    let percent = if total > 0 { 100.0 * done as f64 / total as f64 } else { 0.0 };
    format!("WRKZ-IMPORT {{\"phase\":\"{phase}\",\"done\":{done},\"total\":{total},\"percent\":{percent:.1}}}")
}

/// What a finished import did.
#[derive(Clone, Debug)]
pub struct ImportReport {
    pub header: Header,
    /// Distinct output amounts, and so `W c` records written.
    pub amounts: u64,
    pub verify_time: Duration,
    pub write_time: Duration,
}

/// Everything the verifying pass learns that the writing pass needs.
struct Audit {
    block_info_records: u64,
    key_image_records: u64,
    key_output_records: u64,
    transactions_count: u64,
    /// Per-amount output counts, ascending by amount.
    amounts: Vec<(u64, u32)>,
}

/// Import the snapshot at `path` into `chain`, which must be a lite state at
/// `lite_height` holding nothing but genesis, and whose digest must be in
/// `blessed`. `checkpoints` are the ones every block info below `H` is checked
/// against — the compiled-in table, for the daemon, as in the C++.
///
/// On an error from the verifying pass nothing has been written. On one from
/// the writing pass the message says the database must be deleted.
pub fn import_snapshot<S: KvStore>(
    chain: &mut ChainState<S>,
    path: &Path,
    lite_height: u32,
    blessed: &[BlessedDigest],
    checkpoints: &Checkpoints,
    events: &mut dyn FnMut(ImportEvent),
) -> SnapshotResult<ImportReport> {
    let storage = |e: crate::ChainError| format!("Could not read the database: {e}");
    if lite_height == 0 || chain.lite_start_height() != lite_height {
        return Err("--import-lite-snapshot needs --lite and the --lite-height the snapshot was made at. A snapshot \
             only describes the region below a lite height, so there is nothing to import it into without one."
            .into());
    }
    match chain.tag().map_err(storage)?.as_deref() {
        None => {}
        Some(keys::TAG_LITE_SNAPSHOT_IMPORTING) => {
            return Err("An earlier snapshot import into this database stopped part of the way through, so it holds \
                 part of a snapshot. Nothing has been touched. Delete the data directory and import into a new one."
                .into())
        }
        Some(tag) => {
            return Err(format!(
                "This database was written by `{tag}`, and a snapshot can only be imported into an empty one - \
                 nothing has been touched. Use a new data directory."
            ))
        }
    }
    let top = chain.tip_index().unwrap_or(0);
    if top != 0 {
        return Err(format!(
            "This database already holds a chain up to block {top}. A snapshot can only be imported into an empty \
             one - nothing has been touched. Use a new data directory, or --resync to rebuild this one."
        ));
    }

    let header = container::read_header(path)?;
    let genesis = chain.block_info(0).map_err(storage)?.ok_or("This database has no genesis block to import onto.")?;
    if header.genesis_hash != genesis.block_hash {
        return Err(format!(
            "That snapshot is for a chain whose genesis block is {}, and this daemon's is {}.",
            hex::encode(header.genesis_hash),
            hex::encode(genesis.block_hash)
        ));
    }
    if header.lite_height != lite_height {
        return Err(format!(
            "That snapshot describes the chain below height {}, and this daemon was started with --lite-height \
             {lite_height}. Restart with --lite-height {} to use it.",
            header.lite_height, header.lite_height
        ));
    }
    if !is_blessed(blessed, header.lite_height, &header.payload_digest) {
        return Err(format!(
            "This build does not recognise that snapshot. Its digest is {} at height {}, and no digest for that \
             height is compiled into this daemon.\nEverything a snapshot carries below its height is taken on trust \
             - it cannot be checked without the block bodies you do not have - so an unrecognised one is refused. \
             There is no flag to override this. Use a published snapshot, or sync the chain normally.",
            hex::encode(header.payload_digest),
            header.lite_height
        ));
    }

    events(ImportEvent::Info(format!(
        "Importing a lite node snapshot at height {} from {}",
        header.lite_height,
        path.display()
    )));
    events(ImportEvent::Info(format!(
        "Records: {}, digest {}",
        header.total_records(),
        hex::encode(header.payload_digest)
    )));
    events(ImportEvent::Info("Checking the snapshot before writing any of it...".into()));

    let verify_started = Instant::now();
    let audit = verify(chain, path, &header, &genesis, checkpoints, events)?;
    let verify_time = verify_started.elapsed();
    events(ImportEvent::Info(format!("Snapshot verified in {}s. Writing it now.", verify_time.as_secs())));

    let write_started = Instant::now();
    let reader = container::open(path)?;
    if *reader.header() != header {
        return Err("The snapshot's header changed between the two reads. The file changed underneath the import. \
             Nothing has been written."
            .into());
    }
    write(chain, reader, &header, &audit, events).map_err(|e| {
        format!(
            "{e} This database now holds part of a snapshot and is marked as such: delete the data directory \
             before trying again."
        )
    })?;
    let write_time = write_started.elapsed();

    events(ImportEvent::Progress { phase: "done", done: header.total_records(), total: header.total_records() });
    events(ImportEvent::Info(format!("Imported {} records in {}s.", header.total_records(), write_time.as_secs())));
    events(ImportEvent::Info(format!(
        "This node now holds the chain below height {} and will sync the rest from the network.",
        header.lite_height
    )));
    Ok(ImportReport { header, amounts: audit.amounts.len() as u64, verify_time, write_time })
}

/// Pass one. Reads every record, writes nothing.
fn verify<S: KvStore>(
    chain: &ChainState<S>,
    path: &Path,
    header: &Header,
    genesis: &BlockInfo,
    checkpoints: &Checkpoints,
    events: &mut dyn FnMut(ImportEvent),
) -> SnapshotResult<Audit> {
    let h = header.lite_height;
    let total = header.total_records();
    let wanted: HashMap<u32, Hash> = checkpoints.iter().filter(|(i, _)| *i < h).map(|(i, hash)| (i, *hash)).collect();
    let mut reader = container::open(path)?;

    // `BlockInfoAudit`: flat and sized up front. The three totals are what the
    // monotonic checks need once every block has been seen, because the
    // records arrive in the key's byte order, not in block order.
    let mut seen = vec![0u64; (h as usize).div_ceil(64)];
    let mut totals: Vec<[u64; 3]> = vec![[0; 3]; h as usize];
    let mut blocks_seen = 0u64;
    let mut amounts: HashMap<u64, (u64, u32)> = HashMap::new();
    let mut audit = Audit {
        block_info_records: 0,
        key_image_records: 0,
        key_output_records: 0,
        transactions_count: 0,
        amounts: Vec::new(),
    };
    let mut records_seen = 0u64;

    while let Some((key, value)) = reader.next_record()? {
        records_seen += 1;
        if records_seen.is_multiple_of(PROGRESS_EVERY) {
            events(ImportEvent::Progress { phase: "verify", done: records_seen, total });
        }
        match records::decode(key, value)? {
            SnapshotRecord::BlockInfo { index, info } => {
                if index >= h {
                    return Err(format!(
                        "The snapshot carries block {index}, which is at or above the height it claims to stop below \
                         ({h})"
                    ));
                }
                let (word, bit) = ((index / 64) as usize, 1u64 << (index % 64));
                if seen[word] & bit != 0 {
                    return Err(format!("The snapshot carries block {index} twice"));
                }
                seen[word] |= bit;
                blocks_seen += 1;
                totals[index as usize] =
                    [info.cumulative_difficulty, info.already_generated_coins, info.already_generated_transactions];
                if let Some(expected) = wanted.get(&index) {
                    if *expected != info.block_hash {
                        return Err(format!(
                            "The snapshot's block {index} is {}, but this build's checkpoints say it must be {}. This \
                             snapshot is not for this chain.",
                            hex::encode(info.block_hash),
                            hex::encode(expected)
                        ));
                    }
                }
                if index == 0 && info != *genesis {
                    return Err(
                        "The snapshot's block 0 is not the genesis block this daemon constructs. This snapshot \
                         is not for this chain."
                            .into(),
                    );
                }
                audit.block_info_records += 1;
            }
            SnapshotRecord::KeyImage { spent_at, .. } => {
                if spent_at >= h {
                    return Err(format!(
                        "The snapshot carries a key image spent in block {spent_at}, at or above the height it \
                         claims to stop below ({h})"
                    ));
                }
                audit.key_image_records += 1;
            }
            SnapshotRecord::KeyOutput { amount, global_index, output } => {
                if output.block_index >= h {
                    return Err(format!(
                        "The snapshot carries an output created in block {}, at or above the height it claims to \
                         stop below ({h})",
                        output.block_index
                    ));
                }
                if output.block_index == 0 {
                    check_genesis_output(chain, amount, global_index, &output)?;
                }
                let tally = amounts.entry(amount).or_insert((0, 0));
                tally.0 += 1;
                tally.1 = tally.1.max(global_index);
                audit.key_output_records += 1;
            }
        }
    }

    if reader.computed_digest() != header.payload_digest {
        return Err(format!(
            "The snapshot's contents hash to {} but its header claims {}. The file is damaged or has been tampered \
             with. Nothing has been written.",
            hex::encode(reader.computed_digest()),
            hex::encode(header.payload_digest)
        ));
    }

    // `BlockInfoAudit::finish`.
    if blocks_seen != u64::from(h) {
        return Err(format!(
            "The snapshot carries {blocks_seen} blocks and needs all {h} below its height. It is incomplete."
        ));
    }
    // All three are running totals over the chain, so none may ever go
    // backwards: a snapshot that mints coins or rewrites the difficulty
    // schedule fails here.
    let mut previous = [0u64; 3];
    for (index, entry) in totals.iter().enumerate() {
        for (field, what) in
            ["cumulative difficulty falls", "generated coins fall", "transaction count falls"].iter().enumerate()
        {
            if entry[field] < previous[field] {
                return Err(format!("The snapshot's {what} at block {index}"));
            }
        }
        previous = *entry;
    }
    audit.transactions_count = previous[2];
    drop(totals);
    events(ImportEvent::Progress { phase: "verify", done: total, total });

    if audit.block_info_records != header.block_info_records
        || audit.key_image_records != header.key_image_records
        || audit.key_output_records != header.key_output_records
    {
        return Err("The snapshot holds different record counts than its header claims.".into());
    }
    if audit.transactions_count != header.transactions_count {
        return Err(format!(
            "The snapshot's block info ends at {} transactions and its header claims {}.",
            audit.transactions_count, header.transactions_count
        ));
    }
    if amounts.len() as u64 != header.key_output_amounts_count {
        return Err(format!(
            "The snapshot's key outputs cover {} distinct amounts and its header claims {}. Nothing has been written.",
            amounts.len(),
            header.key_output_amounts_count
        ));
    }
    let mut counts = Vec::with_capacity(amounts.len());
    for (amount, (count, highest)) in amounts {
        if count != u64::from(highest) + 1 || count > u64::from(u32::MAX) {
            return Err(format!(
                "The snapshot's outputs of amount {amount} are not the global indexes 0 to {} without a gap, so \
                 every output this node wrote afterwards would land at the wrong index. Nothing has been written.",
                count.saturating_sub(1)
            ));
        }
        counts.push((amount, count as u32));
    }
    counts.sort_unstable();
    audit.amounts = counts;
    Ok(audit)
}

/// A snapshot's block-0 output against the one this state's own genesis wrote.
fn check_genesis_output<S: KvStore>(
    chain: &ChainState<S>,
    amount: u64,
    global_index: u32,
    theirs: &OutputRecord,
) -> SnapshotResult<()> {
    let ours =
        chain.key_output(amount, u64::from(global_index)).map_err(|e| format!("Could not read the database: {e}"))?;
    let same = ours.is_some_and(|o| {
        o.public_key == theirs.public_key
            && o.unlock_time == theirs.unlock_time
            && o.output_index == theirs.output_index
    });
    if !same {
        return Err(format!(
            "The snapshot's genesis output of amount {amount} at global index {global_index} is not the one this \
             daemon's genesis block creates. This snapshot is not for this chain."
        ));
    }
    Ok(())
}

/// Pass two. The records again, into the state.
fn write<S: KvStore, R: Read>(
    chain: &mut ChainState<S>,
    mut reader: Reader<R>,
    header: &Header,
    audit: &Audit,
    events: &mut dyn FnMut(ImportEvent),
) -> SnapshotResult<()> {
    let storage = |e: crate::ChainError| format!("Failed writing the snapshot: {e}.");

    // Marked first and committed on its own, so there is no instant at which a
    // record is on disk and the mark is not.
    chain.set_tag(keys::TAG_LITE_SNAPSHOT_IMPORTING).map_err(storage)?;
    chain.flush().map_err(storage)?;
    // The bulk-load mode `wrkz-replay` uses: a crash loses what has not reached
    // a table file, and the mark above already says the state is incomplete.
    chain.set_write_ahead_log(false).map_err(storage)?;

    let bulk_total = header.key_image_records.wrapping_add(header.key_output_records);
    let mut blocks: Vec<(u32, BlockInfo)> = Vec::with_capacity(BLOCKS_PER_BATCH);
    let mut outputs: Vec<(u64, u32, OutputRecord)> = Vec::with_capacity(RECORDS_PER_BATCH);
    let mut spent: Vec<(Hash, u32)> = Vec::with_capacity(RECORDS_PER_BATCH);
    let (mut blocks_written, mut ingested) = (0u64, 0u64);

    while let Some((key, value)) = reader.next_record()? {
        match records::decode(key, value)? {
            SnapshotRecord::BlockInfo { index, info } => {
                // Genesis is already in the state, written in full; pass one
                // checked the file's copy against it.
                if index == 0 {
                    continue;
                }
                blocks.push((index, info));
                blocks_written += 1;
                if blocks.len() >= BLOCKS_PER_BATCH {
                    chain.import_block_infos(&blocks).map_err(storage)?;
                    blocks.clear();
                    chain.block_boundary().map_err(storage)?;
                    events(ImportEvent::Info(format!(
                        "  block info: {blocks_written} / {}",
                        header.block_info_records
                    )));
                    events(ImportEvent::Progress {
                        phase: "blocks",
                        done: blocks_written,
                        total: header.block_info_records,
                    });
                }
                continue;
            }
            SnapshotRecord::KeyImage { image, spent_at } => spent.push((image, spent_at)),
            SnapshotRecord::KeyOutput { amount, global_index, output } => {
                if output.block_index != 0 {
                    outputs.push((amount, global_index, output));
                }
            }
        }
        ingested += 1;
        if spent.len() + outputs.len() >= RECORDS_PER_BATCH {
            chain.import_state(&outputs, &[], &spent).map_err(storage)?;
            outputs.clear();
            spent.clear();
            chain.block_boundary().map_err(storage)?;
        }
        if ingested.is_multiple_of(PROGRESS_EVERY) {
            if ingested.is_multiple_of(5 * PROGRESS_EVERY) {
                events(ImportEvent::Info(format!("  bulk load: {ingested} records")));
            }
            events(ImportEvent::Progress { phase: "write", done: ingested, total: bulk_total });
        }
    }
    chain.import_block_infos(&blocks).map_err(storage)?;
    chain.import_state(&outputs, &[], &spent).map_err(storage)?;

    if reader.computed_digest() != header.payload_digest {
        return Err("The snapshot hashed differently on the second read than the first. The file changed underneath \
             the import."
            .into());
    }

    // The counters a resumed sync depends on, from pass one's tally: an off by
    // one here silently corrupts the global index of every output this node
    // ever writes. Then the tip and the final tag, in one batch.
    chain.import_state(&[], &audit.amounts, &[]).map_err(storage)?;
    chain.finish_snapshot_import(header.lite_height).map_err(storage)?;
    chain.set_write_ahead_log(true).map_err(storage)?;
    chain.sync().map_err(storage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_progress_line_is_the_one_a_supervisor_parses() {
        assert_eq!(
            progress_line("verify", 2_000_000, 148_728_732),
            r#"WRKZ-IMPORT {"phase":"verify","done":2000000,"total":148728732,"percent":1.3}"#
        );
        assert_eq!(progress_line("done", 0, 0), r#"WRKZ-IMPORT {"phase":"done","done":0,"total":0,"percent":0.0}"#);
    }
}
