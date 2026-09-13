// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `snapshot_export`'s walk: `DatabaseBlockchainCache::walkSnapshotRecords`
//! (`DatabaseBlockchainCache.cpp:3356-3551`), over this crate's state.
//!
//! The file has to be the C++ one byte for byte, and the C++ walks its own
//! database in RocksDB key order — which for these tables is the order of
//! KV-document keys holding **little-endian** integers. This state keys the
//! same records big-endian, so the walk reproduces that order rather than
//! reading it off an iterator:
//!
//! | table | C++ key order | here |
//! | --- | --- | --- |
//! | `6` block info | index, LE bytes | every index below `H` in LE byte order, point reads in batches |
//! | `7` key images | the image's bytes | an ordered scan of `W k`, which sorts the same way |
//! | `j` key outputs | amount, then global index, LE bytes | see below |
//!
//! For `j`, the amounts of `W c` are sorted by their LE bytes; for each, the
//! count as of `H` is a binary search on the output records' block index, and
//! the global indexes below that count are read in their LE byte order.
//!
//! Each table is filtered to `[0, H)` exactly as the C++ filters it — block
//! index below `H`, key image spent below `H`, output created below `H` — and
//! every output is written with its transaction hash zeroed, so a full node, a
//! lite node that synced and a lite node imported from a snapshot all write the
//! same payload for the same chain and `H`.
//!
//! # A live node
//!
//! The C++ walks under one RocksDB snapshot. This takes the chain's read lock
//! for one batch at a time — a few thousand point reads, or one page of a
//! scan — and releases it before compressing or writing anything, so the node
//! keeps applying blocks while an export runs. That is consistent because
//! nothing below `H` can change while it does: a lite node refuses any
//! reorganisation below its line, and the daemon refuses to export a region
//! that is not at least `MIN_LITE_FULL_BLOCK_DEPTH` (20,160) blocks deep, far
//! past the 180 a reorganisation can reach. Records above `H` come and go and
//! are filtered out.

use std::io::{Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};

use wrkz_primitives::Hash;
use wrkz_storage::KvStore;

use super::container::{FileWriter, Header, SnapshotResult, Writer};
use super::records::{self, Table};
use crate::validate::ChainAccess;
use crate::{ChainError, ChainState};

/// Where the walk puts each record it keeps: a writer's `add`.
type Sink<'a> = dyn FnMut(&[u8], &[u8]) -> SnapshotResult<()> + 'a;

/// Point reads per batch under one read lock.
pub const READ_BATCH: usize = 4096;
/// Entries per scan page under one read lock.
pub const SCAN_PAGE: usize = 65_536;

/// What `snapshot_export status` prints while an export runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportProgress {
    /// The table being walked, in the C++'s names.
    pub table: String,
    pub scanned: u64,
    pub kept: u64,
}

/// Shared between an export and whoever watches it: the progress it reports and
/// the flag that stops it.
#[derive(Debug, Default)]
pub struct ExportControl {
    cancel: AtomicBool,
    progress: Mutex<ExportProgress>,
}

