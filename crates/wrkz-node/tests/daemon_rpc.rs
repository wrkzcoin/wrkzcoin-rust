// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The daemon end to end, offline: the P2P engine and the RPC server over one
//! shared chain state and one shared pool, driven by **the wallet's own client**
//! (`wrkz_wallet::daemon::Daemon`).
//!
//! That client is the second oracle the RPC has to satisfy: it is the code that
//! drives the C++ daemon today, and every field name and optional field in it is
//! part of the wire contract. If it can sync against `node-fin.wrkz.work` and
//! not against us, we are wrong.
//!
//! The chain is synthetic — the records `ChainState` itself writes, put into a
//! `MemStore` at a height above every fork — because the seven wallet endpoints
//! only have anything to say above the mined-money unlock window, and no real
//! chain of that height can be built in a test.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use wrkz_chain::records::{BlockInfo, OutputRecord};
use wrkz_chain::{keys, records, ChainState, Checkpoints, Config};
use wrkz_mempool::TransactionPool;
use wrkz_node::mempool::SharedMempool;
use wrkz_node::{Node, NodeConfig};
use wrkz_primitives::block::{BlockTemplate, ParentBlock};
use wrkz_primitives::tx::{
    append_merge_mining_tag, build_extra, BaseTransaction, Input, MergeMiningTag, Output, Transaction,
    TransactionPrefix,
};
use wrkz_primitives::Hash;
use wrkz_rpc::node::{ChainNode, P2pSnapshot};
use wrkz_rpc::server::{self, RunningServer, ServerConfig};
use wrkz_storage::{KvStore, MemStore};
use wrkz_wallet::daemon::{Daemon, SyncRequest};

/// Above every fork height and above the last checkpoint, so nothing is skipped.
const TIP: u32 = 4_400_000;
const HISTORY: u32 = 300;
const TIP_TIME: u64 = 1_800_000_000;
const SPACING: u64 = 59;
/// The denomination the seeded spendable outputs carry.
const AMOUNT: u64 = 1_000_000;
const OUTPUTS: u32 = 8;

const MINER_ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";

/// A wallet, as `crates/wrkz-chain/tests/synthetic.rs` builds one.
struct Wallet {
    spend_secret: Hash,
    spend_public: Hash,
    view_secret: Hash,
    view_public: Hash,
}

impl Wallet {
    fn from_seed(seed: u8) -> Self {
        let (spend_secret, spend_public) =
            wrkz_pow::curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(&[seed, 0xA1]));
        let (view_secret, view_public) = wrkz_pow::curve::generate_view_from_spend(&spend_secret);
        Self { spend_secret, spend_public, view_secret, view_public }
    }
}

fn derive_output(to: &Wallet, tx_secret: &Hash, tx_public: &Hash, index: u64) -> (Hash, Hash) {
    let sender = wrkz_pow::curve::generate_key_derivation(&to.view_public, tx_secret).expect("derivation");
    let public = wrkz_pow::curve::derive_public_key(&sender, index, &to.spend_public).expect("output key");
    let receiver = wrkz_pow::curve::generate_key_derivation(tx_public, &to.view_secret).expect("derivation");
    let secret = wrkz_pow::curve::derive_secret_key(&receiver, index, &to.spend_secret);
    (public, secret)
}

fn seed_hash(index: u32) -> Hash {
    wrkz_pow::cn_fast_hash(&[b"wrkz-node daemon test".as_slice(), &index.to_le_bytes()].concat())
}

