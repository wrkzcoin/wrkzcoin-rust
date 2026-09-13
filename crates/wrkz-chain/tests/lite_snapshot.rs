// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Lite node snapshots against a chain built from genesis, with every rule on.
//!
//! The real acceptance of this feature — importing the published C++ snapshot
//! at 4,000,000 and exporting the same digest from a real state — needs the
//! 5 GiB file and a synced node, and is documented in `docs/DAEMON.md`. What
//! can be proven without them is proven here, over a chain short enough to
//! build in a test and long enough to cross a lite height with real spends on
//! both sides of it:
//!
//! - a snapshot written the way the C++ database walk writes one — the generic
//!   KV encoder, a byte-sorted map, no fixed layouts — imports, and every
//!   record below the height lands where a synced state has it;
//! - the imported state then applies the blocks above the height exactly as
//!   the state that built them did: the same outcomes, the same records, and
//!   the same refusal of a double spend whose first half is below the line;
//! - every refusal of the C++ importer refuses here, and writes nothing.
//!
//! The chain is built from the real genesis with no checkpoints, so the proof of
//! work, the ring signatures and every state rule run on every block. Blocks
//! are 60 seconds apart, which holds the difficulty at 1, so no work is mined.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use wrkz_chain::records::OutputRecord;
use wrkz_chain::reward::get_block_reward;
use wrkz_chain::snapshot::container::{BlessedDigest, Header, Writer};
use wrkz_chain::snapshot::export::{export_to, ExportControl};
use wrkz_chain::snapshot::import::{import_snapshot, ImportEvent};
use wrkz_chain::{keys, AddOutcome, ChainState, Checkpoints, Config};
use wrkz_pow::{cn_fast_hash, curve};
use wrkz_primitives::block::{BlockTemplate, ParentBlock};
use wrkz_primitives::constants::{block_major_version_for_index, GENESIS_BLOCK_TIMESTAMP};
use wrkz_primitives::tx::{
    absolute_to_relative_offsets, append_merge_mining_tag, build_extra, BaseTransaction, Input, MergeMiningTag, Output,
    Transaction, TransactionPrefix,
};
use wrkz_primitives::Hash;
use wrkz_storage::codec::{self, KeyPart};
use wrkz_storage::records::{CachedBlockInfo, KeyOutputInfo};
use wrkz_storage::MemStore;

const SPACING: u64 = 60;
const NOW: u64 = GENESIS_BLOCK_TIMESTAMP + 10_000_000;
/// Two coinbase outputs of this amount per block, so rings of two can be drawn.
const DENOMINATION: u64 = 1_000_000;
const FEE: u64 = 10;
/// The lite height every test here imports at.
const H: u32 = 60;
/// The chain the tests build: spends below `H` and above it.
const TOP: u32 = 75;

/// A block blob and its transaction blobs.
type Blobs = (Vec<u8>, Vec<Vec<u8>>);
/// Snapshot records, sorted as the file holds them.
type Records = BTreeMap<Vec<u8>, Vec<u8>>;

// ---------------------------------------------------------------------------
// the chain
// ---------------------------------------------------------------------------

struct Wallet {
    spend_secret: Hash,
    spend_public: Hash,
    view_secret: Hash,
    view_public: Hash,
}

impl Wallet {
    fn from_seed(seed: u8) -> Self {
        let (spend_secret, spend_public) = curve::generate_deterministic_keys(&cn_fast_hash(&[seed, 0x5A]));
        let (view_secret, view_public) = curve::generate_view_from_spend(&spend_secret);
        Self { spend_secret, spend_public, view_secret, view_public }
    }
}

#[derive(Clone, Copy, Debug)]
struct Spendable {
    amount: u64,
    global_index: u32,
    public_key: Hash,
    secret_key: Hash,
}

fn tx_keys(tag: &[u8]) -> (Hash, Hash) {
    curve::generate_deterministic_keys(&cn_fast_hash(&[b"lite snapshot test".as_slice(), tag].concat()))
}

