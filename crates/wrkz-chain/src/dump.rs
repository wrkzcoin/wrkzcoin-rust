// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The blockchain dump of `--export-blockchain` and `--import-blockchain`
//! (`Core::exportBlockchain` and `Core::importBlockchain`,
//! `Core.cpp:3005-3990`).
//!
//! # The file
//!
//! A flat run of records with no magic, version, header, checksum or
//! compression. It is the C++ file byte for byte, so a dump written by either
//! daemon imports into the other:
//!
//! ```text
//! <height> 0x20 <length> 0x20 <length bytes of body> 0x20
//! ```
//!
//! `height` is the block index and `length` the body's size, both ASCII
//! decimal (`writeBlockchain`, `Core.cpp:3018`). The body is
//! `toBinaryArray(RawBlock)` (`serialize(RawBlock &)`,
//! `CryptoNoteSerialization.cpp:547-600`) through
//! `BinaryOutputStreamSerializer`, whose integers are unsigned LEB128
//! (`Common::writeVarint`, `StreamTools.cpp:280`) and whose `binary(ptr, size)`
//! writes raw bytes with no length:
//!
//! ```text
//! varint block_size   block_size bytes: the block blob
//! varint tx_count     varint tx_count again (beginArray writes the count)
//! tx_count times:     varint tx_size, tx_size bytes: the transaction blob
//! ```
//!
//! That is neither the C++ database's `RawBlock` record nor this port's
//! [`crate::keys::TAG_RAW_BLOCK`] record; all three are encodings of the same
//! two blobs. Records start at height 1: genesis is never written, since every
//! node constructs it.
//!
//! # Reading it
//!
//! The reader follows `readImportRecord` (`Core.cpp:3412`): whitespace between
//! tokens is skipped, a record at or below the state's top when the import
//! began is stepped over without being read (a seek, or a step inside the read
//! buffer), a length of 0 or above `CRYPTONOTE_MAX_BLOCK_BLOB_SIZE` is an error,
//! and exactly one byte separates the length from the body. Where it differs:
//!
//! - a token must be plain decimal digits. The C++ parses with `std::stoull`,
//!   which also takes a sign and stops at the first non-digit; its writer never
//!   produces either;
//! - a file that ends after a height but before its length is an error. The
//!   C++'s `dump >> height >> length` fails there without setting an error,
//!   and the import reports success at the wrong height;
//! - the body's second transaction count is read and not used, exactly as
//!   `BinaryInputStreamSerializer` reads it into a variable the loop never
//!   consults. A body must be consumed to its last byte, as `fromBinaryArray`
//!   requires.
//!
//! # Importing it
//!
//! [`import`] never trusts the file. Every block goes through
//! [`ChainState::add_block`], the path a peer's block takes, with the same
//! checkpoints: fast below the last checkpoint, as a peer sync is, and every
//! rule above it. The C++ without `--import-validate` instead pushes each block
//! straight into the database with no proof of work, signature or double-spend
//! check (`Core.cpp:3908`), which this port does not reproduce.
//!
//! The commit is the one a node uses: [`ChainState::block_boundary`] after
//! every block, so a batched store commits whole batches, and
//! [`ChainState::sync`] at the end whether the import finished, stopped or
//! failed — the blocks before a failure are whole and valid, and keeping them is
//! what lets a rerun resume, as the C++ keeps them (`Core.cpp:3951`).
//!
//! Memory is bounded by one record: the reader holds one body at a time, and
//! only as many bytes of it as the file actually holds.

use crate::state::RawBlockBlobs;
use crate::{ChainError, ChainState};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::constants::{CRYPTONOTE_MAX_BLOCK_BLOB_SIZE, CRYPTONOTE_MAX_BLOCK_NUMBER, CRYPTONOTE_MAX_TX_SIZE};
use wrkz_primitives::ser::{Reader, Writer};
use wrkz_storage::KvStore;

/// `--dump-file`'s default (`DaemonConfiguration.h:75`), relative to the
/// current directory.
pub const DEFAULT_DUMP_FILE: &str = "blockchain.dump";

