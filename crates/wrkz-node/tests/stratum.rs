// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The stratum server end to end, over a chain built from genesis: a miner
//! logs in, hashes the job it is handed exactly as xmrig does — the nonce
//! written at offset 39 of the blob, the algorithm the job names — submits,
//! and the block lands on the chain and in the queue the daemon loop
//! announces from.
//!
//! Block 1 is major version 1 at difficulty 1, so every nonce is a block and
//! the test never searches; the v7 half (the merge-mining tag, the parent
//! block's hashing blob) is covered by the unit tests in `src/stratum.rs`.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use wrkz_chain::{ChainState, Checkpoints};
use wrkz_mempool::TransactionPool;
use wrkz_node::stratum::{encode_target, StratumConfig, StratumServer};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::tx::{Input, Transaction, TransactionPrefix};
use wrkz_primitives::Hash;
use wrkz_rpc::api::{
    self, BlockDetails, BlockHeaderInfo, BlockListEntry, BlockTemplateAnswer, HeightSnapshot, InfoSnapshot, PeerLists,
    PoolChanges, PoolTransactionSummary, QueryBlocksLite, RawBlocks, SubmitOutcome, SyncRequest, TransactionsStatus,
    WalletSyncData,
};
use wrkz_rpc::node::{serving_config, ChainNode};
use wrkz_rpc::NodeApi;
use wrkz_storage::MemStore;

const MINER_ADDRESS: &str =
    "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue";

fn genesis_node() -> Arc<ChainNode<MemStore>> {
    let chain =
        ChainState::open_or_genesis(MemStore::default(), serving_config(), Checkpoints::mainnet()).expect("genesis");
    Arc::new(ChainNode::standalone(chain, TransactionPool::new(Default::default())))
}

/// On an ephemeral loopback port, ready to mine unless told otherwise.
fn start(node: &Arc<ChainNode<MemStore>>, cfg: StratumConfig, ready: bool) -> StratumServer {
    StratumServer::start(
        Arc::clone(node) as Arc<dyn NodeApi>,
        &StratumConfig { port: 0, ..cfg },
        Box::new(move || ready),
    )
    .expect("the stratum server binds")
}

struct Miner {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Miner {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connects");
        stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        Self { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.writer, "{v}").expect("the server is reading");
    }

    /// The next line, or `None` once the server has closed the connection.
    fn read(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))),
        }
    }

    /// The answer to request `id`, past any job notifications.
    fn reply(&mut self, id: u64) -> Value {
        loop {
            let v = self.read().expect("the connection is still open");
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "id": id, "jsonrpc": "2.0", "method": method, "params": params }));
        self.reply(id)
    }

    /// The next `job` notification.
    fn job(&mut self) -> Value {
        loop {
            let v = self.read().expect("the connection is still open");
            if v["method"] == "job" {
                return v["params"].clone();
            }
        }
    }

    fn login(&mut self, address: &str) -> Value {
        self.call(1, "login", json!({ "login": address, "pass": "x", "agent": "test-miner/1.0" }))
    }
}

/// What a stratum miner does with a job: the nonce into the blob at offset 39,
/// then the algorithm the job names.
fn hash_job(job: &Value, nonce: u32) -> (String, String) {
    let mut blob = hex::decode(job["blob"].as_str().expect("a blob")).expect("hex");
    blob[39..43].copy_from_slice(&nonce.to_le_bytes());
    let major = match job["algo"].as_str().expect("an algorithm") {
        "cn/0" => 1,
        "cn/upx2" => 7,
        other => panic!("no test hash for {other}"),
    };
    let hash = wrkz_pow::pow_hash_for_block_version(major, &blob).expect("a hash");
    (hex::encode(nonce.to_le_bytes()), hex::encode(hash))
}

fn error_message(reply: &Value) -> &str {
    assert!(reply["result"].is_null(), "an error carries no result: {reply}");
    assert_eq!(reply["error"]["code"], -1);
    reply["error"]["message"].as_str().expect("a message")
}