fn derive_output(to: &Wallet, tx_secret: &Hash, tx_public: &Hash, output_index: u64) -> (Hash, Hash) {
    let derivation = curve::generate_key_derivation(tx_public, &to.view_secret).expect("derivation");
    let public_key = curve::derive_public_key(&derivation, output_index, &to.spend_public).expect("public key");
    let receiver = curve::generate_key_derivation(&to.view_public, tx_secret).expect("receiver derivation");
    let secret_key = curve::derive_secret_key(&receiver, output_index, &to.spend_secret);
    (public_key, secret_key)
}

fn serving() -> Config {
    Config { store_raw_blocks: true, unwind_history: u32::MAX, recent_window: 256, ..Config::default() }
}

fn lite(height: u32) -> Config {
    Config { lite_start_height: height, ..serving() }
}

struct Chain {
    state: ChainState<MemStore>,
    miner: Wallet,
    payee: Wallet,
    /// Block `i + 1`, as applied.
    blocks: Vec<Blobs>,
    outcomes: Vec<AddOutcome>,
    /// The two `DENOMINATION` outputs of block `i + 1`'s coinbase.
    coinbase: Vec<[Spendable; 2]>,
}

impl Chain {
    fn new() -> Self {
        let mut state = ChainState::open_or_genesis(MemStore::default(), serving(), Checkpoints::none()).unwrap();
        state.set_clock(Some(NOW));
        Self {
            state,
            miner: Wallet::from_seed(1),
            payee: Wallet::from_seed(2),
            blocks: Vec::new(),
            outcomes: Vec::new(),
            coinbase: Vec::new(),
        }
    }

    fn tip(&self) -> u32 {
        self.state.tip_index().unwrap()
    }

    /// The next block over `txs`, built and mined but not applied, and its two
    /// spendable coinbase outputs.
    fn build(&self, txs: &[Transaction], tag: &[u8]) -> (Blobs, [Spendable; 2]) {
        let index = self.tip() + 1;
        let major = block_major_version_for_index(u64::from(index));
        let parent = *self.state.tip_info().unwrap();
        let fee: u64 = txs.iter().map(|t| t.fee().unwrap()).sum();
        let reward = get_block_reward(major, 0, 0, parent.already_generated_coins, fee, u64::from(index))
            .expect("a small block has a reward")
            .reward;
        let (tx_secret, tx_public) = tx_keys(&[tag, b"coinbase"].concat());
        let amounts = [DENOMINATION, DENOMINATION, reward - 2 * DENOMINATION];
        let mut outputs = Vec::new();
        let mut spendable = Vec::new();
        for (i, amount) in amounts.iter().enumerate() {
            let (key, secret_key) = derive_output(&self.miner, &tx_secret, &tx_public, i as u64);
            outputs.push(Output { amount: *amount, key });
            spendable.push((key, secret_key));
        }
        let coinbase = Transaction {
            prefix: TransactionPrefix {
                version: 1,
                unlock_time: u64::from(index) + 40,
                inputs: vec![Input::Base { block_index: u64::from(index) }],
                outputs,
                extra: build_extra(&tx_public, None, None).unwrap(),
            },
            signatures: Vec::new(),
        };
        let mut block = BlockTemplate {
            major_version: major,
            minor_version: 0,
            timestamp: GENESIS_BLOCK_TIMESTAMP + u64::from(index) * SPACING,
            previous_block_hash: parent.block_hash,
            nonce: 0,
            parent_block: None,
            base_transaction: coinbase,
            transaction_hashes: txs.iter().map(|t| t.hash().unwrap()).collect(),
        };
        if major >= 2 {
            let aux = block.auxiliary_header_hash().unwrap();
            let mut parent_extra = Vec::new();
            append_merge_mining_tag(&mut parent_extra, &MergeMiningTag { depth: 0, merkle_root: aux });
            let parent_coinbase = BaseTransaction {
                prefix: TransactionPrefix {
                    version: 0,
                    unlock_time: 0,
                    inputs: vec![],
                    outputs: vec![],
                    extra: parent_extra,
                },
            };
            block.parent_block =
                Some(ParentBlock::new(0, 0, parent.block_hash, 1, Vec::new(), parent_coinbase, Vec::new()));
        }
        let difficulty = self.state.main_chain_difficulty_for_next_block(self.tip()).unwrap().expect("a difficulty");
        while !block.check_proof_of_work(difficulty).unwrap() {
            block.nonce += 1;
        }
        // The two denominations take the next two global indexes of their
        // amount: the coinbase's outputs are numbered before the block's
        // transactions', and no transaction here pays out that amount.
        let first = self.state.output_count_for_amount(DENOMINATION).unwrap();
        let pair = [0usize, 1].map(|i| Spendable {
            amount: DENOMINATION,
            global_index: first + i as u32,
            public_key: spendable[i].0,
            secret_key: spendable[i].1,
        });
        ((block.to_bytes().unwrap(), txs.iter().map(|t| t.to_bytes().unwrap()).collect()), pair)
    }