/// A chain whose tip is [`TIP`], with [`OUTPUTS`] unlocked outputs of
/// [`AMOUNT`] so `/getrandom_outs` has something real to serve, and a stored
/// raw block at the tip so the wallet-sync endpoints have a block to return.
fn synthetic_chain() -> (ChainState<MemStore>, Hash) {
    let miner = Wallet::from_seed(2);
    let (tx_secret, tx_public) =
        wrkz_pow::curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(b"daemon test coinbase"));
    let (out_key, _) = derive_output(&miner, &tx_secret, &tx_public, 0);

    // The tip block: a v7 block with the daemon template's parent block, the
    // same shape `wrkz-chain`'s synthetic tests build.
    let coinbase = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: TIP as u64 + 40,
            inputs: vec![Input::Base { block_index: TIP as u64 }],
            outputs: vec![Output { amount: AMOUNT, key: out_key }],
            extra: build_extra(&tx_public, None, None).expect("extra"),
        },
        signatures: Vec::new(),
    };
    let mut block = BlockTemplate {
        major_version: 7,
        minor_version: 0,
        timestamp: TIP_TIME,
        previous_block_hash: seed_hash(TIP - 1),
        nonce: 1,
        parent_block: None,
        base_transaction: coinbase,
        transaction_hashes: Vec::new(),
    };
    let aux = block.auxiliary_header_hash().expect("aux hash");
    let mut parent_extra = Vec::new();
    append_merge_mining_tag(&mut parent_extra, &MergeMiningTag { depth: 0, merkle_root: aux });
    block.parent_block = Some(ParentBlock::new(
        0,
        0,
        block.previous_block_hash,
        1,
        Vec::new(),
        BaseTransaction {
            prefix: TransactionPrefix {
                version: 0,
                unlock_time: 0,
                inputs: vec![],
                outputs: vec![],
                extra: parent_extra,
            },
        },
        Vec::new(),
    ));
    let blob = block.to_bytes().expect("the block serializes");
    let tip_hash = block.hash().expect("the block hashes");
    let coinbase_hash = block.base_transaction.hash().expect("the coinbase hashes");
    let coinbase_size = block.base_transaction.to_bytes().expect("bytes").len() as u32;

    let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    let mut cumulative = 10_000_000_000_000u64;
    for i in (TIP - HISTORY)..TIP {
        let info = BlockInfo {
            block_hash: seed_hash(i),
            timestamp: TIP_TIME - (TIP - i) as u64 * SPACING,
            block_size: coinbase_size,
            cumulative_difficulty: cumulative,
            already_generated_coins: 30_000_000_000_000,
            already_generated_transactions: i as u64 + 1,
        };
        ops.push((keys::block_info(i), Some(info.encode())));
        ops.push((keys::hash_to_index(&info.block_hash), Some(i.to_le_bytes().to_vec())));
        ops.push((keys::block_tx_hashes(i), Some(records::encode_hashes(&[seed_hash(i)]))));
        cumulative += 1;
    }
    let tip = BlockInfo {
        block_hash: tip_hash,
        timestamp: TIP_TIME,
        block_size: coinbase_size,
        cumulative_difficulty: cumulative,
        already_generated_coins: 30_000_000_000_000 + AMOUNT,
        already_generated_transactions: TIP as u64 + 2,
    };
    ops.push((keys::block_info(TIP), Some(tip.encode())));
    ops.push((keys::hash_to_index(&tip_hash), Some(TIP.to_le_bytes().to_vec())));
    ops.push((keys::block_tx_hashes(TIP), Some(records::encode_hashes(&[coinbase_hash]))));
    ops.push((keys::transaction_index(&coinbase_hash), Some(TIP.to_le_bytes().to_vec())));
    ops.push((keys::raw_block(TIP), Some(records::encode_raw_block(&blob, &[]))));
    ops.push((keys::block_outputs(TIP), Some(records::encode_output_refs(&[(AMOUNT, OUTPUTS)]))));

    // Spendable outputs of one denomination, mature and unlocked, so
    // `/getrandom_outs` returns real ring members a wallet can build with.
    let owner = Wallet::from_seed(1);
    let (out_secret, out_public) =
        wrkz_pow::curve::generate_deterministic_keys(&wrkz_pow::cn_fast_hash(b"daemon test outputs"));
    for i in 0..OUTPUTS {
        let (public_key, _) = derive_output(&owner, &out_secret, &out_public, i as u64);
        let record = OutputRecord {
            public_key,
            unlock_time: 0,
            transaction_hash: seed_hash(TIP - HISTORY),
            output_index: i as u16,
            block_index: TIP - HISTORY,
        };
        ops.push((keys::output(AMOUNT, i), Some(record.encode())));
    }
    // Plus the tip's coinbase output, at global index OUTPUTS.
    ops.push((
        keys::output(AMOUNT, OUTPUTS),
        Some(
            OutputRecord {
                public_key: out_key,
                unlock_time: TIP as u64 + 40,
                transaction_hash: coinbase_hash,
                output_index: 0,
                block_index: TIP,
            }
            .encode(),
        ),
    ));
    ops.push((keys::output_count(AMOUNT), Some((OUTPUTS + 1).to_le_bytes().to_vec())));

    ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_TIP), Some(TIP.to_le_bytes().to_vec())));

    let mut store = MemStore::default();
    store.write_batch(ops).expect("the seed writes");
    let cfg = Config { store_raw_blocks: true, unwind_history: u32::MAX, recent_window: 256, ..Config::default() };
    let mut chain = ChainState::open(store, cfg, Checkpoints::mainnet()).expect("the seeded state opens");
    chain.set_clock(Some(TIP_TIME + 600));
    assert_eq!(chain.tip_index(), Some(TIP));
    (chain, coinbase_hash)
}