/// The shortest chain, genesis included, an export will write
/// (`Core.cpp:3120`).
pub const MIN_EXPORT_BLOCKS: u64 = 1000;

/// A progress line every this many blocks, on import and export.
pub const PROGRESS_EVERY: u32 = 10_000;

/// The longest body a record may claim (`CRYPTONOTE_MAX_BLOCK_BLOB_SIZE`).
pub const MAX_RECORD_BYTES: u64 = CRYPTONOTE_MAX_BLOCK_BLOB_SIZE as u64;

/// The digits of `u64::MAX`: a longer token cannot be a height or a length.
const MAX_TOKEN_BYTES: usize = 20;

// ---------------------------------------------------------------------------
// the record
// ---------------------------------------------------------------------------

/// `toBinaryArray(RawBlock)`: the body of one record.
pub fn encode_body(block: &[u8], transactions: &[Vec<u8>]) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(block.len() as u64).raw(block);
    // `serializer(txCount)`, then `beginArray(txCount)` writes it again.
    w.varint(transactions.len() as u64).varint(transactions.len() as u64);
    for tx in transactions {
        w.varint(tx.len() as u64).raw(tx);
    }
    w.into_inner()
}

/// `fromBinaryArray(RawBlock)`: a body back into the block blob and its
/// transaction blobs.
pub fn decode_body(body: &[u8]) -> Result<RawBlockBlobs, String> {
    let mut r = Reader::new(body);
    let size = |n: u64| usize::try_from(n).map_err(|_| format!("a size of {n} bytes"));
    let block_size = size(r.varint().map_err(|e| e.to_string())?)?;
    let block = r.raw(block_size).map_err(|e| e.to_string())?.to_vec();
    // Every transaction takes at least the one byte of its size, so a count
    // beyond the remaining bytes cannot be honest and must not size anything.
    let count = r.count(1).map_err(|e| e.to_string())?;
    // `beginArray`'s copy of the count: read, and not used, as the C++ reads it.
    r.varint().map_err(|e| e.to_string())?;
    let mut transactions = Vec::with_capacity(count);
    for _ in 0..count {
        let tx_size = size(r.varint().map_err(|e| e.to_string())?)?;
        transactions.push(r.raw(tx_size).map_err(|e| e.to_string())?.to_vec());
    }
    r.finish().map_err(|e| e.to_string())?;
    Ok((block, transactions))
}

/// One record: `<height> <length> <body> `.
pub fn write_record<W: Write>(out: &mut W, height: u64, block: &[u8], transactions: &[Vec<u8>]) -> io::Result<()> {
    let body = encode_body(block, transactions);
    write!(out, "{height} {} ", body.len())?;
    out.write_all(&body)?;
    out.write_all(b" ")
}

/// What [`RecordReader::next`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// A record to import, with its body unparsed.
    Block { height: u64, body: Vec<u8> },
    /// A record at or below the height the caller asked to step over. Its
    /// body was not read.
    Skipped { height: u64 },
}

/// Reads records off a dump one at a time.
pub struct RecordReader<R> {
    input: R,
    /// Where the input ends, so that a seek past it is seen as the truncation
    /// it is rather than as a clean end of file.
    end: u64,
}

impl<R: BufRead + Seek> RecordReader<R> {
    /// A reader positioned where `input` is.
    pub fn new(mut input: R) -> io::Result<Self> {
        let here = input.stream_position()?;
        let end = input.seek(SeekFrom::End(0))?;
        input.seek(SeekFrom::Start(here))?;
        Ok(Self { input, end })
    }