    /// Build, apply and record the next block.
    fn push(&mut self, txs: &[Transaction]) -> AddOutcome {
        let tag = (self.tip() + 1).to_le_bytes();
        let (block, pair) = self.build(txs, &tag);
        let outcome =
            self.state.add_block(&block.0, &block.1).unwrap_or_else(|e| panic!("block {}: {e}", self.tip() + 1));
        self.blocks.push(block);
        self.outcomes.push(outcome);
        self.coinbase.push(pair);
        outcome
    }

    /// Output `which` of block `index`'s coinbase.
    fn output(&self, index: u32, which: usize) -> Spendable {
        self.coinbase[index as usize - 1][which]
    }

    /// A transaction spending `real` in a ring with `decoy`.
    fn spend(&self, real: &Spendable, decoy: &Spendable, tag: &[u8]) -> Transaction {
        let (lower, higher, secret_index) =
            if real.global_index < decoy.global_index { (real, decoy, 0usize) } else { (decoy, real, 1usize) };
        let ring = [lower.public_key, higher.public_key];
        let offsets =
            absolute_to_relative_offsets(&[u64::from(lower.global_index), u64::from(higher.global_index)]).unwrap();
        let key_image = curve::generate_key_image(&real.public_key, &real.secret_key);
        let (tx_secret, tx_public) = tx_keys(tag);
        let (out_key, _) = derive_output(&self.payee, &tx_secret, &tx_public, 0);
        let mut tx = Transaction {
            prefix: TransactionPrefix {
                version: 1,
                unlock_time: 0,
                inputs: vec![Input::Key { amount: real.amount, key_offsets: offsets, key_image }],
                outputs: vec![Output { amount: real.amount - FEE, key: out_key }],
                extra: build_extra(&tx_public, None, None).unwrap(),
            },
            signatures: Vec::new(),
        };
        let prefix_hash = tx.prefix.hash();
        tx.signatures.push(
            curve::generate_ring_signature(&prefix_hash, &key_image, &ring, &real.secret_key, secret_index).unwrap(),
        );
        tx
    }
}

/// The chain every test here starts from: spends of old coinbase outputs in
/// blocks 46 to 55, below `H`, and in blocks 62 and 64, above it, with rings
/// that reach below it.
fn build_chain(top: u32) -> Chain {
    let mut chain = Chain::new();
    while chain.tip() < top {
        let next = chain.tip() + 1;
        let txs = match next {
            46..=55 => {
                let from = next - 45;
                vec![chain.spend(&chain.output(from, 0), &chain.output(from, 1), &next.to_le_bytes())]
            }
            62 => vec![chain.spend(&chain.output(11, 0), &chain.output(11, 1), b"62")],
            64 => vec![chain.spend(&chain.output(12, 0), &chain.output(2, 1), b"64")],
            _ => Vec::new(),
        };
        chain.push(&txs);
    }
    chain
}

// ---------------------------------------------------------------------------
// the C++ walk, written independently of the exporter
// ---------------------------------------------------------------------------