#[test]
fn a_miner_logs_in_mines_a_block_and_the_block_is_queued_for_announcement() {
    let node = genesis_node();
    let server = start(&node, StratumConfig::default(), true);
    let mut miner = Miner::connect(server.local_addr());

    let login = miner.login(MINER_ADDRESS);
    assert!(login["error"].is_null(), "{login}");
    let result = &login["result"];
    assert_eq!(result["status"], "OK");
    assert!(!result["id"].as_str().unwrap().is_empty());
    assert_eq!(result["extensions"], json!([]));
    let job = result["job"].clone();
    assert_eq!(job["height"], 1, "a count: the block being mined");
    assert_eq!(job["algo"], "cn/0", "block 1 is major version 1");
    assert_eq!(job["target"], "ffffffffffffffff", "difficulty 1");

    let keepalive = miner.call(2, "keepalived", json!({}));
    assert_eq!(keepalive["result"]["status"], "KEEPALIVED");
    // Until the tip moves, getjob hands back the job the miner already has.
    let again = miner.call(3, "getjob", json!({}));
    assert_eq!(again["result"]["job_id"], job["job_id"]);
    assert_eq!(error_message(&miner.call(4, "mine_harder", json!({}))), "Unknown method");

    // A hash that is not the job's under its algorithm is refused as such,
    // and the nonce is spent: sending it again is a duplicate.
    let wrong = json!({ "job_id": job["job_id"], "nonce": "01000000", "result": "00".repeat(32) });
    assert_eq!(error_message(&miner.call(5, "submit", wrong.clone())), "Invalid result");
    assert_eq!(error_message(&miner.call(6, "submit", wrong)), "Duplicate share");
    let bad_job = json!({ "job_id": "nope", "nonce": "02000000", "result": "" });
    assert_eq!(error_message(&miner.call(7, "submit", bad_job)), "Invalid job id");
    let bad_nonce = json!({ "job_id": job["job_id"], "nonce": "0200", "result": "" });
    assert_eq!(error_message(&miner.call(8, "submit", bad_nonce)), "Malformed nonce");

    let (nonce, hash) = hash_job(&job, 2);
    let found = miner.call(9, "submit", json!({ "job_id": job["job_id"], "nonce": nonce, "result": hash }));
    assert!(found["error"].is_null(), "{found}");
    assert_eq!(found["result"]["status"], "OK");
    assert_eq!(node.top_index(), 1, "the block is on the chain");

    // Queued as a block, with its (no) transactions — never as a transaction.
    let mined = node.take_block_relay_queue();
    assert_eq!(mined.len(), 1);
    let block = BlockTemplate::from_bytes(&mined[0].block).expect("the queued block parses");
    assert_eq!(block.nonce, 2);
    assert_eq!(node.block_hash_by_index(1).unwrap(), Some(block.hash().unwrap()));
    assert!(mined[0].transactions.is_empty());
    assert!(node.take_relay_queue().is_empty());

    // And the miner is moved on to the next height without asking.
    let next = miner.job();
    assert_eq!(next["height"], 2);
    assert_ne!(next["job_id"], job["job_id"]);
}

/// A chain that is only a counter, for tests that need blocks to land faster
/// than a real chain's difficulty allows: every template is a version 1 block
/// at difficulty 1, so any nonce is a block, and a block on the current tip is
/// accepted and moves it. Only what the stratum server calls is implemented.
struct CounterChain {
    tip: AtomicU64,
    /// How long a template takes to build: the window in which the two threads
    /// that hand out jobs can overlap.
    template_time: Duration,
}

impl CounterChain {
    fn new(template_time: Duration) -> Arc<Self> {
        Arc::new(Self { tip: AtomicU64::new(0), template_time })
    }

    fn advance(&self) {
        self.tip.fetch_add(1, Ordering::SeqCst);
    }

    fn hash_at(index: u64) -> Hash {
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&index.to_le_bytes());
        hash
    }

    fn serve(self: &Arc<Self>, cfg: StratumConfig) -> StratumServer {
        let api = Arc::clone(self) as Arc<dyn NodeApi>;
        StratumServer::start(api, &StratumConfig { port: 0, ..cfg }, Box::new(|| true)).expect("binds")
    }
}

impl NodeApi for CounterChain {
    fn top_index(&self) -> u64 {
        self.tip.load(Ordering::SeqCst)
    }

    fn block_hash_by_index(&self, index: u64) -> api::Result<Option<Hash>> {
        Ok((index <= self.top_index()).then(|| Self::hash_at(index)))
    }

    fn height(&self) -> HeightSnapshot {
        let height = self.top_index() + 1;
        HeightSnapshot { height, network_height: height }
    }

