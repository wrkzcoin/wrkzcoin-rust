// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The `.litesnap` file: `src/daemon/LiteSnapshot.{h,cpp}`, byte for byte.
//!
//! ```text
//! header   128 bytes, little-endian, written last over a reserved copy
//! frame*   u32 rawLen | u32 compLen | compLen bytes (one zstd frame, level 10)
//! end      u32 0 | u32 0
//! ```
//!
//! | offset | width | field |
//! | --- | --- | --- |
//! | 0 | 8 | magic `WRKZLITE` |
//! | 8 | u32 | format version, 1 |
//! | 12 | 32 | genesis hash |
//! | 44 | u32 | lite height `H` |
//! | 48 | u64 | block info records |
//! | 56 | u64 | key image records |
//! | 64 | u64 | amount count records (the C++ export never sets it) |
//! | 72 | u64 | key output records |
//! | 80 | u64 | transactions count, `info[H-1].alreadyGeneratedTransactions` |
//! | 88 | u64 | distinct key output amounts |
//! | 96 | 32 | payload digest |
//!
//! A raw frame is a run of `LEB128(keyLen) key LEB128(valueLen) value` records,
//! keys strictly ascending across the whole file. The writer closes a frame
//! after the record that brings it to [`FRAME_PAYLOAD_TARGET`] bytes or more,
//! so frame boundaries are a function of the record stream alone; the digest
//! chains over the **raw** frames,
//! `running = cn_fast_hash(running ‖ cn_fast_hash(frame))` from 32 zero bytes,
//! and so depends on that rule and on nothing about zstd. Two builds on
//! different zstd versions may write different files for one chain; they write
//! the same digest.
//!
//! # Where this reads more strictly than the C++
//!
//! Every refusal of `LiteSnapshot::Reader` is here with its wording. Two more
//! are added, both of which a file the C++ writer produced always passes:
//!
//! - **key order** is checked on read, not only on write. The C++ importer
//!   feeds the stream to `SstFileWriter`, which refuses unsorted input late and
//!   obscurely; this importer depends on the order for its per-amount checks,
//!   so it says so at the record;
//! - **bytes after the terminator** are refused. The C++ stops reading at the
//!   terminator and never looks further, so a file with something appended
//!   reads there and does not here.

use std::fs::File;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use wrkz_pow::cn_fast_hash;
use wrkz_primitives::Hash;

/// The first eight bytes of every snapshot.
pub const MAGIC: [u8; 8] = *b"WRKZLITE";
/// `LiteSnapshot::FORMAT_VERSION`. A reader refuses any other.
pub const FORMAT_VERSION: u32 = 1;
/// `LiteSnapshot::HEADER_SIZE`.
pub const HEADER_SIZE: usize = 128;
/// `LiteSnapshot::FRAME_PAYLOAD_TARGET`: a frame is closed after the record that
/// brings it to this many raw bytes or more. Part of the digest's definition.
pub const FRAME_PAYLOAD_TARGET: usize = 4 * 1024 * 1024;
/// A frame length above this is refused rather than allocated
/// (`MAX_FRAME_PAYLOAD`, `LiteSnapshot.cpp:31`).
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024 * 1024;
/// `ZSTD_LEVEL` (`LiteSnapshot.cpp:26`).
pub const ZSTD_LEVEL: i32 = 10;

/// What every function here returns on failure: a message meant for an
/// operator, in the C++'s wording where the C++ has one.
pub type SnapshotResult<T> = Result<T, String>;

/// `LiteSnapshot::Header`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub format_version: u32,
    /// Identifies the chain a snapshot belongs to.
    pub genesis_hash: Hash,
    /// `H`: the snapshot is the chain as it stood with a top block of `H - 1`.
    pub lite_height: u32,
    pub block_info_records: u64,
    pub key_image_records: u64,
    /// Declared by the format and never set by the C++ export: always 0.
    pub amount_count_records: u64,
    pub key_output_records: u64,
    /// `alreadyGeneratedTransactions` of block `H - 1`.
    pub transactions_count: u64,
    /// Distinct amounts among the key outputs.
    pub key_output_amounts_count: u64,
    /// The chained digest over the raw frames.
    pub payload_digest: Hash,
}

impl Default for Header {
    /// `Header {}` in the C++: every count zero, the version already set. It is
    /// what the writer reserves at the front of the file.
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            genesis_hash: [0; 32],
            lite_height: 0,
            block_info_records: 0,
            key_image_records: 0,
            amount_count_records: 0,
            key_output_records: 0,
            transactions_count: 0,
            key_output_amounts_count: 0,
            payload_digest: [0; 32],
        }
    }
}

impl Header {
    /// `totalRecords()`: the four counts, summed with the C++'s `uint64_t`
    /// wrap-around, so a hostile header cannot make this panic.
    pub fn total_records(&self) -> u64 {
        self.block_info_records
            .wrapping_add(self.key_image_records)
            .wrapping_add(self.amount_count_records)
            .wrapping_add(self.key_output_records)
    }