/// Every record a C++ database at `state`'s chain would hand
/// `walkSnapshotRecords(h)`, as the C++ serializer encodes them, in the byte
/// order RocksDB would iterate them: a map sorted on the encoded keys. This is
/// deliberately not `wrkz_chain::snapshot::export`: it shares no layout code
/// and no enumeration order with it.
fn cpp_records(state: &ChainState<MemStore>, h: u32) -> (BTreeMap<Vec<u8>, Vec<u8>>, Header) {
    let mut records = BTreeMap::new();
    let mut header = Header { lite_height: h, ..Header::default() };
    for index in 0..h {
        let info = state.block_info(index).unwrap().unwrap();
        let cached = CachedBlockInfo {
            block_hash: info.block_hash,
            timestamp: info.timestamp,
            block_size: info.block_size,
            cumulative_difficulty: info.cumulative_difficulty,
            already_generated_coins: info.already_generated_coins,
            already_generated_transactions: info.already_generated_transactions,
        };
        records.insert(codec::key("6", KeyPart::U32(index)), cached.encode());
        header.block_info_records += 1;
        if index == 0 {
            header.genesis_hash = info.block_hash;
        }
        if index == h - 1 {
            header.transactions_count = info.already_generated_transactions;
        }
    }
    let mut amounts = HashSet::new();
    for (key, value) in &state.store().map {
        if key.len() == 34 && key[..2] == [keys::NS, keys::TAG_KEY_IMAGE] {
            let spent_at = u32::from_le_bytes(value[..4].try_into().unwrap());
            if spent_at < h {
                let image: Hash = key[2..].try_into().unwrap();
                records.insert(codec::key("7", KeyPart::Hash(image)), codec::value_u32("7", spent_at));
                header.key_image_records += 1;
            }
        }
        if key.len() == 14 && key[..2] == [keys::NS, keys::TAG_OUTPUT] {
            let output = OutputRecord::decode(value).unwrap();
            if output.block_index < h {
                let amount = u64::from_be_bytes(key[2..10].try_into().unwrap());
                let global_index = u32::from_be_bytes(key[10..14].try_into().unwrap());
                let info = KeyOutputInfo {
                    public_key: output.public_key,
                    transaction_hash: [0; 32],
                    unlock_time: output.unlock_time,
                    output_index: output.output_index,
                    block_index: output.block_index,
                };
                records.insert(codec::key("j", KeyPart::AmountIndex(amount, global_index)), info.encode());
                header.key_output_records += 1;
                amounts.insert(amount);
            }
        }
    }
    header.key_output_amounts_count = amounts.len() as u64;
    (records, header)
}