    fn block_template(&self, _address: &str, reserve: &[u8]) -> api::Result<Result<BlockTemplateAnswer, String>> {
        std::thread::sleep(self.template_time);
        let top = self.top_index();
        let block = BlockTemplate {
            major_version: 1,
            minor_version: 0,
            timestamp: 1_800_000_000,
            previous_block_hash: Self::hash_at(top),
            nonce: 0,
            parent_block: None,
            base_transaction: Transaction {
                prefix: TransactionPrefix {
                    version: 1,
                    unlock_time: top + 41,
                    inputs: vec![Input::Base { block_index: top + 1 }],
                    outputs: Vec::new(),
                    extra: reserve.to_vec(),
                },
                signatures: Vec::new(),
            },
            transaction_hashes: Vec::new(),
        };
        let blob = block.to_bytes().expect("a template serializes");
        Ok(Ok(BlockTemplateAnswer { blob, difficulty: 1, height: top + 1, tx_public_key: [0; 32] }))
    }

    fn submit_block(&self, blob: &[u8]) -> api::Result<SubmitOutcome> {
        let block = BlockTemplate::from_bytes(blob).expect("the server submits what it was handed");
        let top = self.top_index();
        let on_tip = block.previous_block_hash == Self::hash_at(top);
        if on_tip && self.tip.compare_exchange(top, top + 1, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            Ok(SubmitOutcome::Added { relay: true })
        } else {
            Ok(SubmitOutcome::NotAccepted)
        }
    }

    fn info(&self) -> api::Result<InfoSnapshot> {
        unimplemented!("not used by the stratum server")
    }
    fn peers(&self) -> PeerLists {
        unimplemented!("not used by the stratum server")
    }
    fn is_synced(&self) -> bool {
        unimplemented!("not used by the stratum server")
    }
    fn block_header_by_hash(&self, _: &Hash) -> api::Result<Option<BlockHeaderInfo>> {
        unimplemented!("not used by the stratum server")
    }
    fn block_header_by_index(&self, _: u64) -> api::Result<Option<BlockHeaderInfo>> {
        unimplemented!("not used by the stratum server")
    }
    fn block_list(&self, _: u64) -> api::Result<Vec<BlockListEntry>> {
        unimplemented!("not used by the stratum server")
    }
    fn block_details(&self, _: &Hash) -> api::Result<Option<BlockDetails>> {
        unimplemented!("not used by the stratum server")
    }
    fn wallet_sync_data(&self, _: &SyncRequest) -> api::Result<WalletSyncData> {
        unimplemented!("not used by the stratum server")
    }
    fn raw_blocks(&self, _: &SyncRequest) -> api::Result<RawBlocks> {
        unimplemented!("not used by the stratum server")
    }
    fn global_indexes_for_range(&self, _: u64, _: u64) -> api::Result<Vec<(Hash, Vec<u64>)>> {
        unimplemented!("not used by the stratum server")
    }
    fn transaction_global_indexes(&self, _: &Hash) -> api::Result<Option<Vec<u32>>> {
        unimplemented!("not used by the stratum server")
    }
    fn random_outputs(&self, _: u64, _: u16) -> api::Result<Result<Vec<(u32, Hash)>, String>> {
        unimplemented!("not used by the stratum server")
    }
    fn add_transaction_to_pool(&self, _: &[u8]) -> Result<(), String> {
        unimplemented!("not used by the stratum server")
    }
    fn transactions_status(&self, _: &[Hash]) -> api::Result<TransactionsStatus> {
        unimplemented!("not used by the stratum server")
    }
    fn pool_transactions(&self) -> api::Result<Vec<PoolTransactionSummary>> {
        unimplemented!("not used by the stratum server")
    }
    fn pool_changes_lite(&self, _: &Hash, _: &[Hash]) -> api::Result<PoolChanges> {
        unimplemented!("not used by the stratum server")
    }
    fn query_blocks_lite(&self, _: &[Hash], _: u64) -> api::Result<QueryBlocksLite> {
        unimplemented!("not used by the stratum server")
    }
    fn transaction_blob(&self, _: &Hash) -> api::Result<Option<Vec<u8>>> {
        unimplemented!("not used by the stratum server")
    }
}