    /// `serializeHeader` (`LiteSnapshot.cpp:130`).
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[0..8].copy_from_slice(&MAGIC);
        out[8..12].copy_from_slice(&self.format_version.to_le_bytes());
        out[12..44].copy_from_slice(&self.genesis_hash);
        out[44..48].copy_from_slice(&self.lite_height.to_le_bytes());
        out[48..56].copy_from_slice(&self.block_info_records.to_le_bytes());
        out[56..64].copy_from_slice(&self.key_image_records.to_le_bytes());
        out[64..72].copy_from_slice(&self.amount_count_records.to_le_bytes());
        out[72..80].copy_from_slice(&self.key_output_records.to_le_bytes());
        out[80..88].copy_from_slice(&self.transactions_count.to_le_bytes());
        out[88..96].copy_from_slice(&self.key_output_amounts_count.to_le_bytes());
        out[96..128].copy_from_slice(&self.payload_digest);
        out
    }

    /// `Reader::open`'s parse (`LiteSnapshot.cpp:343-372`), refusing a foreign
    /// file and any other format version. `name` is what the messages call the
    /// file.
    pub fn from_bytes(raw: &[u8; HEADER_SIZE], name: &str) -> SnapshotResult<Self> {
        if raw[0..8] != MAGIC {
            return Err(format!("{name} is not a lite node snapshot"));
        }
        let format_version = u32_at(raw, 8);
        if format_version != FORMAT_VERSION {
            return Err(format!(
                "{name} is a version {format_version} lite node snapshot, and this build reads version \
                 {FORMAT_VERSION}"
            ));
        }
        Ok(Self {
            format_version,
            genesis_hash: raw[12..44].try_into().expect("32 bytes"),
            lite_height: u32_at(raw, 44),
            block_info_records: u64_at(raw, 48),
            key_image_records: u64_at(raw, 56),
            amount_count_records: u64_at(raw, 64),
            key_output_records: u64_at(raw, 72),
            transactions_count: u64_at(raw, 80),
            key_output_amounts_count: u64_at(raw, 88),
            payload_digest: raw[96..128].try_into().expect("32 bytes"),
        })
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// `chainDigest` (`LiteSnapshot.cpp:119`):
/// `running = cn_fast_hash(running ‖ cn_fast_hash(frame))`.
pub fn chain_digest(running: &Hash, frame: &[u8]) -> Hash {
    let frame_hash = cn_fast_hash(frame);
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(running);
    combined[32..].copy_from_slice(&frame_hash);
    cn_fast_hash(&combined)
}

/// `putVarint`: unsigned LEB128.
pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// `getVarint` (`LiteSnapshot.cpp:88`), with its limits: `None` for a varint
/// that runs off the end of the input or past a shift of 63.
pub fn get_varint(input: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    while *offset < input.len() {
        let byte = input[*offset];
        *offset += 1;
        if shift > 63 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

// ---------------------------------------------------------------------------
// writing
// ---------------------------------------------------------------------------

/// `LiteSnapshot::Writer` over any seekable sink. Records must arrive in
/// strictly ascending key order; [`Writer::add`] checks rather than trusts.
pub struct Writer<W: Write + Seek> {
    out: W,
    name: String,
    frame: Vec<u8>,
    last_key: Vec<u8>,
    have_last_key: bool,
    digest: Hash,
    bytes_written: u64,
    records: u64,
    frames: u64,
    compressor: zstd::bulk::Compressor<'static>,
}

impl<W: Write + Seek> Writer<W> {
    /// `Writer::begin`: reserve the header. `name` is what messages call the
    /// sink.
    pub fn new(mut out: W, name: &str) -> SnapshotResult<Self> {
        out.write_all(&Header::default().to_bytes())
            .map_err(|e| format!("Could not write the header of {name}: {e}"))?;
        let compressor = zstd::bulk::Compressor::new(ZSTD_LEVEL)
            .map_err(|e| format!("Failed to set up the lite snapshot compressor: {e}"))?;
        Ok(Self {
            out,
            name: name.to_string(),
            frame: Vec::with_capacity(FRAME_PAYLOAD_TARGET + 64 * 1024),
            last_key: Vec::new(),
            have_last_key: false,
            digest: [0; 32],
            bytes_written: HEADER_SIZE as u64,
            records: 0,
            frames: 0,
            compressor,
        })
    }

    /// `Writer::add`: one record, closing the frame once it reaches
    /// [`FRAME_PAYLOAD_TARGET`].
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> SnapshotResult<()> {
        if self.have_last_key && self.last_key.as_slice() >= key {
            return Err("Lite snapshot records are not in ascending key order".to_string());
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.have_last_key = true;
        put_varint(&mut self.frame, key.len() as u64);
        self.frame.extend_from_slice(key);
        put_varint(&mut self.frame, value.len() as u64);
        self.frame.extend_from_slice(value);
        self.records += 1;
        if self.frame.len() >= FRAME_PAYLOAD_TARGET {
            self.flush_frame()?;
        }
        Ok(())
    }

    /// `Writer::flushFrame`.
    fn flush_frame(&mut self) -> SnapshotResult<()> {
        if self.frame.is_empty() {
            return Ok(());
        }
        // A reader refuses a frame above this, so a writer must never make one.
        if self.frame.len() > MAX_FRAME_PAYLOAD {
            return Err(format!("A lite snapshot frame of {} bytes is larger than a reader accepts", self.frame.len()));
        }
        self.digest = chain_digest(&self.digest, &self.frame);
        let compressed = self
            .compressor
            .compress(&self.frame)
            .map_err(|e| format!("Failed to compress a lite snapshot frame: {e}"))?;
        let mut lengths = [0u8; 8];
        lengths[..4].copy_from_slice(&(self.frame.len() as u32).to_le_bytes());
        lengths[4..].copy_from_slice(&(compressed.len() as u32).to_le_bytes());
        self.out
            .write_all(&lengths)
            .and_then(|()| self.out.write_all(&compressed))
            .map_err(|e| format!("Failed writing to {} - out of disk space? ({e})", self.name))?;
        self.bytes_written += 8 + compressed.len() as u64;
        self.frames += 1;
        self.frame.clear();
        Ok(())
    }

    /// `Writer::finish`: close the last frame, write the terminator, stamp the
    /// digest into `header` and write it over the reserved bytes. Returns the
    /// header as written and the sink.
    pub fn finish(mut self, header: Header) -> SnapshotResult<(Header, W)> {
        self.flush_frame()?;
        let written = Header { format_version: FORMAT_VERSION, payload_digest: self.digest, ..header };
        let name = self.name.clone();
        let fail = |e: std::io::Error| format!("Failed finalising {name}: {e}");
        self.out.write_all(&[0u8; 8]).map_err(fail)?;
        self.out.seek(SeekFrom::Start(0)).map_err(fail)?;
        self.out.write_all(&written.to_bytes()).map_err(fail)?;
        self.out.flush().map_err(fail)?;
        self.bytes_written += 8;
        Ok((written, self.out))
    }

    /// Bytes written so far, the reserved header included.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Records accepted so far.
    pub fn records(&self) -> u64 {
        self.records
    }

    /// Frames closed so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The digest over the frames closed so far.
    pub fn digest(&self) -> Hash {
        self.digest
    }
}

/// A [`Writer`] onto a new file that **deletes the file** unless
/// [`FileWriter::finish`] succeeds — the C++ `~Writer`'s "a partial snapshot
/// that looks like a whole one is exactly the thing a digest exists to catch,
/// but only if someone checks" (`LiteSnapshot.cpp:168`).
///
/// Unlike the C++, which truncates whatever is there, the file is created
/// exclusively: an existing file is refused rather than overwritten.
pub struct FileWriter {
    path: PathBuf,
    writer: Option<Writer<BufWriter<File>>>,
    finished: bool,
}

impl FileWriter {
    pub fn create(path: &Path) -> SnapshotResult<Self> {
        let file = File::options().write(true).create_new(true).open(path).map_err(|e| {
            if e.kind() == ErrorKind::AlreadyExists {
                format!("{} already exists. Move it aside or name another path.", path.display())
            } else {
                format!("Could not open {} for writing: {e}", path.display())
            }
        })?;
        let mut this = Self { path: path.to_path_buf(), writer: None, finished: false };
        this.writer = Some(Writer::new(BufWriter::with_capacity(1 << 20, file), &path.display().to_string())?);
        Ok(this)
    }

    pub fn add(&mut self, key: &[u8], value: &[u8]) -> SnapshotResult<()> {
        self.writer.as_mut().expect("open until finished").add(key, value)
    }

    /// Finish the file, make it durable, and keep it.
    pub fn finish(mut self, header: Header) -> SnapshotResult<Header> {
        let writer = self.writer.take().expect("open until finished");
        let (written, out) = writer.finish(header)?;
        let file = out.into_inner().map_err(|e| format!("Failed finalising {}: {}", self.path.display(), e.error()))?;
        file.sync_all().map_err(|e| format!("Failed finalising {}: {e}", self.path.display()))?;
        self.finished = true;
        Ok(written)
    }

    pub fn bytes_written(&self) -> u64 {
        self.writer.as_ref().map_or(0, Writer::bytes_written)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        if !self.finished {
            self.writer = None;
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

/// `LiteSnapshot::Reader` over any byte source: the header is checked on
/// construction, the digest chained frame by frame, and
/// [`Reader::next_record`] returns `None` at the terminator, from which point
/// [`Reader::computed_digest`] covers the whole payload.
///
/// Memory is bounded by one compressed and one raw frame, each at most
/// [`MAX_FRAME_PAYLOAD`].
pub struct Reader<R: Read> {
    input: R,
    name: String,
    header: Header,
    frame: Vec<u8>,
    compressed: Vec<u8>,
    decompressor: zstd::bulk::Decompressor<'static>,
    offset: usize,
    digest: Hash,
    records: u64,
    frames: u64,
    exhausted: bool,
    last_key: Vec<u8>,
    have_last_key: bool,
}

/// Open a snapshot file and read its header.
pub fn open(path: &Path) -> SnapshotResult<Reader<BufReader<File>>> {
    let file = File::open(path).map_err(|e| format!("Could not open {}: {e}", path.display()))?;
    Reader::new(BufReader::with_capacity(1 << 20, file), &path.display().to_string())
}

/// The header of a snapshot file, and nothing else: what `--snapshot-info`
/// reads.
pub fn read_header(path: &Path) -> SnapshotResult<Header> {
    open(path).map(|r| r.header)
}

impl<R: Read> Reader<R> {
    pub fn new(mut input: R, name: &str) -> SnapshotResult<Self> {
        let mut raw = [0u8; HEADER_SIZE];
        input.read_exact(&mut raw).map_err(|e| match e.kind() {
            ErrorKind::UnexpectedEof => format!("{name} is too short to be a lite node snapshot"),
            _ => format!("Could not read {name}: {e}"),
        })?;
        let header = Header::from_bytes(&raw, name)?;
        let decompressor = zstd::bulk::Decompressor::new()
            .map_err(|e| format!("Failed to set up the lite snapshot decompressor: {e}"))?;
        Ok(Self {
            input,
            name: name.to_string(),
            header,
            frame: Vec::new(),
            compressed: Vec::new(),
            decompressor,
            offset: 0,
            digest: [0; 32],
            records: 0,
            frames: 0,
            exhausted: false,
            last_key: Vec::new(),
            have_last_key: false,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The digest over every frame read so far; the payload's once
    /// [`Reader::next_record`] has returned `None`.
    pub fn computed_digest(&self) -> Hash {
        self.digest
    }

    pub fn records_read(&self) -> u64 {
        self.records
    }

    pub fn frames_read(&self) -> u64 {
        self.frames
    }

    fn truncated(&self) -> String {
        format!("{} ends in the middle of a frame - the file is truncated", self.name)
    }

    fn read_error(&self, e: std::io::Error) -> String {
        match e.kind() {
            ErrorKind::UnexpectedEof => self.truncated(),
            _ => format!("Could not read {}: {e}", self.name),
        }
    }

    /// `Reader::loadFrame`. Returns `false` at the terminator.
    fn load_frame(&mut self) -> SnapshotResult<bool> {
        let mut lengths = [0u8; 8];
        if let Err(e) = self.input.read_exact(&mut lengths) {
            return Err(self.read_error(e));
        }
        let raw_length = u32_at(&lengths, 0) as usize;
        let compressed_length = u32_at(&lengths, 4) as usize;
        if raw_length == 0 {
            self.exhausted = true;
            let mut probe = [0u8; 1];
            return match self.input.read(&mut probe) {
                Ok(0) => Ok(false),
                Ok(_) => Err(format!("{} continues after its last frame - the file is corrupt", self.name)),
                Err(e) => Err(self.read_error(e)),
            };
        }
        if raw_length > MAX_FRAME_PAYLOAD || compressed_length > MAX_FRAME_PAYLOAD {
            return Err(format!("{} declares an implausible frame size - the file is corrupt", self.name));
        }
        self.compressed.resize(compressed_length, 0);
        if let Err(e) = self.input.read_exact(&mut self.compressed) {
            return Err(self.read_error(e));
        }
        self.frame.clear();
        self.frame.reserve(raw_length);
        let decompressed = self.decompressor.decompress_to_buffer(&self.compressed, &mut self.frame);
        if !matches!(decompressed, Ok(n) if n == raw_length) || self.frame.len() != raw_length {
            return Err(format!("{} holds a frame that does not decompress - the file is corrupt", self.name));
        }
        self.digest = chain_digest(&self.digest, &self.frame);
        self.offset = 0;
        self.frames += 1;
        Ok(true)
    }

    /// `Reader::next`: the next `(key, value)`, borrowed from the current
    /// frame, or `None` at the end of the payload.
    pub fn next_record(&mut self) -> SnapshotResult<Option<(&[u8], &[u8])>> {
        while !self.exhausted && self.offset >= self.frame.len() {
            self.load_frame()?;
        }
        if self.exhausted {
            return Ok(None);
        }
        let mut at = self.offset;
        let (Some((key_start, key_end)), Some((value_start, value_end))) =
            (field(&self.frame, &mut at), field(&self.frame, &mut at))
        else {
            return Err(format!("{} holds a malformed record - the file is corrupt", self.name));
        };
        self.offset = at;
        let key = &self.frame[key_start..key_end];
        if self.have_last_key && self.last_key.as_slice() >= key {
            return Err(format!("{} holds records out of ascending key order - the file is corrupt", self.name));
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.have_last_key = true;
        self.records += 1;
        Ok(Some((&self.frame[key_start..key_end], &self.frame[value_start..value_end])))
    }
}

/// One `LEB128(len) bytes` field starting at `*at`: its byte range.
fn field(frame: &[u8], at: &mut usize) -> Option<(usize, usize)> {
    let len = get_varint(frame, at)?;
    if len > (frame.len() - *at) as u64 {
        return None;
    }
    let start = *at;
    *at += len as usize;
    Some((start, *at))
}

// ---------------------------------------------------------------------------
// the blessed digests, names and --snapshot-info
// ---------------------------------------------------------------------------

/// One entry of `LITE_SNAPSHOT_DIGESTS` (`CryptoNoteSnapshots.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlessedDigest {
    pub lite_height: u32,
    pub payload_digest: Hash,
}

/// `LITE_SNAPSHOT_DIGESTS`, verbatim. This table is the entire security of a
/// snapshot import: everything below `H` is taken on trust, because it cannot
/// be checked without the block bodies the importing node does not have. There
/// is no override, here or in the C++.
const COMPILED_IN_DIGESTS: &[(u32, &str)] =
    &[(4_000_000, "4601d802d990fa26b876ed7fdaffc00953cff6ca6b77299fd1c6981ef94fe09e")];

/// The digests this build imports. The library takes the list as a parameter
/// so tests can bless a synthetic snapshot; the daemon passes only this.
pub fn compiled_in_digests() -> Vec<BlessedDigest> {
    COMPILED_IN_DIGESTS
        .iter()
        .map(|(lite_height, hex_digest)| BlessedDigest {
            lite_height: *lite_height,
            payload_digest: hex::decode(hex_digest)
                .ok()
                .and_then(|b| b.try_into().ok())
                .expect("the compiled-in digests are 64 hex characters"),
        })
        .collect()
}

/// `digestIsBlessed`.
pub fn is_blessed(blessed: &[BlessedDigest], lite_height: u32, digest: &Hash) -> bool {
    blessed.iter().any(|b| b.lite_height == lite_height && b.payload_digest == *digest)
}

/// `LiteSnapshot::defaultFileName`: no timestamp, so identical content gets an
/// identical name.
pub fn default_file_name(lite_height: u32) -> String {
    format!("wrkz-lite-base-h{lite_height}-v{FORMAT_VERSION}.litesnap")
}

/// Where `snapshot_export` writes (`DaemonCommandsHandler.cpp:1206-1224`): the
/// default name in the parent of the absolute data directory, or `requested`,
/// with the default name appended when it is a directory.
pub fn default_output_path(data_dir: &Path, requested: Option<&Path>, lite_height: u32) -> PathBuf {
    match requested {
        Some(path) if path.is_dir() => path.join(default_file_name(lite_height)),
        Some(path) => path.to_path_buf(),
        None => {
            let absolute = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
            let parent = absolute.parent().map(Path::to_path_buf).unwrap_or(absolute);
            parent.join(default_file_name(lite_height))
        }
    }
}

/// The one line `--snapshot-info` prints for a readable file
/// (`LiteSnapshotImporter.cpp:350-364`), key for key.
pub fn describe_json(header: &Header, accepted: bool) -> String {
    format!(
        "{{\"formatVersion\":{},\"liteHeight\":{},\"records\":{},\"blockInfoRecords\":{},\"keyImageRecords\":{},\
         \"keyOutputRecords\":{},\"transactionsCount\":{},\"genesisHash\":\"{}\",\"digest\":\"{}\",\"accepted\":{}}}",
        header.format_version,
        header.lite_height,
        header.total_records(),
        header.block_info_records,
        header.key_image_records,
        header.key_output_records,
        header.transactions_count,
        hex::encode(header.genesis_hash),
        hex::encode(header.payload_digest),
        accepted
    )
}

/// `{"error":"<what>"}`. The C++ pastes the message in unescaped, so a path
/// with a backslash or a quote makes its line invalid JSON; this escapes it.
pub fn error_json(message: &str) -> String {
    let mut escaped = String::with_capacity(message.len());
    for c in message.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    format!("{{\"error\":\"{escaped}\"}}")
}

/// `describeSnapshot`: the line to print and whether the file could be read at
/// all (the process exit status). Header only; no digest is checked.
pub fn describe(path: &Path, blessed: &[BlessedDigest]) -> (String, bool) {
    match read_header(path) {
        Ok(header) => {
            let accepted = is_blessed(blessed, header.lite_height, &header.payload_digest);
            (describe_json(&header, accepted), true)
        }
        Err(e) => (error_json(&e), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_header() -> Header {
        Header {
            format_version: FORMAT_VERSION,
            genesis_hash: std::array::from_fn(|i| i as u8),
            lite_height: 0x0102_0304,
            block_info_records: 0x1111,
            key_image_records: 0x2222,
            amount_count_records: 0,
            key_output_records: 0x3333,
            transactions_count: 0x4444,
            key_output_amounts_count: 0x5555,
            payload_digest: std::array::from_fn(|i| 200 + i as u8),
        }
    }

    /// Write `records` into a snapshot held in memory.
    fn write(records: &[(Vec<u8>, Vec<u8>)], header: Header) -> (Header, Vec<u8>) {
        let mut w = Writer::new(Cursor::new(Vec::new()), "memory").unwrap();
        for (k, v) in records {
            w.add(k, v).unwrap();
        }
        let (header, out) = w.finish(header).unwrap();
        (header, out.into_inner())
    }

    type Records = Vec<(Vec<u8>, Vec<u8>)>;

    fn read_all(bytes: &[u8]) -> SnapshotResult<(Records, Hash)> {
        let mut r = Reader::new(Cursor::new(bytes), "memory")?;
        let mut out = Vec::new();
        while let Some((k, v)) = r.next_record()? {
            out.push((k.to_vec(), v.to_vec()));
        }
        Ok((out, r.computed_digest()))
    }

    /// The frame table of a finished file: `(raw length, compressed length)`
    /// for every frame, terminator excluded.
    fn frames(bytes: &[u8]) -> Vec<(u32, u32)> {
        let mut at = HEADER_SIZE;
        let mut out = Vec::new();
        loop {
            let raw = u32_at(bytes, at);
            let comp = u32_at(bytes, at + 4);
            if raw == 0 {
                return out;
            }
            out.push((raw, comp));
            at += 8 + comp as usize;
        }
    }

    #[test]
    fn the_header_fields_sit_at_the_cpp_offsets() {
        let h = sample_header();
        let b = h.to_bytes();
        assert_eq!(&b[0..8], b"WRKZLITE");
        assert_eq!(&b[8..12], &[1, 0, 0, 0]);
        assert_eq!(b[12..44], h.genesis_hash);
        assert_eq!(&b[44..48], &[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&b[48..56], &0x1111u64.to_le_bytes());
        assert_eq!(&b[56..64], &0x2222u64.to_le_bytes());
        assert_eq!(&b[64..72], &[0; 8]);
        assert_eq!(&b[72..80], &0x3333u64.to_le_bytes());
        assert_eq!(&b[80..88], &0x4444u64.to_le_bytes());
        assert_eq!(&b[88..96], &0x5555u64.to_le_bytes());
        assert_eq!(b[96..128], h.payload_digest);
        assert_eq!(Header::from_bytes(&b, "x").unwrap(), h);
        assert_eq!(h.total_records(), 0x1111 + 0x2222 + 0x3333);

        let reserved = Header::default().to_bytes();
        assert_eq!(&reserved[..12], b"WRKZLITE\x01\0\0\0", "the reserved header carries the magic and version");
        assert!(reserved[12..].iter().all(|b| *b == 0));
    }

    #[test]
    fn a_foreign_or_newer_file_is_refused_with_the_cpp_words() {
        let mut b = sample_header().to_bytes();
        b[8] = 2;
        assert_eq!(
            Header::from_bytes(&b, "f.litesnap").unwrap_err(),
            "f.litesnap is a version 2 lite node snapshot, and this build reads version 1"
        );
        b[0] = b'X';
        assert_eq!(Header::from_bytes(&b, "f.litesnap").unwrap_err(), "f.litesnap is not a lite node snapshot");
        let short = Reader::new(Cursor::new(vec![0u8; 100]), "f.litesnap").err().unwrap();
        assert_eq!(short, "f.litesnap is too short to be a lite node snapshot");
    }

    #[test]
    fn leb128_round_trips_and_stops_where_the_cpp_stops() {
        for v in [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            let mut at = 0;
            assert_eq!(get_varint(&buf, &mut at), Some(v));
            assert_eq!(at, buf.len());
        }
        let mut buf = Vec::new();
        put_varint(&mut buf, 300);
        assert_eq!(buf, [0xAC, 0x02]);
        assert_eq!(get_varint(&[0x80], &mut 0), None, "runs off the end");
        assert_eq!(get_varint(&[0x80; 11], &mut 0), None, "past a shift of 63");
    }

    /// Two hashes over known bytes, worked by hand from the definition.
    #[test]
    fn the_digest_chains_over_the_raw_frames() {
        let (records, mut digest) = (vec![(b"a".to_vec(), b"1".to_vec())], [0u8; 32]);
        let (header, bytes) = write(&records, Header::default());
        let raw_frame = [1u8, b'a', 1, b'1'];
        let expected = cn_fast_hash(&[[0u8; 32], cn_fast_hash(&raw_frame)].concat());
        assert_eq!(header.payload_digest, expected);
        digest = chain_digest(&digest, &raw_frame);
        assert_eq!(digest, expected);
        let (back, computed) = read_all(&bytes).unwrap();
        assert_eq!(back, records);
        assert_eq!(computed, expected);
        assert_eq!(Header::from_bytes(bytes[..HEADER_SIZE].try_into().unwrap(), "m").unwrap(), header);

        // An empty payload has no frame and so the zero digest.
        let (empty, bytes) = write(&[], Header::default());
        assert_eq!(empty.payload_digest, [0; 32]);
        assert_eq!(bytes.len(), HEADER_SIZE + 8);
        assert_eq!(read_all(&bytes).unwrap(), (Vec::new(), [0; 32]));
    }

    /// A frame closes after the record that brings it to 4 MiB, not before it
    /// and not at the next one, which is what makes the digest a function of
    /// the records alone.
    #[test]
    fn a_frame_closes_after_the_record_that_reaches_four_mebibytes() {
        // Each record is 1 + 4 + 2 + 1024 = 1031 bytes raw: a four-byte key and
        // a value whose length is a two-byte LEB128.
        let record = |i: u32| (i.to_be_bytes().to_vec(), vec![(i % 251) as u8; 1024]);
        let per = 1 + 4 + 2 + 1024;
        let fits = FRAME_PAYLOAD_TARGET / per; // records strictly below the target
        assert!(fits * per < FRAME_PAYLOAD_TARGET && (fits + 1) * per >= FRAME_PAYLOAD_TARGET);

        let records: Vec<_> = (0..(fits as u32 + 1)).map(record).collect();
        let (_, bytes) = write(&records, Header::default());
        assert_eq!(frames(&bytes).iter().map(|f| f.0 as usize).collect::<Vec<_>>(), [(fits + 1) * per]);

        let records: Vec<_> = (0..(fits as u32 + 2)).map(record).collect();
        let (h2, bytes) = write(&records, Header::default());
        assert_eq!(frames(&bytes).iter().map(|f| f.0 as usize).collect::<Vec<_>>(), [(fits + 1) * per, per]);

        // The digest is the chain over exactly those two raw frames.
        let mut raw = Vec::new();
        for (k, v) in &records {
            put_varint(&mut raw, k.len() as u64);
            raw.extend_from_slice(k);
            put_varint(&mut raw, v.len() as u64);
            raw.extend_from_slice(v);
        }
        let split = (fits + 1) * per;
        let expected = chain_digest(&chain_digest(&[0; 32], &raw[..split]), &raw[split..]);
        assert_eq!(h2.payload_digest, expected);
        assert_eq!(read_all(&bytes).unwrap().1, expected);
    }

    #[test]
    fn records_out_of_order_are_refused_on_both_sides() {
        let mut w = Writer::new(Cursor::new(Vec::new()), "m").unwrap();
        w.add(b"b", b"").unwrap();
        assert_eq!(w.add(b"a", b"").unwrap_err(), "Lite snapshot records are not in ascending key order");
        assert!(w.add(b"b", b"").is_err(), "a repeated key is not ascending either");

        // Hand-build a file whose one frame holds two keys the wrong way round.
        let mut raw = Vec::new();
        for key in [b"b", b"a"] {
            put_varint(&mut raw, 1);
            raw.extend_from_slice(key);
            put_varint(&mut raw, 0);
        }
        let comp = zstd::bulk::compress(&raw, 3).unwrap();
        let mut bytes = Header::default().to_bytes().to_vec();
        bytes.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&comp);
        bytes.extend_from_slice(&[0; 8]);
        assert_eq!(
            read_all(&bytes).unwrap_err(),
            "memory holds records out of ascending key order - the file is corrupt"
        );
    }

    #[test]
    fn truncation_and_corruption_are_named() {
        let records: Vec<_> = (0..100u32).map(|i| (i.to_be_bytes().to_vec(), vec![i as u8; 40])).collect();
        let (_, bytes) = write(&records, Header::default());
        let truncated = "memory ends in the middle of a frame - the file is truncated";

        // No terminator, a cut inside the lengths, a cut inside the frame.
        assert_eq!(read_all(&bytes[..bytes.len() - 8]).unwrap_err(), truncated);
        assert_eq!(read_all(&bytes[..bytes.len() - 3]).unwrap_err(), truncated);
        assert_eq!(read_all(&bytes[..HEADER_SIZE + 4]).unwrap_err(), truncated);
        assert_eq!(read_all(&bytes[..HEADER_SIZE + 20]).unwrap_err(), truncated);
        assert_eq!(read_all(&bytes[..HEADER_SIZE]).unwrap_err(), truncated);

        // A flipped byte inside the compressed frame.
        let mut flipped = bytes.clone();
        let middle = HEADER_SIZE + 8 + frames(&bytes)[0].1 as usize / 2;
        flipped[middle] ^= 0xFF;
        // zstd carries no checksum here (neither does the C++'s frame), so a
        // flip may decompress: then the record parse or the digest catches it.
        let e = read_all(&flipped);
        assert!(
            e.as_ref().is_err_and(|m| m.ends_with("the file is corrupt"))
                || e.as_ref().is_ok_and(|(_, d)| *d != read_all(&bytes).unwrap().1),
            "zstd, the record parse or the digest notices: {e:?}"
        );

        // A raw length that disagrees with what the frame decompresses to.
        let mut lying = bytes.clone();
        let raw = u32_at(&bytes, HEADER_SIZE);
        lying[HEADER_SIZE..HEADER_SIZE + 4].copy_from_slice(&(raw - 1).to_le_bytes());
        assert_eq!(
            read_all(&lying).unwrap_err(),
            "memory holds a frame that does not decompress - the file is corrupt"
        );

        // An implausible length is refused before anything is allocated.
        let mut huge = bytes.clone();
        huge[HEADER_SIZE + 4..HEADER_SIZE + 8].copy_from_slice(&(65 * 1024 * 1024u32).to_le_bytes());
        assert_eq!(read_all(&huge).unwrap_err(), "memory declares an implausible frame size - the file is corrupt");

        // Bytes after the terminator.
        let mut appended = bytes.clone();
        appended.push(0);
        assert_eq!(read_all(&appended).unwrap_err(), "memory continues after its last frame - the file is corrupt");
    }

    #[test]
    fn a_record_that_overruns_its_frame_is_malformed() {
        let mut raw = Vec::new();
        put_varint(&mut raw, 10);
        raw.extend_from_slice(b"short");
        let comp = zstd::bulk::compress(&raw, 3).unwrap();
        let mut bytes = Header::default().to_bytes().to_vec();
        bytes.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&comp);
        bytes.extend_from_slice(&[0; 8]);
        assert_eq!(read_all(&bytes).unwrap_err(), "memory holds a malformed record - the file is corrupt");
    }

    #[test]
    fn a_file_writer_removes_what_it_did_not_finish() {
        let dir = std::env::temp_dir().join(format!("wrkz-litesnap-writer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("partial.litesnap");
        let _ = std::fs::remove_file(&path);
        {
            let mut w = FileWriter::create(&path).unwrap();
            w.add(b"k", b"v").unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "a writer dropped before finish deletes its file");

        let mut w = FileWriter::create(&path).unwrap();
        w.add(b"k", b"v").unwrap();
        let header = w.finish(Header { lite_height: 7, ..Header::default() }).unwrap();
        assert!(path.exists());
        assert_eq!(read_header(&path).unwrap(), header);
        let e = FileWriter::create(&path).err().unwrap();
        assert!(e.ends_with("already exists. Move it aside or name another path."), "{e}");
        assert!(path.exists(), "and the refusal left the finished file alone");
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn snapshot_info_prints_the_cpp_line() {
        let header = Header {
            lite_height: 4_000_000,
            block_info_records: 4_000_000,
            key_image_records: 67_190_000,
            key_output_records: 77_538_732,
            transactions_count: 7_469_434,
            key_output_amounts_count: 140_000,
            genesis_hash: [0x87; 32],
            payload_digest: compiled_in_digests()[0].payload_digest,
            ..Header::default()
        };
        let accepted = is_blessed(&compiled_in_digests(), header.lite_height, &header.payload_digest);
        assert!(accepted);
        assert_eq!(
            describe_json(&header, accepted),
            format!(
                "{{\"formatVersion\":1,\"liteHeight\":4000000,\"records\":148728732,\"blockInfoRecords\":4000000,\
                 \"keyImageRecords\":67190000,\"keyOutputRecords\":77538732,\"transactionsCount\":7469434,\
                 \"genesisHash\":\"{}\",\"digest\":\"{}\",\"accepted\":true}}",
                "87".repeat(32),
                "4601d802d990fa26b876ed7fdaffc00953cff6ca6b77299fd1c6981ef94fe09e"
            )
        );
        assert!(!is_blessed(&compiled_in_digests(), 3_999_999, &header.payload_digest), "the height is part of it");
        assert_eq!(error_json("C:\\x \"y\""), r#"{"error":"C:\\x \"y\""}"#);
        let (line, ok) = describe(Path::new("no/such/file.litesnap"), &compiled_in_digests());
        assert!(!ok);
        assert!(line.starts_with("{\"error\":\"Could not open no/such/file.litesnap"), "{line}");
    }

    #[test]
    fn the_default_name_and_path_follow_the_cpp() {
        assert_eq!(default_file_name(4_000_000), "wrkz-lite-base-h4000000-v1.litesnap");
        let dir = std::env::temp_dir().join(format!("wrkz-litesnap-path-{}", std::process::id()));
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let default = default_output_path(&data, None, 5);
        assert_eq!(default.file_name().unwrap(), "wrkz-lite-base-h5-v1.litesnap");
        assert_eq!(default.parent().unwrap().file_name(), dir.file_name(), "the parent of the data directory");
        assert_eq!(default_output_path(&data, Some(&dir), 5), dir.join("wrkz-lite-base-h5-v1.litesnap"));
        assert_eq!(default_output_path(&data, Some(&dir.join("x.bin")), 5), dir.join("x.bin"));
        let _ = std::fs::remove_dir(&data);
        let _ = std::fs::remove_dir(&dir);
    }
}