    /// The next record, or `None` at the end of the file. A record whose
    /// height is at or below `skip_through` is stepped over.
    ///
    /// `Err` carries the message the import fails with; the C++'s wording
    /// where the C++ has the case.
    pub fn next(&mut self, skip_through: Option<u64>) -> Result<Option<Record>, String> {
        let io = |e: io::Error| format!("Blockchain import failed while reading the dump file: {e}");
        if !self.skip_space().map_err(io)? {
            return Ok(None);
        }
        let height_text = self.token().map_err(io)?;
        let has_length = self.skip_space().map_err(io)?;
        let length_text = if has_length { self.token().map_err(io)? } else { Vec::new() };
        let show = |t: &[u8]| String::from_utf8_lossy(t).into_owned();
        let (Some(height), Some(length)) = (decimal(&height_text), decimal(&length_text)) else {
            if !has_length {
                return Err(format!(
                    "Blockchain import file is invalid, it ends inside the header of a record - got \"{}\"",
                    show(&height_text)
                ));
            }
            return Err(format!(
                "Blockchain import file is invalid, could not read a block header - got \"{}\" and \"{}\"",
                show(&height_text),
                show(&length_text)
            ));
        };
        if length == 0 || length > MAX_RECORD_BYTES {
            return Err(format!(
                "Blockchain import file is invalid, the block at height {height} claims to be {length} bytes"
            ));
        }
        let ends_inside = || format!("Blockchain import file is invalid, it ends inside the block at height {height}");
        // The one byte between the length and the body (`dump.ignore()`). The
        // length token stopped at whitespace, so this is that byte, if any.
        if self.input.fill_buf().map_err(io)?.is_empty() {
            return Err(ends_inside());
        }
        self.input.consume(1);

        if skip_through.is_some_and(|top| height <= top) {
            if !self.step_over(length).map_err(io)? {
                return Err(ends_inside());
            }
            return Ok(Some(Record::Skipped { height }));
        }
        // Grown as the bytes arrive rather than sized by the claim, so a file
        // cut short costs only what it holds.
        let mut body = Vec::with_capacity(length.min(1 << 20) as usize);
        (&mut self.input).take(length).read_to_end(&mut body).map_err(io)?;
        if body.len() as u64 != length {
            return Err(ends_inside());
        }
        Ok(Some(Record::Block { height, body }))
    }

    /// Past `length` bytes; `false` when the file ends first. Inside the read
    /// buffer when the bytes are already there — a seek would throw the buffer
    /// away for every small block of a resume, which is the cost the C++ warns
    /// about (`Core.cpp:3467`) — and a bounded seek otherwise.
    fn step_over(&mut self, length: u64) -> io::Result<bool> {
        let buffered = self.input.fill_buf()?.len() as u64;
        if length <= buffered {
            self.input.consume(length as usize);
            return Ok(true);
        }
        let position = self.input.stream_position()?;
        if self.end.saturating_sub(position) < length {
            return Ok(false);
        }
        // `length` is at most `MAX_RECORD_BYTES`, far inside an `i64`.
        self.input.seek(SeekFrom::Current(length as i64))?;
        Ok(true)
    }

    /// Past whitespace; `false` at the end of the file.
    fn skip_space(&mut self) -> io::Result<bool> {
        loop {
            let buffer = self.input.fill_buf()?;
            if buffer.is_empty() {
                return Ok(false);
            }
            let spaces = buffer.iter().take_while(|b| is_space(**b)).count();
            let more = spaces == buffer.len();
            self.input.consume(spaces);
            if !more {
                return Ok(true);
            }
        }
    }

    /// Bytes up to the next whitespace, which is left in place. Stops one byte
    /// past [`MAX_TOKEN_BYTES`], which is enough to know it is not a number.
    fn token(&mut self) -> io::Result<Vec<u8>> {
        let mut token = Vec::new();
        loop {
            let buffer = self.input.fill_buf()?;
            if buffer.is_empty() {
                return Ok(token);
            }
            let run = buffer.iter().take_while(|b| !is_space(**b)).count();
            let wanted = run.min(MAX_TOKEN_BYTES + 1 - token.len());
            token.extend_from_slice(&buffer[..wanted]);
            let ended = run < buffer.len();
            self.input.consume(wanted);
            if ended || token.len() > MAX_TOKEN_BYTES {
                return Ok(token);
            }
        }
    }
}