/// Every job a connection is handed is its own. Two threads hand them out —
/// the reader, re-jobbing the rig that just found a block, and the tip
/// watcher, re-jobbing everyone — and a message that described the newest job
/// rather than the one its sender built made two of them identical, which
/// xmrig answers by reconnecting ("duplicate job received").
///
/// The window the old code left was the few instructions between pushing a job
/// and describing it, too narrow to hit on demand, so this pins the invariant
/// under load rather than reproducing that race: a rig submitting on each job
/// the moment it arrives, so the reader's re-job after one block overlaps the
/// watcher's re-job for the block before.
#[test]
fn a_job_is_never_handed_out_twice_while_blocks_land_quickly() {
    const BLOCKS: u64 = 60;
    let node = CounterChain::new(Duration::from_millis(2));
    let server = node.serve(StratumConfig::default());
    let mut miner = Miner::connect(server.local_addr());

    let mut current = miner.login(MINER_ADDRESS)["result"]["job"].clone();
    let mut jobs = vec![current.clone()];
    let mut id = 2u64;
    while node.top_index() < BLOCKS {
        let (nonce, hash) = hash_job(&current, id as u32);
        let params = json!({ "job_id": current["job_id"], "nonce": nonce, "result": hash });
        miner.send(json!({ "id": id, "jsonrpc": "2.0", "method": "submit", "params": params }));
        // Its answer first, recording the jobs on the way: until then the tip
        // may not have moved, and a job that looks current may be about to go
        // stale.
        loop {
            let v = miner.read().expect("the connection stays open");
            assert!(v["error"].is_null(), "no share of this test is refused: {v}");
            if v["method"] == "job" {
                jobs.push(v["params"].clone());
            } else if v["id"] == json!(id) {
                break;
            }
        }
        id += 1;
        // Then on to the first job for the height after the new tip.
        loop {
            let v = miner.read().expect("the connection stays open");
            if v["method"] == "job" {
                jobs.push(v["params"].clone());
                if v["params"]["height"] == json!(node.top_index() + 1) {
                    current = v["params"].clone();
                    break;
                }
            }
        }
    }
    // Let the watcher's last re-job land, then take whatever is left.
    std::thread::sleep(Duration::from_millis(600));
    miner.writer.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    while let Some(v) = miner.read() {
        if v["method"] == "job" {
            jobs.push(v["params"].clone());
        }
    }

    let ids: Vec<&str> = jobs.iter().map(|j| j["job_id"].as_str().expect("a job id")).collect();
    let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len(), "a job id went out twice: {ids:?}");
    assert!(jobs.len() > BLOCKS as usize, "both threads re-jobbed the miner");
    let last = jobs.last().unwrap();
    assert_eq!(last["height"], json!(node.top_index() + 1), "the job a miner holds last is the newest");
}

/// A login's answer is the first line a miner reads. The tip watcher re-jobs
/// every logged-in connection; before building a job and queuing it became one
/// step per connection, a tip change while a login's own template was being
/// built could put a `job` notification — for the newer tip — ahead of the
/// login's answer, which then left the rig on the older job.
///
/// With a template taking 20 ms and the watcher looking every 200 ms, that
/// window opened on about one login in ten, so forty of them against a tip that
/// never stops moving find it reliably.
#[test]
fn a_login_is_answered_before_any_job_while_the_tip_moves() {
    let node = CounterChain::new(Duration::from_millis(20));
    let server = node.serve(StratumConfig { max_connections: 64, ..StratumConfig::default() });
    let running = Arc::new(AtomicBool::new(true));
    let ticker = {
        let (node, running) = (Arc::clone(&node), Arc::clone(&running));
        std::thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                node.advance();
                std::thread::sleep(Duration::from_millis(7));
            }
        })
    };
    for attempt in 0..40 {
        let mut miner = Miner::connect(server.local_addr());
        let params = json!({ "login": MINER_ADDRESS, "pass": "x", "agent": "test-miner/1.0" });
        miner.send(json!({ "id": 1, "jsonrpc": "2.0", "method": "login", "params": params }));
        let first = miner.read().expect("an answer");
        assert_eq!(first["id"], json!(1), "attempt {attempt}: a job arrived before the login's answer: {first}");
        assert_eq!(first["result"]["status"], "OK", "attempt {attempt}: {first}");
    }
    running.store(false, Ordering::SeqCst);
    ticker.join().unwrap();
}

#[test]
fn logins_are_checked_and_work_needs_one() {
    let node = genesis_node();
    let server = start(&node, StratumConfig::default(), true);

    let mut anonymous = Miner::connect(server.local_addr());
    assert_eq!(error_message(&anonymous.call(1, "getjob", json!({}))), "Unauthenticated");
    let submit = json!({ "job_id": "1", "nonce": "00000000", "result": "" });
    assert_eq!(error_message(&anonymous.call(2, "submit", submit)), "Unauthenticated");

    // A bad address is answered with the RPC's own message, then closed.
    let mut typo = Miner::connect(server.local_addr());
    let refused = typo.login("WrkzNotAnAddress");
    assert!(error_message(&refused).contains("wrong length"), "{refused}");
    assert!(typo.read().is_none(), "the connection is closed after the refusal");

    // Malformed JSON closes the connection without an answer.
    let mut garbage = Miner::connect(server.local_addr());
    garbage.writer.write_all(b"{ not json\n").unwrap();
    assert!(garbage.read().is_none());
}