impl ExportControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the export to stop at its next batch. Its partial file is removed.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn progress(&self) -> ExportProgress {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Record progress, and stop if asked to (`walk`'s `progress` callback).
    fn report(&self, table: &str, scanned: u64, kept: u64) -> SnapshotResult<()> {
        {
            let mut p = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
            if p.table != table {
                p.table = table.to_string();
            }
            p.scanned = scanned;
            p.kept = kept;
        }
        if self.is_cancelled() {
            return Err("Lite snapshot export cancelled".into());
        }
        Ok(())
    }
}

/// Export the region below `height` of the chain behind `chain` to a new file
/// at `path`. The file is removed again on any failure or cancellation.
pub fn export_snapshot<S: KvStore>(
    chain: &RwLock<ChainState<S>>,
    path: &Path,
    height: u32,
    control: &ExportControl,
) -> SnapshotResult<Header> {
    let mut file = FileWriter::create(path)?;
    let header = walk(chain, height, control, &mut |k, v| file.add(k, v))?;
    file.finish(header)
}

/// [`export_snapshot`] into any seekable sink, for a caller that wants the
/// bytes rather than a file.
pub fn export_to<S: KvStore, W: Write + Seek>(
    chain: &RwLock<ChainState<S>>,
    out: W,
    height: u32,
    control: &ExportControl,
) -> SnapshotResult<(Header, W)> {
    let mut writer = Writer::new(out, "the snapshot")?;
    let header = walk(chain, height, control, &mut |k, v| writer.add(k, v))?;
    writer.finish(header)
}

/// Calls `visit` with every value in `[0, count)` in the byte order of its
/// `width`-byte little-endian encoding — the order RocksDB iterates a KV key
/// holding that integer. `count` must fit in `width` bytes.
///
/// Recursive over the bytes rather than a sort: the lowest byte is the most
/// significant for the order, so for each value of it in turn, the values
/// sharing it are the ones whose remaining bytes count up to
/// `(count - 1 - low) / 256 + 1`. No allocation, whatever `count` is.
pub fn for_each_in_le_order(
    count: u64,
    width: u32,
    visit: &mut dyn FnMut(u64) -> SnapshotResult<()>,
) -> SnapshotResult<()> {
    fn walk_bytes(
        count: u64,
        width: u32,
        base: u64,
        shift: u32,
        visit: &mut dyn FnMut(u64) -> SnapshotResult<()>,
    ) -> SnapshotResult<()> {
        if count == 0 {
            return Ok(());
        }
        if width == 1 {
            for x in 0..count {
                visit(base | (x << shift))?;
            }
            return Ok(());
        }
        for low in 0..count.min(256) {
            walk_bytes((count - 1 - low) / 256 + 1, width - 1, base | (low << shift), shift + 8, visit)?;
        }
        Ok(())
    }
    assert!((1..=8).contains(&width) && (width == 8 || count <= 1u64 << (8 * width)), "{count} does not fit");
    walk_bytes(count, width, 0, 0, visit)
}

/// The walk: each table's kept records into `sink`, in the C++'s order, and
/// the header they add up to.
fn walk<S: KvStore>(
    chain: &RwLock<ChainState<S>>,
    height: u32,
    control: &ExportControl,
    sink: &mut Sink<'_>,
) -> SnapshotResult<Header> {
    if height == 0 {
        return Err("A lite snapshot describes the chain below a height, so that height cannot be 0".into());
    }
    let read = || chain.read().unwrap_or_else(PoisonError::into_inner);
    let storage = |e: ChainError| format!("Failed reading the chain state: {e}");
    let mut header = Header { lite_height: height, ..Header::default() };
    header.genesis_hash =
        read().block_info(0).map_err(storage)?.ok_or("This node holds no genesis block to export from.")?.block_hash;

    // Block info. The key carries the height, so this is also where the
    // transaction counter as of the snapshot's top block comes from.
    let table = Table::BlockInfo.name();
    control.report(table, 0, 0)?;
    let mut kept = 0u64;
    let mut top_transactions = None;
    let mut batch: Vec<u32> = Vec::with_capacity(READ_BATCH);
    let mut emit_blocks = |batch: &mut Vec<u32>| -> SnapshotResult<()> {
        let infos = read().block_infos(batch).map_err(storage)?;
        for (index, info) in batch.iter().zip(infos) {
            let Some(info) = info else {
                return Err(format!(
                    "This node holds no block info for height {index}, and a snapshot at {height} needs all {height} \
                     blocks below it. Let it finish syncing first."
                ));
            };
            if *index == height - 1 {
                top_transactions = Some(info.already_generated_transactions);
            }
            let (key, value) = records::block_info_record(*index, &info);
            sink(&key, &value)?;
            kept += 1;
        }
        batch.clear();
        control.report(table, kept, kept)
    };
    for_each_in_le_order(u64::from(height), 4, &mut |index| {
        batch.push(index as u32);
        if batch.len() == READ_BATCH {
            emit_blocks(&mut batch)?;
        }
        Ok(())
    })?;
    emit_blocks(&mut batch)?;
    header.block_info_records = kept;
    header.transactions_count = top_transactions.expect("every index below the height was read");

    // Spent key images: `W k` sorts by image exactly as `7` does. The value is
    // the block the image was spent in, which decides whether it belongs.
    let table = Table::KeyImage.name();
    control.report(table, 0, 0)?;
    let (mut after, mut scanned, mut kept) = (None::<Hash>, 0u64, 0u64);
    loop {
        let (page, next) = read().scan_key_images(after.as_ref(), SCAN_PAGE).map_err(storage)?;
        for (image, spent_at) in page {
            scanned += 1;
            if spent_at < height {
                let (key, value) = records::key_image_record(&image, spent_at);
                sink(&key, &value)?;
                kept += 1;
            }
        }
        control.report(table, scanned, kept)?;
        match next {
            Some(key) => after = Some(key),
            None => break,
        }
    }
    header.key_image_records = kept;

    // Key output info, the bulk of the file.
    let table = Table::KeyOutput.name();
    control.report(table, 0, 0)?;
    let mut amounts = Vec::new();
    let mut after = None::<u64>;
    loop {
        let (page, next) = read().scan_output_amounts(after, SCAN_PAGE).map_err(storage)?;
        amounts.extend(page.into_iter().map(|(amount, _)| amount));
        match next {
            Some(amount) => after = Some(amount),
            None => break,
        }
    }
    amounts.sort_unstable_by_key(|amount| amount.to_le_bytes());
    let (mut scanned, mut kept) = (0u64, 0u64);
    for amount in amounts {
        // Global indexes are handed out in chain order, so the outputs below
        // the height are exactly `0 .. count` of the amount.
        let count = read().output_count_for_amount_below(amount, height).map_err(storage)?;
        if count == 0 {
            continue;
        }
        header.key_output_amounts_count += 1;
        let mut batch: Vec<u64> = Vec::with_capacity(READ_BATCH);
        let mut emit_outputs = |batch: &mut Vec<u64>| -> SnapshotResult<()> {
            let outputs = read().key_outputs(amount, batch).map_err(storage)?;
            for (global_index, output) in batch.iter().zip(outputs) {
                let Some(output) = output.filter(|o| o.block_index < height) else {
                    return Err(format!(
                        "The output of amount {amount} at global index {global_index} is missing or above height \
                         {height}, below the count this state keeps for that amount. The state is inconsistent."
                    ));
                };
                let (key, value) = records::key_output_record(amount, *global_index as u32, &output);
                sink(&key, &value)?;
                kept += 1;
            }
            scanned += batch.len() as u64;
            batch.clear();
            control.report(table, scanned, kept)
        };
        for_each_in_le_order(u64::from(count), 4, &mut |global_index| {
            batch.push(global_index);
            if batch.len() == READ_BATCH {
                emit_outputs(&mut batch)?;
            }
            Ok(())
        })?;
        emit_outputs(&mut batch)?;
    }
    header.key_output_records = kept;
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_le_order_is_the_order_of_the_little_endian_bytes() {
        for count in [0u64, 1, 2, 255, 256, 257, 511, 65_535, 65_536, 65_537, 70_001] {
            let mut got = Vec::new();
            for_each_in_le_order(count, 4, &mut |v| {
                got.push(v);
                Ok(())
            })
            .unwrap();
            let mut want: Vec<u64> = (0..count).collect();
            want.sort_by_key(|v| (*v as u32).to_le_bytes());
            assert_eq!(got, want, "count {count}");
        }
        let mut got = Vec::new();
        for_each_in_le_order(300, 8, &mut |v| {
            got.push(v);
            Ok(())
        })
        .unwrap();
        let mut want: Vec<u64> = (0..300).collect();
        want.sort_by_key(|v| v.to_le_bytes());
        assert_eq!(got, want);
    }

    #[test]
    fn a_cancelled_or_zero_height_export_writes_nothing() {
        let chain = RwLock::new(
            ChainState::open_or_genesis(
                wrkz_storage::MemStore::default(),
                crate::Config::default(),
                crate::Checkpoints::none(),
            )
            .unwrap(),
        );
        let control = ExportControl::new();
        let e = export_to(&chain, std::io::Cursor::new(Vec::new()), 0, &control).unwrap_err();
        assert!(e.contains("cannot be 0"), "{e}");

        let dir = std::env::temp_dir().join(format!("wrkz-export-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cancelled.litesnap");
        let _ = std::fs::remove_file(&path);
        control.cancel();
        assert_eq!(export_snapshot(&chain, &path, 1, &control).unwrap_err(), "Lite snapshot export cancelled");
        assert!(!path.exists(), "the partial file is removed");
        let _ = std::fs::remove_dir(&dir);

        // Above the tip, the walk says which block it does not have.
        let e = export_to(&chain, std::io::Cursor::new(Vec::new()), 2, &ExportControl::new()).unwrap_err();
        assert!(e.contains("no block info for height 1, and a snapshot at 2 needs all 2"), "{e}");

        // Genesis alone: one block info, the three genesis outputs of one amount.
        let (header, _) = export_to(&chain, std::io::Cursor::new(Vec::new()), 1, &ExportControl::new()).unwrap();
        assert_eq!(
            (header.block_info_records, header.key_image_records, header.key_output_records),
            (1, 0, 3),
            "{header:?}"
        );
        assert_eq!((header.key_output_amounts_count, header.transactions_count), (1, 1));
    }
}