/// `isspace` in the C locale, which is what `operator>>` skips.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// A plain decimal `u64`.
fn decimal(text: &[u8]) -> Option<u64> {
    if text.is_empty() || text.len() > MAX_TOKEN_BYTES || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// `HH:MM:SS`, UTC. The C++ prints local time; this port's log is UTC
/// throughout, and a progress line should agree with the timestamp beside it.
fn clock() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let secs = secs % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// What an export will write, settled before the file is created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportPlan {
    /// The first height written: 1, since genesis is constructed.
    pub start: u32,
    /// One past the last height written. The dump plus genesis is a chain of
    /// `end` blocks.
    pub end: u32,
    /// Said when `--max-export-blocks` asked for more than the chain holds.
    pub note: Option<String>,
}

/// The C++'s `endIndex` (`Core.cpp:3102-3115`): the chain's height, lowered to
/// `max_blocks` when that is smaller, with a note when it is larger. A
/// `max_blocks` of `Some(n)` means a chain of `n` blocks, genesis included, so
/// heights 1 to `n - 1` — the C++'s arithmetic, kept so the same option makes
/// the same file.
pub fn export_end(chain_height: u64, max_blocks: Option<u64>) -> (u64, Option<String>) {
    match max_blocks.filter(|n| *n > 0) {
        Some(n) if n < chain_height => (n, None),
        Some(n) if n > chain_height => (
            chain_height,
            Some(format!(
                "Chain is only {chain_height} blocks tall, exporting all of it rather than the {n} asked for."
            )),
        ),
        _ => (chain_height, None),
    }
}

/// The checks `Core::exportBlockchain` makes before it writes anything
/// (`Core.cpp:3094-3148`), in its order. The one before them — the file must
/// not exist — is the caller's, since this knows no file.
pub fn plan_export<S: KvStore>(chain: &ChainState<S>, max_blocks: Option<u64>) -> Result<ExportPlan, String> {
    let height = chain.tip_index().map_or(0, |t| u64::from(t) + 1);
    let (end, note) = export_end(height, max_blocks);
    if !(MIN_EXPORT_BLOCKS..=CRYPTONOTE_MAX_BLOCK_NUMBER).contains(&end) {
        return Err(format!("Top block is too low or too high, not going to create an export. endIndex: {end}"));
    }
    let lite = chain.lite_start_height();
    if lite > 1 {
        return Err(format!(
            "This is a lite node: it stores no block bodies below height {lite}, so it cannot export them. \
             Export from a node holding the whole chain."
        ));
    }
    // The first body the export needs. A pruned node, or an import made
    // without bodies, has none there; asking is cheaper than finding out a
    // million blocks in.
    if chain.raw_block(1).map_err(|e| format!("Reading block 1: {e}"))?.is_none() {
        return Err("No block body is stored at height 1, so there is nothing to export. A node that has pruned \
                    its raw blocks cannot produce a dump."
            .to_string());
    }
    Ok(ExportPlan { start: 1, end: end as u32, note })
}

/// Write heights `start..end` to `out`, one record each, from the bodies the
/// state serves ([`ChainState::raw_block`]), and report how many were written.
///
/// Stops with an error at the first height with no body, and when `stop`
/// says so; the caller deletes the partial file, as the C++ does, since a dump
/// that ends early imports cleanly up to where it ends.
pub fn export<S: KvStore, W: Write>(
    chain: &ChainState<S>,
    out: &mut W,
    start: u32,
    end: u32,
    stop: &dyn Fn() -> bool,
    log: &mut dyn FnMut(&str),
) -> Result<u64, String> {
    let mut written = 0u64;
    for height in start..end {
        if (height - start).is_multiple_of(PROGRESS_EVERY) {
            log(&format!("Progress [{height} / {end}] @ Time [{}]", clock()));
        }
        if stop() {
            return Err(format!("Interrupted before the block at height {height}"));
        }
        let (block, transactions) =
            chain.raw_block(height).map_err(|e| format!("Reading block {height}: {e}"))?.ok_or_else(|| {
                format!(
                    "No block body is stored at height {height}. A lite node, or a node that has pruned its raw \
                 blocks, cannot export the region it did not keep - export from a node holding the whole chain."
                )
            })?;
        write_record(out, u64::from(height), &block, &transactions)
            .map_err(|e| format!("Failed writing block {height} to the dump file: {e}"))?;
        written += 1;
    }
    log(&format!("Progress [{end} / {end}] @ Time [{}]", clock()));
    Ok(written)
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// What an import did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Blocks applied.
    pub imported: u64,
    /// Records at or below the state's top when the import began, stepped
    /// over.
    pub skipped: u64,
    /// The state's top block index when the import ended.
    pub top: u32,
    /// Whether `stop` ended it rather than the end of the file.
    pub stopped: bool,
}