#[test]
fn a_node_that_is_not_ready_turns_miners_away_but_keeps_them_connected() {
    let node = genesis_node();
    let server = start(&node, StratumConfig::default(), false);
    let mut miner = Miner::connect(server.local_addr());
    let refused = miner.login(MINER_ADDRESS);
    assert!(error_message(&refused).starts_with("Node is still synchronizing"), "{refused}");
    // Still connected: the rig retries on the same connection.
    let keepalive = miner.call(2, "keepalived", json!({}));
    assert_eq!(keepalive["result"]["status"], "KEEPALIVED");
}

#[test]
fn a_fixed_share_difficulty_sets_the_target_and_gates_shares() {
    let node = genesis_node();
    let server = start(&node, StratumConfig { share_difficulty: 1000, ..StratumConfig::default() }, true);
    let mut miner = Miner::connect(server.local_addr());
    let job = miner.login(MINER_ADDRESS)["result"]["job"].clone();
    assert_eq!(job["target"], encode_target(1000));

    // Whether this nonce clears 1000 is a property of its hash; the answer
    // must follow it either way.
    let (nonce, hash) = hash_job(&job, 7);
    let clears = wrkz_pow::check_hash(&hex::decode(&hash).unwrap().try_into().unwrap(), 1000);
    let reply = miner.call(2, "submit", json!({ "job_id": job["job_id"], "nonce": nonce, "result": hash }));
    if clears {
        assert_eq!(reply["result"]["status"], "OK");
        assert_eq!(node.top_index(), 1, "above the share difficulty and the network's: a block");
    } else {
        assert_eq!(error_message(&reply), "Low difficulty share");
        assert_eq!(node.top_index(), 0);
    }
}

#[test]
fn the_connection_cap_holds() {
    let node = genesis_node();
    let server = start(&node, StratumConfig { max_connections: 1, ..StratumConfig::default() }, true);
    let mut first = Miner::connect(server.local_addr());
    assert_eq!(first.login(MINER_ADDRESS)["result"]["status"], "OK");
    let mut second = Miner::connect(server.local_addr());
    assert!(second.read().is_none(), "the second connection is closed on arrival");
    assert_eq!(server.connections(), 1);
    // The first is untouched.
    assert_eq!(first.call(2, "keepalived", json!({}))["result"]["status"], "KEEPALIVED");
}

/// The oracle for the v7 half: mainnet block 4,213,000, which a real miner
/// found. Put its parent block back into the shape a daemon template hands
/// out — the merge-mining tag a zeroed placeholder — and sealing it must give
/// back that block byte for byte, its hashing blob must carry its nonce at
/// offset 39, and the work must clear the difficulty it was mined at.
#[test]
fn sealing_a_daemon_template_reproduces_a_real_mainnet_block() {
    use wrkz_node::stratum::{algorithm_name, seal_merge_mining_tag};
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/vectors/mainnet_rawblocks_4213000_v7.json");
    let vector: Value = serde_json::from_slice(&std::fs::read(path).expect("the vector")).expect("JSON");
    let blob = hex::decode(vector["items"][0]["block"].as_str().expect("a block")).expect("hex");
    let real = BlockTemplate::from_bytes(&blob).expect("the captured block parses");
    assert_eq!(real.major_version, 7);
    assert_eq!(algorithm_name(real.major_version), "cn/upx2");

    let mut template = real.clone();
    template.parent_block = Some(wrkz_mempool::template_builder::daemon_template_parent_block());
    assert_ne!(template.to_bytes().unwrap(), blob, "the placeholder is not the real tag");
    seal_merge_mining_tag(&mut template).expect("seals");
    assert_eq!(template.to_bytes().unwrap(), blob, "sealed, it is the block the network accepted");

    let hashing_blob = template.pow_input().unwrap();
    assert_eq!(&hashing_blob[39..43], &real.nonce.to_le_bytes());
    // spec/09's conformance row: the block's difficulty.
    assert!(template.check_proof_of_work(24_880_685).unwrap());
}

#[test]
fn stopping_the_server_closes_its_miners() {
    let node = genesis_node();
    let mut server = start(&node, StratumConfig::default(), true);
    let mut miner = Miner::connect(server.local_addr());
    assert_eq!(miner.login(MINER_ADDRESS)["result"]["status"], "OK");
    server.stop();
    assert!(miner.read().is_none());
}