/// Write `records` as a snapshot file with `header`'s counts, and return the
/// header as written (digest stamped).
fn write_snapshot(path: &Path, records: &BTreeMap<Vec<u8>, Vec<u8>>, header: Header) -> Header {
    let _ = std::fs::remove_file(path);
    let file = std::fs::File::create(path).unwrap();
    let mut writer = Writer::new(std::io::BufWriter::new(file), "test").unwrap();
    for (k, v) in records {
        writer.add(k, v).unwrap();
    }
    let (written, mut out) = writer.finish(header).unwrap();
    std::io::Write::flush(&mut out).unwrap();
    written
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wrkz-lite-snapshot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn bless(header: &Header) -> Vec<BlessedDigest> {
    vec![BlessedDigest { lite_height: header.lite_height, payload_digest: header.payload_digest }]
}

/// A fresh lite state at `h`, holding genesis and nothing else.
fn empty_lite(h: u32) -> ChainState<MemStore> {
    let mut state = ChainState::open_or_genesis(MemStore::default(), lite(h), Checkpoints::none()).unwrap();
    state.set_clock(Some(NOW));
    state
}

fn import(
    state: &mut ChainState<MemStore>,
    path: &Path,
    blessed: &[BlessedDigest],
) -> Result<Vec<ImportEvent>, String> {
    let mut events = Vec::new();
    let h = state.lite_start_height();
    import_snapshot(state, path, h, blessed, &Checkpoints::none(), &mut |e| events.push(e))?;
    Ok(events)
}

/// The records under one tag, keyed without the namespace.
fn table(state: &ChainState<MemStore>, tag: u8) -> BTreeMap<Vec<u8>, Vec<u8>> {
    state
        .store()
        .map
        .iter()
        .filter(|(k, _)| k.len() >= 2 && k[..2] == [keys::NS, tag])
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn index_of_key(key: &[u8]) -> u32 {
    u32::from_be_bytes(key[2..6].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn a_snapshot_imports_and_the_state_follows_the_chain_exactly_as_the_original() {
    let mut original = build_chain(TOP);
    let (records, template) = cpp_records(&original.state, H);
    let path = scratch("follow.litesnap");
    let header = write_snapshot(&path, &records, template);
    assert_eq!(header.block_info_records, u64::from(H));
    assert!(header.key_image_records >= 10, "the spends of blocks 46 to 55 are below the line");

    let mut imported = empty_lite(H);
    let events = import(&mut imported, &path, &bless(&header)).expect("the snapshot imports");
    assert!(events.contains(&ImportEvent::Progress {
        phase: "done",
        done: header.total_records(),
        total: header.total_records()
    }));
    assert_eq!(imported.tip_index(), Some(H - 1));
    assert_eq!(imported.tag().unwrap().as_deref(), Some(keys::TAG_LITE_SNAPSHOT));
    assert_eq!(imported.transactions_floor(), H);
    assert_eq!(imported.tip_info(), original.state.block_info(H - 1).unwrap().as_ref());

    // Below the line: every consensus record the original has, the output
    // transaction hashes zeroed except genesis's, and no transaction records.
    for tag in [keys::TAG_BLOCK_INFO, keys::TAG_HASH_TO_INDEX, keys::TAG_KEY_IMAGE] {
        let ours = table(&imported, tag);
        let theirs = table(&original.state, tag);
        for (k, v) in &ours {
            assert_eq!(theirs.get(k), Some(v), "tag {}", tag as char);
        }
    }
    assert_eq!(
        imported.output_count_for_amount(DENOMINATION).unwrap(),
        original.state.output_count_for_amount_below(DENOMINATION, H).unwrap()
    );
    for (k, v) in table(&imported, keys::TAG_OUTPUT) {
        let ours = OutputRecord::decode(&v).unwrap();
        let theirs = OutputRecord::decode(&table(&original.state, keys::TAG_OUTPUT)[&k]).unwrap();
        let expected_hash = if ours.block_index == 0 { theirs.transaction_hash } else { [0; 32] };
        assert_eq!(ours, OutputRecord { transaction_hash: expected_hash, ..theirs });
    }
    assert!(table(&imported, keys::TAG_TRANSACTION_INDEX).len() == 1, "the genesis coinbase, and nothing else");
    assert!(table(&imported, keys::TAG_BLOCK_TX_HASHES).keys().all(|k| index_of_key(k) == 0));

    // Above the line: the original's blocks, one by one, with the same outcome.
    for index in H..=TOP {
        let (blob, txs) = &original.blocks[index as usize - 1];
        let outcome = imported.add_block(blob, txs).unwrap_or_else(|e| panic!("block {index}: {e}"));
        assert_eq!(outcome, original.outcomes[index as usize - 1], "block {index}");
    }
    assert_eq!(imported.tip_info(), original.state.tip_info());
    // ... leaving the same consensus records, and the same per-block records
    // from the line up.
    for tag in [keys::TAG_BLOCK_INFO, keys::TAG_HASH_TO_INDEX, keys::TAG_KEY_IMAGE, keys::TAG_OUTPUT_COUNT] {
        assert_eq!(table(&imported, tag), table(&original.state, tag), "tag {}", tag as char);
    }
    for tag in [keys::TAG_BLOCK_TX_HASHES, keys::TAG_BLOCK_KEY_IMAGES, keys::TAG_BLOCK_OUTPUTS, keys::TAG_RAW_BLOCK] {
        let theirs: BTreeMap<_, _> = table(&original.state, tag)
            .into_iter()
            .filter(|(k, _)| index_of_key(k) >= H || index_of_key(k) == 0)
            .collect();
        assert_eq!(table(&imported, tag), theirs, "tag {}", tag as char);
    }
    let theirs_outputs = table(&original.state, keys::TAG_OUTPUT);
    for (k, v) in table(&imported, keys::TAG_OUTPUT) {
        let ours = OutputRecord::decode(&v).unwrap();
        if ours.block_index >= H {
            assert_eq!(v, theirs_outputs[&k], "an output created above the line carries its real hash");
        }
    }

    // A double spend whose first half is below the line: block 47 spent block
    // 2's first output. Both states refuse it, for the same reason.
    let respend = original.spend(&original.output(2, 0), &original.output(3, 1), b"respend");
    let (bad, _) = original.build(&[respend], b"bad");
    let theirs = original.state.add_block(&bad.0, &bad.1).unwrap_err();
    let ours = imported.add_block(&bad.0, &bad.1).unwrap_err();
    assert_eq!(ours.rule(), theirs.rule());
    assert!(ours.to_string().contains("INPUT_KEYIMAGE_ALREADY_SPENT"), "{ours}");

    // A good spend with a ring drawn from below the line applies to both.
    let good = original.spend(&original.output(20, 0), &original.output(5, 1), b"good");
    let (next, _) = original.build(&[good], b"good block");
    let theirs = original.state.add_block(&next.0, &next.1).unwrap();
    let ours = imported.add_block(&next.0, &next.1).unwrap();
    assert_eq!(ours, theirs);
    assert_eq!(
        imported
            .key_image_spent_at(&curve::generate_key_image(
                &original.output(20, 0).public_key,
                &original.output(20, 0).secret_key
            ))
            .unwrap(),
        Some(TOP + 1)
    );
    let _ = std::fs::remove_file(&path);
}

/// The C++'s refusals, each before a record is written.
#[test]
fn every_refusal_of_the_cpp_importer_writes_nothing() {
    let original = build_chain(H + 5);
    let (records, template) = cpp_records(&original.state, H);
    let path = scratch("refusals.litesnap");
    let header = write_snapshot(&path, &records, template);
    let blessed = bless(&header);

    let refuse = |state: &mut ChainState<MemStore>, path: &Path, blessed: &[BlessedDigest], want: &str| {
        let before = state.store().map.clone();
        let e = import(state, path, blessed).expect_err("refused");
        assert!(e.contains(want), "expected `{want}` in: {e}");
        assert_eq!(state.store().map, before, "a refusal writes nothing: {e}");
    };

    // Not blessed.
    refuse(&mut empty_lite(H), &path, &[], "This build does not recognise that snapshot");
    // Another height.
    let mut other = empty_lite(H + 1);
    let e = import_snapshot(&mut other, &path, H + 1, &blessed, &Checkpoints::none(), &mut |_| {}).unwrap_err();
    assert!(
        e.contains("describes the chain below height 60, and this daemon was started with --lite-height 61"),
        "{e}"
    );
    // A state that is not lite at that height.
    let mut full = ChainState::open_or_genesis(MemStore::default(), serving(), Checkpoints::none()).unwrap();
    let e = import_snapshot(&mut full, &path, H, &blessed, &Checkpoints::none(), &mut |_| {}).unwrap_err();
    assert!(e.contains("needs --lite and the --lite-height"), "{e}");
    // A database holding more than genesis.
    let mut busy = empty_lite(H);
    busy.add_block(&original.blocks[0].0, &original.blocks[0].1).unwrap();
    refuse(&mut busy, &path, &blessed, "already holds a chain up to block 1");
    // One an earlier import stopped half way through.
    let mut half = empty_lite(H);
    half.set_tag(keys::TAG_LITE_SNAPSHOT_IMPORTING).unwrap();
    refuse(&mut half, &path, &blessed, "stopped part of the way through");

    // Another chain's genesis.
    let foreign = scratch("foreign.litesnap");
    std::fs::copy(&path, &foreign).unwrap();
    patch(&foreign, 12, &[0x42; 32]);
    refuse(&mut empty_lite(H), &foreign, &blessed, "That snapshot is for a chain whose genesis block is 4242");

    // A payload that does not hash to the blessed digest its header claims.
    let mut altered = records.clone();
    let (k, _) = altered.iter().find(|(k, _)| k[11] == b'7').map(|(k, v)| (k.clone(), v.clone())).unwrap();
    altered.insert(k, codec::value_u32("7", 1));
    let tampered = scratch("tampered.litesnap");
    write_snapshot(&tampered, &altered, template_of(&header));
    patch(&tampered, 96, &header.payload_digest);
    refuse(&mut empty_lite(H), &tampered, &blessed, "The file is damaged or has been tampered with");

    // A checkpoint it disagrees with.
    let wrong = Checkpoints::from_csv(&format!("10,{}", hex::encode([7u8; 32]))).unwrap();
    let mut state = empty_lite(H);
    let before = state.store().map.clone();
    let e = import_snapshot(&mut state, &path, H, &blessed, &wrong, &mut |_| {}).unwrap_err();
    assert!(e.contains("this build's checkpoints say it must be 0707"), "{e}");
    assert_eq!(state.store().map, before);

    // Counts in the header that the payload does not have.
    let miscounted = scratch("miscounted.litesnap");
    let h2 = write_snapshot(
        &miscounted,
        &records,
        Header { key_image_records: header.key_image_records + 1, ..template_of(&header) },
    );
    refuse(&mut empty_lite(H), &miscounted, &bless(&h2), "different record counts than its header claims");

    // Each of the rest is a well-formed, blessed file describing a region that
    // is not a chain.
    let mut cases: Vec<(Records, Header, &str)> = Vec::new();
    // A block missing.
    let mut missing = records.clone();
    missing.remove(&codec::key("6", KeyPart::U32(5)));
    cases.push((
        missing,
        Header { block_info_records: u64::from(H) - 1, ..template_of(&header) },
        "carries 59 blocks and needs all 60",
    ));
    // A block at the height it claims to stop below.
    let mut above = records.clone();
    above.insert(codec::key("6", KeyPart::U32(H)), records[&codec::key("6", KeyPart::U32(1))].clone());
    cases.push((
        above,
        Header { block_info_records: u64::from(H) + 1, ..template_of(&header) },
        "The snapshot carries block 60, which is at or above",
    ));
    // Coins that go backwards.
    let mut minted = records.clone();
    let mut info =
        wrkz_storage::records::CachedBlockInfo::decode(&records[&codec::key("6", KeyPart::U32(30))]).unwrap();
    info.already_generated_coins = 1;
    minted.insert(codec::key("6", KeyPart::U32(30)), info.encode());
    cases.push((minted, template_of(&header), "The snapshot's generated coins fall at block 30"));
    // A key image spent at the line.
    let mut late = records.clone();
    let (k, _) = late.iter().find(|(k, _)| k[11] == b'7').map(|(k, v)| (k.clone(), v.clone())).unwrap();
    late.insert(k, codec::value_u32("7", H));
    cases.push((late, template_of(&header), "a key image spent in block 60"));
    // A gap in an amount's global indexes.
    let mut gap = records.clone();
    gap.remove(&codec::key("j", KeyPart::AmountIndex(DENOMINATION, 3))).unwrap();
    cases.push((
        gap,
        Header { key_output_records: header.key_output_records - 1, ..template_of(&header) },
        "without a gap",
    ));
    // A table a snapshot may not carry.
    let mut foreign_table = records.clone();
    foreign_table.insert(codec::key("b", KeyPart::U64(DENOMINATION)), codec::value_u32("b", 1));
    cases.push((foreign_table, template_of(&header), "belonging to no table a snapshot may carry"));
    // An amount count the payload does not have.
    cases.push((
        records.clone(),
        Header { key_output_amounts_count: header.key_output_amounts_count + 1, ..template_of(&header) },
        "distinct amounts and its header claims",
    ));

    for (i, (records, template, want)) in cases.into_iter().enumerate() {
        let file = scratch(&format!("case-{i}.litesnap"));
        let written = write_snapshot(&file, &records, template);
        refuse(&mut empty_lite(H), &file, &bless(&written), want);
        let _ = std::fs::remove_file(&file);
    }
    for f in [&path, &foreign, &tampered, &miscounted] {
        let _ = std::fs::remove_file(f);
    }
}

/// `header` with its digest cleared, for writing a variant of the file.
fn template_of(header: &Header) -> Header {
    Header { payload_digest: [0; 32], ..*header }
}

fn patch(path: &Path, at: usize, bytes: &[u8]) {
    let mut data = std::fs::read(path).unwrap();
    data[at..at + bytes.len()].copy_from_slice(bytes);
    std::fs::write(path, data).unwrap();
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// Export `state` at `h` into memory, handing the state back.
fn export_bytes(state: ChainState<MemStore>, h: u32) -> (ChainState<MemStore>, Header, Vec<u8>) {
    let lock = RwLock::new(state);
    let (header, out) = export_to(&lock, std::io::Cursor::new(Vec::new()), h, &ExportControl::new()).unwrap();
    (lock.into_inner().unwrap(), header, out.into_inner())
}

/// A fresh state under `cfg` that has synced `blocks` from genesis.
///
/// Two chains built separately are two different chains — a ring signature
/// takes fresh randomness, so every spend has a different hash — which is why
/// a second node is made by replaying the first one's blocks, not by building
/// its own.
fn replay(cfg: Config, blocks: &[Blobs]) -> ChainState<MemStore> {
    let mut state = ChainState::open_or_genesis(MemStore::default(), cfg, Checkpoints::none()).unwrap();
    state.set_clock(Some(NOW));
    for (blob, txs) in blocks {
        state.add_block(blob, txs).unwrap();
    }
    state
}

/// The exporter writes, byte for byte, what the model of the C++ walk writes —
/// and a node's tip, and whether it is full or lite, change nothing.
#[test]
fn the_exporter_writes_the_cpp_walk_whatever_the_tip_or_the_mode() {
    let tall = build_chain(TOP);
    let (records, template) = cpp_records(&tall.state, H);
    let reference = scratch("reference.litesnap");
    let expected = write_snapshot(&reference, &records, template);
    let reference_bytes = std::fs::read(&reference).unwrap();

    let blocks = tall.blocks.clone();
    let (_, header, bytes) = export_bytes(tall.state, H);
    assert_eq!(header, expected);
    assert_eq!(bytes, reference_bytes, "byte for byte what the C++ walk writes");
    assert!(header.key_output_amounts_count >= 2 && header.key_image_records >= 10, "{header:?}");

    // Another node on the same chain, at another tip above the line: the same
    // file.
    let short = replay(serving(), &blocks[..H as usize + 2]);
    let (_, short_header, short_bytes) = export_bytes(short, H);
    assert_eq!(short_header, header);
    assert_eq!(short_bytes, bytes);

    // A lite node that synced the same chain, bodies dropped below the line:
    // the same file.
    let (_, lite_header, lite_bytes) = export_bytes(replay(lite(H), &blocks), H);
    assert_eq!(lite_header, header);
    assert_eq!(lite_bytes, bytes);
    let _ = std::fs::remove_file(&reference);
}

/// Export, import into an empty state, export again: the same file. Then the
/// imported node follows the chain to its top, and it and the node that built
/// the chain export the same digest over the whole of it.
#[test]
fn a_snapshot_survives_an_import_and_an_export_unchanged() {
    let Chain { state, blocks, .. } = build_chain(TOP);
    let (state, header, bytes) = export_bytes(state, H);
    let path = scratch("round-trip.litesnap");
    std::fs::write(&path, &bytes).unwrap();

    let mut imported = empty_lite(H);
    import(&mut imported, &path, &bless(&header)).unwrap();
    let (mut imported, again, again_bytes) = export_bytes(imported, H);
    assert_eq!(again, header);
    assert_eq!(again_bytes, bytes, "an imported state exports the file it was imported from");

    for (blob, txs) in &blocks[H as usize - 1..] {
        imported.add_block(blob, txs).unwrap();
    }
    let (_, theirs, _) = export_bytes(state, TOP + 1);
    let (_, ours, _) = export_bytes(imported, TOP + 1);
    assert_eq!(ours, theirs, "the imported state and the original agree on every record of the chain");
    let _ = std::fs::remove_file(&path);
}