/// Why an import stopped short, and what it had done by then — which is on
/// disk: the blocks before the failure are committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportFailure {
    pub message: String,
    pub report: ImportReport,
}

/// Apply the records of a dump to `chain`, each through
/// [`ChainState::add_block`]. See the module documentation for what is and is
/// not trusted, and for how the state is committed.
///
/// Records at or below the state's top when the import begins are skipped, so
/// an import resumes where an earlier one — or a sync — stopped. The first
/// record applied must be the block right above that top and must name it as
/// its parent.
pub fn import<S: KvStore, R: BufRead + Seek>(
    chain: &mut ChainState<S>,
    input: R,
    stop: &dyn Fn() -> bool,
    log: &mut dyn FnMut(&str),
) -> Result<ImportReport, ImportFailure> {
    let Some(start) = chain.tip_index() else {
        return Err(ImportFailure {
            message: "the chain state has no genesis block".to_string(),
            report: ImportReport::default(),
        });
    };
    log(&format!("Existing DB has currentIndex: {}", u64::from(start) + 1));
    let mut report = ImportReport { top: start, ..ImportReport::default() };
    let outcome = import_records(chain, input, stop, log, &mut report);
    let committed = chain.sync();
    match (outcome, committed) {
        (Err(message), _) => Err(ImportFailure { message, report }),
        (Ok(()), Err(e)) => {
            Err(ImportFailure { message: format!("Blockchain import failed writing the last blocks: {e}"), report })
        }
        (Ok(()), Ok(())) => {
            let skipped = if report.skipped > 0 {
                format!(", skipped {} the database already had", report.skipped)
            } else {
                String::new()
            };
            log(&format!("Imported {} blocks{skipped}. Completed at block {}.", report.imported, report.top));
            Ok(report)
        }
    }
}