/// The whole daemon, in process: the engine, the RPC, one chain, one pool.
struct Harness {
    server: RunningServer,
    /// Kept so the engine's threads live as long as the test.
    _node: Node<MemStore, SharedMempool<MemStore>>,
    coinbase_hash: Hash,
    url: String,
}

fn start_daemon() -> Harness {
    let (chain, coinbase_hash) = synthetic_chain();
    let chain = Arc::new(RwLock::new(chain));
    let pool = Arc::new(Mutex::new(TransactionPool::new(Default::default())));
    let mempool = SharedMempool::new(Arc::clone(&pool), Arc::clone(&chain));

    let dir =
        std::env::temp_dir().join(format!("wrkz-daemon-test-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = NodeConfig { data_dir: dir, listen: false, use_default_seeds: false, p2p_port: 0, ..Default::default() };
    let mut node = Node::with_shared_chain(Arc::clone(&chain), mempool, cfg);
    node.start().expect("the engine starts without a listener");

    let rpc = ChainNode::shared(chain, pool, Box::new(P2pSnapshot::standalone));
    let server = server::start(
        Arc::new(rpc) as Arc<dyn wrkz_rpc::NodeApi>,
        ServerConfig { bind: "127.0.0.1:0".into(), ..Default::default() },
    )
    .expect("the rpc binds");
    let url = format!("http://{}", server.local_addr());
    Harness { server, _node: node, coinbase_hash, url }
}

fn client(h: &Harness) -> Daemon {
    Daemon::new(&h.url).expect("the wallet client accepts our URL")
}

// ---------------------------------------------------------------------------
// the seven endpoints the wallet drives
// ---------------------------------------------------------------------------

/// `/info`, parsed by the wallet's own `Info` struct.
#[test]
fn the_wallet_client_reads_our_info() {
    let h = start_daemon();
    let info = client(&h).info().expect("/info parses and says OK");
    assert_eq!(info.height, TIP as u64 + 1, "a count");
    assert_eq!(info.top_index(), TIP as u64, "the wallet subtracts one");
    assert_eq!(info.network_height, TIP as u64 + 1);
    assert!(info.synced);
    assert_eq!(info.lite_start_height, 0);
    assert!(info.supports("skipEmptyBlocks") && info.supports("base64") && info.supports("heightRange"));
    assert_eq!(
        info.compression.as_deref(),
        Some("gzip"),
        "the server gzips for clients that accept it, and says so as a C++ node built with zlib does"
    );
    assert_eq!(info.version.as_deref(), Some("0.4.8"));
    assert_eq!(info.upgrade_heights.first().copied(), Some(1));
    assert_eq!(info.supported_height, Some(4_500_000));
    assert_eq!(info.status, "OK");
    drop(h);
}

/// `/getwalletsyncdata`, parsed by `WalletSyncData`, including `topBlock` and
/// `scannedToHeight`.
#[test]
fn the_wallet_client_syncs_blocks_from_us() {
    let h = start_daemon();
    let d = client(&h);
    let data = d
        .wallet_sync_data(&SyncRequest { start_height: TIP as u64, block_count: 100, ..Default::default() })
        .expect("/getwalletsyncdata parses");
    assert_eq!(data.items.len(), 1);
    let block = &data.items[0];
    assert_eq!(block.block_height, TIP as u64);
    assert_eq!(block.block_timestamp, TIP_TIME);
    let coinbase = block.coinbase_tx.as_ref().expect("the coinbase is present by default");
    assert_eq!(coinbase.hash, hex::encode(h.coinbase_hash));
    assert_eq!(coinbase.outputs.len(), 1);
    assert_eq!(coinbase.outputs[0].amount, AMOUNT);
    assert_eq!(coinbase.outputs[0].key.len(), 64);
    assert!(coinbase.outputs[0].global_index.is_none(), "a daemon never fills this in");
    assert_eq!(coinbase.unlock_time, TIP as u64 + 40);
    assert_eq!(coinbase.tx_public_key.len(), 64);
    assert!(block.transactions.is_empty());
    assert_eq!(data.scanned_to_height, Some(TIP as u64));
    assert!(!data.synced, "blocks were returned");
    assert!(data.top_block.is_none());

    // Past the tip: no blocks, `synced`, and the top block so the wallet can
    // record the tip.
    let data = d
        .wallet_sync_data(&SyncRequest { start_height: TIP as u64 + 10, block_count: 100, ..Default::default() })
        .expect("parses");
    assert!(data.items.is_empty());
    assert!(data.synced);
    let top = data.top_block.expect("topBlock is present when synced");
    assert_eq!(top.height, TIP as u64);
    assert_eq!(top.hash.len(), 64);

    // The optional flags the wallet may send.
    let data = d
        .wallet_sync_data(&SyncRequest {
            start_height: TIP as u64,
            block_count: 100,
            skip_coinbase_transactions: true,
            skip_input_key_offsets: Some(true),
            skip_empty_blocks: Some(true),
            end_height: Some(TIP as u64 + 1),
            ..Default::default()
        })
        .expect("parses with every optional field set");
    assert_eq!(data.scanned_to_height, Some(TIP as u64), "coverage is reported even with nothing to send");
    drop(h);
}

/// `/getrawblocks`, parsed by `RawBlocks`; the bytes must re-parse as the block.
#[test]
fn the_wallet_client_pulls_raw_blocks_from_us() {
    let h = start_daemon();
    let raw = client(&h)
        .raw_blocks(&SyncRequest { start_height: TIP as u64, block_count: 1, ..Default::default() })
        .expect("/getrawblocks parses");
    assert_eq!(raw.items.len(), 1);
    assert!(raw.items[0].transactions.is_empty());
    let block = BlockTemplate::from_bytes(&hex::decode(&raw.items[0].block).unwrap()).expect("the bytes are a block");
    assert_eq!(block.major_version, 7);
    assert_eq!(block.timestamp, TIP_TIME);
    assert!(!raw.synced);
    drop(h);
}

/// `/get_global_indexes_for_range`, parsed by `GlobalIndexes`.
#[test]
fn the_wallet_client_fills_global_indexes_from_us() {
    let h = start_daemon();
    let g = client(&h).global_indexes_for_range(TIP as u64, TIP as u64 + 1).expect("parses");
    assert_eq!(g.indexes.len(), 1, "one entry, for the block's coinbase");
    assert_eq!(g.indexes[0].key, hex::encode(h.coinbase_hash));
    assert_eq!(g.indexes[0].value, vec![OUTPUTS as u64]);
    // The wallet asks for a ten-block window around the block of interest.
    let g = client(&h).global_indexes_for_range(TIP as u64 - 5, TIP as u64 + 1).expect("parses");
    assert_eq!(g.indexes.len(), 1, "the seeded history has no output records");
    drop(h);
}

/// `/getrandom_outs`, parsed by `RandomOuts`. These must be *real* outputs of
/// our own chain, or a wallet cannot build a ring.
#[test]
fn the_wallet_client_gets_real_ring_members_from_us() {
    let h = start_daemon();
    let outs = client(&h).random_outs(&[AMOUNT], 4).expect("/getrandom_outs parses");
    assert_eq!(outs.outs.len(), 1, "one entry per requested amount");
    assert_eq!(outs.outs[0].amount, AMOUNT);
    assert_eq!(outs.outs[0].outs.len(), 4, "four of the eight mature outputs");
    let mut indexes: Vec<u64> = outs.outs[0].outs.iter().map(|o| o.global_amount_index).collect();
    let sorted = {
        let mut v = indexes.clone();
        v.sort_unstable();
        v
    };
    assert_eq!(indexes, sorted, "the C++ sorts them before answering");
    indexes.dedup();
    assert_eq!(indexes.len(), 4, "distinct");
    for out in &outs.outs[0].outs {
        assert_eq!(out.out_key.len(), 64);
        assert!(
            out.global_amount_index < OUTPUTS as u64,
            "the tip's own coinbase is inside the unlock window and must not be offered as a decoy"
        );
    }

    // Asking for more than exist returns what there is, not an error.
    let outs = client(&h).random_outs(&[AMOUNT], 100).expect("parses");
    assert_eq!(outs.outs[0].outs.len(), OUTPUTS as usize);
    // A denomination with nothing on chain is an empty list, not an error.
    let outs = client(&h).random_outs(&[7], 3).expect("parses");
    assert_eq!(outs.outs.len(), 1);
    assert!(outs.outs[0].outs.is_empty());
    drop(h);
}

/// `/sendrawtransaction`, parsed by `SendResult`: HTTP 200 either way.
#[test]
fn the_wallet_client_sees_our_send_result() {
    let h = start_daemon();
    let d = client(&h);
    let r = d.send_raw_transaction("zz").expect("HTTP 200 even on failure");
    assert_eq!(r.status, "Failed");
    assert_eq!(r.error.as_deref(), Some("Failed to parse transaction from hex buffer"));

    let r = d.send_raw_transaction("00").expect("HTTP 200");
    assert_eq!(r.status, "Failed");
    assert_eq!(r.error.as_deref(), Some("Could not deserialize transaction"), "the pool's own message");
    drop(h);
}

/// `/get_transactions_status`, parsed by `TransactionsStatus`.
#[test]
fn the_wallet_client_reads_transaction_status_from_us() {
    let h = start_daemon();
    let mined = hex::encode(h.coinbase_hash);
    let unknown = "00".repeat(32);
    let s = client(&h).transactions_status(&[mined.clone(), unknown.clone()]).expect("parses");
    assert_eq!(s.transactions_in_block, vec![mined]);
    assert_eq!(s.transactions_in_pool, Vec::<String>::new());
    assert_eq!(s.transactions_unknown, vec![unknown]);
    drop(h);
}

// ---------------------------------------------------------------------------
// mining, and the shared pool
// ---------------------------------------------------------------------------

/// `getblocktemplate` and `getlastblockheader` through the wallet client, which
/// is what a pool and `xmrig` drive.
#[test]
fn the_wallet_client_can_drive_mining_against_us() {
    let h = start_daemon();
    let d = client(&h);
    let header = d.last_block_header().expect("getlastblockheader parses");
    assert_eq!(header.height, TIP as u64);
    assert_eq!(header.depth, 0);
    assert_eq!(header.num_txes, 1);
    assert_eq!(header.major_version, 7);
    assert_eq!(header.reward, AMOUNT);

    let same = d.block_header_by_height(TIP as u64).expect("getblockheaderbyheight parses");
    assert_eq!(same.hash, header.hash);

    let t = d.block_template(MINER_ADDRESS, 8).expect("getblocktemplate parses");
    assert_eq!(t.height, TIP as u64 + 1, "a count");
    assert_eq!(t.status, "OK");
    let blob = hex::decode(&t.blocktemplate_blob).expect("hex");
    let offset = t.reserved_offset as usize;
    assert_eq!(&blob[offset..offset + 8], &[0u8; 8], "the reserved bytes are where the offset says");
    let parsed = BlockTemplate::from_bytes(&blob).expect("the template parses");
    assert_eq!(parsed.previous_block_hash, hex::decode(&header.hash).unwrap()[..], "built on our tip");
    drop(h);
}

/// The daemon must refuse to serve a state a windowed replay wrote, and accept
/// one a linear replay wrote.
#[test]
fn a_windowed_replay_state_is_refused_and_a_linear_one_is_served() {
    use wrkz_node::daemon::{check_state_tag, TAG_LINEAR, TAG_WINDOWS};
    let (mut chain, _) = synthetic_chain();
    assert!(check_state_tag(chain.tag().unwrap().as_deref()).is_ok(), "an untagged state is a daemon's own");
    chain.set_tag(TAG_LINEAR).unwrap();
    assert!(check_state_tag(chain.tag().unwrap().as_deref()).is_ok(), "a linear replay is a complete chain");
    chain.set_tag(TAG_WINDOWS).unwrap();
    let e = check_state_tag(chain.tag().unwrap().as_deref()).unwrap_err();
    assert!(e.contains("windowed replay"), "{e}");
}

/// One pool, seen by both halves: what the RPC accepts is what the engine
/// relays and what the template builder mines.
#[test]
fn the_rpc_and_the_engine_share_one_pool() {
    let (chain, _) = synthetic_chain();
    let chain = Arc::new(RwLock::new(chain));
    let pool = Arc::new(Mutex::new(TransactionPool::new(Default::default())));
    let engine_view = SharedMempool::new(Arc::clone(&pool), Arc::clone(&chain));
    let rpc = ChainNode::shared(Arc::clone(&chain), Arc::clone(&pool), Box::new(P2pSnapshot::standalone));

    let (rpc_chain, rpc_pool) = rpc.handles();
    assert!(Arc::ptr_eq(&rpc_chain, &chain), "the RPC reads the same chain");
    assert!(Arc::ptr_eq(&rpc_pool, &engine_view.handle()), "and the same pool");

    // Nothing accepted yet, so nothing to relay.
    assert!(rpc.take_relay_queue().is_empty());
}

/// The RPC keeps answering while the engine is running, and the engine keeps
/// running while the RPC is answering: neither lock starves the other.
#[test]
fn rpc_reads_and_the_engine_run_at_the_same_time() {
    let h = start_daemon();
    let url = h.url.clone();
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let url = url.clone();
            std::thread::spawn(move || {
                let d = Daemon::new(&url).unwrap();
                for _ in 0..20 {
                    d.height().expect("/height answers throughout");
                }
            })
        })
        .collect();
    let started = std::time::Instant::now();
    for w in workers {
        w.join().expect("no worker panicked");
    }
    assert!(started.elapsed() < Duration::from_secs(20), "160 reads took {:?}", started.elapsed());
    // And the server is still healthy afterwards.
    assert_eq!(client(&h).height().expect("still serving").height, TIP as u64 + 1);
    assert_eq!(h.server.local_addr().ip().to_string(), "127.0.0.1");
    drop(h);
}