/// The loop of [`import`], which commits whatever this returns.
fn import_records<S: KvStore, R: BufRead + Seek>(
    chain: &mut ChainState<S>,
    input: R,
    stop: &dyn Fn() -> bool,
    log: &mut dyn FnMut(&str),
    report: &mut ImportReport,
) -> Result<(), String> {
    let mut reader =
        RecordReader::new(input).map_err(|e| format!("Blockchain import failed while reading the dump file: {e}"))?;
    let skip_through = u64::from(report.top);
    // Taken from the state rather than from a skipped record, so a dump that
    // starts exactly where the state stops chains on correctly (`Core.cpp:3659`).
    let mut parent = chain.tip_info().ok_or("the chain state has no genesis block")?.block_hash;
    loop {
        if stop() {
            report.stopped = true;
            return Ok(());
        }
        let (height, body) = match reader.next(Some(skip_through))? {
            None => return Ok(()),
            Some(Record::Skipped { .. }) => {
                report.skipped += 1;
                continue;
            }
            Some(Record::Block { height, body }) => (height, body),
        };

        // `prepareBlock` (`Core.cpp:3324`): what the block says about itself.
        let (block_blob, tx_blobs) = decode_body(&body)
            .map_err(|_| format!("Blockchain import file is invalid, cannot parse the raw block at height {height}"))?;
        drop(body);
        let block = BlockTemplate::from_bytes(&block_blob).map_err(|_| {
            format!("Blockchain import file is invalid, cannot parse the block header at height {height}")
        })?;
        if let Some(tx) = tx_blobs.iter().find(|tx| tx.len() > CRYPTONOTE_MAX_TX_SIZE) {
            return Err(format!(
                "Blockchain import file is invalid, the transaction of {} bytes in the block at height {height} is \
                 larger than a transaction may be",
                tx.len()
            ));
        }

        // Then where it goes (`Core.cpp:3862`). Both are checked before the
        // block reaches the chain: `add_block` would take a block whose parent
        // is lower on the main chain as an alternative, and an import must never
        // make one.
        let top = u64::from(report.top);
        if height != top + 1 {
            return Err(format!(
                "Blockchain import file is invalid, found block height of {height} after previous block height of {top}"
            ));
        }
        if block.previous_block_hash != parent {
            return Err(format!(
                "Blockchain import file is invalid, the previous block hash of the block at height {height} does \
                 not match the hash of the block at height {top}"
            ));
        }

        let outcome = chain.add_block(&block_blob, &tx_blobs).map_err(|e| match e {
            ChainError::Rule(rule) => format!("Blockchain import file is invalid at height {height}, {rule}"),
            other => format!("Failed to import the block at height {height}: {other}"),
        })?;
        chain.block_boundary().map_err(|e| format!("Failed to import the block at height {height}: {e}"))?;
        parent = outcome.hash;
        report.top = outcome.index;
        report.imported += 1;
        if report.top.is_multiple_of(PROGRESS_EVERY) {
            log(&format!("Importing block [{}] @ Time [{}]", report.top, clock()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    /// A body laid out by hand from the C++ serializer, with a block long
    /// enough that its size takes two varint bytes.
    fn hand_built_body() -> (Vec<u8>, Vec<u8>, Vec<Vec<u8>>) {
        let block: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let transactions = vec![vec![0x01], vec![0x02, 0x03]];
        let mut body = vec![0xc8, 0x01]; // 200
        body.extend_from_slice(&block);
        body.extend_from_slice(&[0x02, 0x02]); // tx_count, and beginArray's copy
        body.extend_from_slice(&[0x01, 0x01]);
        body.extend_from_slice(&[0x02, 0x02, 0x03]);
        (body, block, transactions)
    }

    fn reader(bytes: &[u8]) -> RecordReader<Cursor<Vec<u8>>> {
        RecordReader::new(Cursor::new(bytes.to_vec())).unwrap()
    }

    #[test]
    fn a_hand_built_record_reads_and_the_writer_writes_the_same_bytes() {
        let (body, block, transactions) = hand_built_body();
        let mut record = format!("42 {} ", body.len()).into_bytes();
        record.extend_from_slice(&body);
        record.push(b' ');

        let mut r = reader(&record);
        assert_eq!(r.next(None).unwrap(), Some(Record::Block { height: 42, body: body.clone() }));
        assert_eq!(r.next(None).unwrap(), None, "the trailing space is the end of the file");
        assert_eq!(decode_body(&body).unwrap(), (block.clone(), transactions.clone()));

        assert_eq!(encode_body(&block, &transactions), body);
        let mut written = Vec::new();
        write_record(&mut written, 42, &block, &transactions).unwrap();
        assert_eq!(written, record);
    }

    #[test]
    fn the_second_transaction_count_is_read_and_not_used() {
        // `serializer(txCount)` sizes the list; `beginArray` overwrites a
        // variable nothing reads again.
        let body = [0x01, 0xaa, 0x01, 0x07, 0x01, 0xbb];
        assert_eq!(decode_body(&body).unwrap(), (vec![0xaa], vec![vec![0xbb]]));
    }

    #[test]
    fn a_body_that_is_short_long_or_lies_about_a_count_is_refused() {
        let (body, _, _) = hand_built_body();
        let mut long = body.clone();
        long.push(0);
        assert!(decode_body(&long).is_err(), "every byte must be consumed");
        assert!(decode_body(&body[..body.len() - 1]).is_err(), "a transaction cut short");
        assert!(decode_body(&[0x00, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x00]).is_err(), "a count the bytes cannot hold");
        assert!(decode_body(&[0x80, 0x00]).is_err(), "a varint with a zero continuation, as the C++ refuses");
    }

    #[test]
    fn whitespace_between_tokens_is_skipped_as_operator_extraction_skips_it() {
        let mut r = reader(b"\n\t 7   3 abc \r\n8 1 z");
        assert_eq!(r.next(None).unwrap(), Some(Record::Block { height: 7, body: b"abc".to_vec() }));
        assert_eq!(r.next(None).unwrap(), Some(Record::Block { height: 8, body: b"z".to_vec() }));
        assert_eq!(r.next(None).unwrap(), None, "a missing final space is still a clean end");
    }

    #[test]
    fn headers_the_cpp_writer_never_writes_are_refused_by_name() {
        let err = |bytes: &[u8]| reader(bytes).next(None).unwrap_err();
        assert!(err(b"x 3 abc ").contains("could not read a block header - got \"x\" and \"3\""));
        assert!(err(b"+7 3 abc ").contains("could not read a block header"), "stoull takes a sign; nothing writes one");
        assert!(err(b"7 3z abc ").contains("could not read a block header"));
        assert!(err(b"123456789012345678901234 3 abc ").contains("could not read a block header"));
        assert!(err(b"7 0 ").contains("the block at height 7 claims to be 0 bytes"));
        assert!(err(b"7 500000001 ").contains("claims to be 500000001 bytes"));
        assert!(err(b"7").contains("ends inside the header of a record - got \"7\""));
        assert!(err(b"7 3").contains("ends inside the block at height 7"));
        assert!(err(b"7 3 ab").contains("ends inside the block at height 7"));
    }

    #[test]
    fn a_skipped_record_is_stepped_over_and_one_cut_short_is_not() {
        // A buffer too small for the bodies, so the step is a seek.
        let bytes = b"1 5 aaaaa 2 5 bbbbb 3 1 c ".to_vec();
        let mut r = RecordReader::new(BufReader::with_capacity(4, Cursor::new(bytes.clone()))).unwrap();
        assert_eq!(r.next(Some(2)).unwrap(), Some(Record::Skipped { height: 1 }));
        assert_eq!(r.next(Some(2)).unwrap(), Some(Record::Skipped { height: 2 }));
        assert_eq!(r.next(Some(2)).unwrap(), Some(Record::Block { height: 3, body: b"c".to_vec() }));
        assert_eq!(r.next(Some(2)).unwrap(), None);

        // And inside the buffer, when the bytes are already there.
        let mut r = reader(&bytes);
        assert_eq!(r.next(Some(1)).unwrap(), Some(Record::Skipped { height: 1 }));
        assert_eq!(r.next(Some(1)).unwrap(), Some(Record::Block { height: 2, body: b"bbbbb".to_vec() }));

        // A truncated record is an error even when it would have been skipped:
        // a seek past the end is not the end of the file.
        let cut = b"1 5 aaaaa 2 50 bbbbb".to_vec();
        let mut r = RecordReader::new(BufReader::with_capacity(4, Cursor::new(cut.clone()))).unwrap();
        assert_eq!(r.next(Some(5)).unwrap(), Some(Record::Skipped { height: 1 }));
        assert!(r.next(Some(5)).unwrap_err().contains("ends inside the block at height 2"));
        let mut r = reader(&cut);
        r.next(Some(5)).unwrap();
        assert!(r.next(Some(5)).unwrap_err().contains("ends inside the block at height 2"));
    }

    #[test]
    fn the_export_end_is_the_cpp_end_index() {
        assert_eq!(export_end(5000, None), (5000, None));
        assert_eq!(export_end(5000, Some(3000)), (3000, None), "a ceiling below the chain lowers the end");
        assert_eq!(export_end(5000, Some(5000)), (5000, None));
        let (end, note) = export_end(5000, Some(9000));
        assert_eq!(end, 5000, "a ceiling above the chain exports all of it");
        assert_eq!(
            note.as_deref(),
            Some("Chain is only 5000 blocks tall, exporting all of it rather than the 9000 asked for.")
        );
    }
}
