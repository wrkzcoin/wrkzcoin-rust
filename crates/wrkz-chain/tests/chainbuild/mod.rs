// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! A real chain, built block by block, and the C++-layout database a replay
//! reads it out of.
//!
//! `tests/synthetic.rs` builds one block at a time to exercise one rule at a
//! time; the import tests need a *run* of blocks with real spends across them,
//! plus the source database a linear replay would read. This is that, and it is
//! the same construction: the chain is seeded at 4,400,000, above every fork
//! height and above the last checkpoint (4,188,000), so the proof of work, the
//! ring signatures and every state rule really run. Blocks are mined at
//! difficulty 1, which any hash satisfies, so no work is needed.
//!
//! Every key, derivation, key image and ring signature comes from the real
//! curve functions in `wrkz-pow`.

use wrkz_chain::records::{BlockInfo, OutputRecord};
use wrkz_chain::{keys, records, ChainState, Checkpoints, Config};
use wrkz_pow::cn_fast_hash;
use wrkz_pow::curve;
use wrkz_primitives::block::{BlockTemplate, ParentBlock};
use wrkz_primitives::tx::{
    absolute_to_relative_offsets, append_merge_mining_tag, build_extra, BaseTransaction, Input, MergeMiningTag, Output,
    Transaction, TransactionPrefix,
};
use wrkz_primitives::Hash;
use wrkz_storage::codec::{self, KeyPart};
use wrkz_storage::records::{CachedBlockInfo, RawBlockRecord};
use wrkz_storage::{KvStore, MemStore};

/// The seeded top block index.
pub const TIP: u32 = 4_400_000;
/// Block infos written below the tip; must cover `replay::SEED_DEPTH`.
pub const SEED_LEN: u32 = wrkz_chain::replay::SEED_DEPTH;
pub const BASE: u32 = TIP - SEED_LEN + 1;
/// The solvetime that keeps LWMA-2 at the same difficulty.
pub const SPACING: u64 = 59;
pub const TIP_TIME: u64 = 1_800_000_000;
/// The validating node's clock. Far enough ahead that a few hundred blocks all
/// stay under the future-time limit.
pub const NOW: u64 = TIP_TIME + 1_000_000;
pub const TIP_CUMULATIVE: u64 = 10_000_000_000_000;
/// The amount of every seeded spendable output, and — since the flat reward
/// above `FIXED_REWARD_V1_HEIGHT` is exactly this — of an empty block's
/// coinbase output too, so the two share one global-index sequence.
pub const AMOUNT: u64 = 1_000_000;
/// A fee that clears the fee ladder and the transaction proof-of-work escape.
pub const FEE: u64 = 10_000;
/// Blocks a coinbase output stays locked (`CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW`).
pub const COINBASE_LOCK: u32 = 40;

pub fn config() -> Config {
    // `store_raw_blocks` off is what `wrkz-replay` defaults to: the source
    // database already holds the bodies.
    Config { store_raw_blocks: false, unwind_history: 512, recent_window: 256, ..Config::default() }
}

/// A wallet: a spend key pair and a view key pair.
pub struct Wallet {
    pub spend_secret: Hash,
    pub spend_public: Hash,
    pub view_secret: Hash,
    pub view_public: Hash,
}

impl Wallet {
    pub fn from_seed(seed: u8) -> Self {
        let (spend_secret, spend_public) = curve::generate_deterministic_keys(&cn_fast_hash(&[seed, 0xA1]));
        let (view_secret, view_public) = curve::generate_view_from_spend(&spend_secret);
        Self { spend_secret, spend_public, view_secret, view_public }
    }
}

/// One output this harness can spend.
#[derive(Clone, Copy, Debug)]
pub struct Spendable {
    pub amount: u64,
    pub global_index: u32,
    pub public_key: Hash,
    pub secret_key: Hash,
}

/// The one-time key pair of output `output_index` of a transaction paying `to`.
pub fn derive_output(to: &Wallet, tx_secret: &Hash, tx_public: &Hash, output_index: u64) -> (Hash, Hash) {
    let derivation = curve::generate_key_derivation(tx_public, &to.view_secret).expect("derivation");
    let public_key = curve::derive_public_key(&derivation, output_index, &to.spend_public).expect("public key");
    let receiver = curve::generate_key_derivation(&to.view_public, tx_secret).expect("receiver derivation");
    let secret_key = curve::derive_secret_key(&receiver, output_index, &to.spend_secret);
    (public_key, secret_key)
}

pub fn tx_keys(tag: &[u8]) -> (Hash, Hash) {
    curve::generate_deterministic_keys(&cn_fast_hash(&[b"wrkz import tx key".as_slice(), tag].concat()))
}

pub fn seed_hash(index: u32) -> Hash {
    cn_fast_hash(&[b"wrkz-chain import seed".as_slice(), &index.to_le_bytes()].concat())
}

/// A built block: everything a replay needs to store it and everything a test
/// needs to name it.
pub struct Built {
    pub index: u32,
    pub blob: Vec<u8>,
    pub tx_blobs: Vec<Vec<u8>>,
    /// The coinbase output this block created, and where it landed.
    pub coinbase_output: Spendable,
}

/// The state store a replay starts from: [`SEED_LEN`] block infos ending at
/// [`TIP`], nine spendable outputs of [`AMOUNT`], and the tip record.
///
/// Deterministic, so two calls produce byte-identical stores — which is what
/// lets a batched run and an unbatched run be compared record for record.
pub fn seed_state_store() -> (MemStore, Vec<Spendable>) {
    seed_state_store_with_outputs(9)
}

/// [`seed_state_store`] with `spendable` outputs of [`AMOUNT`] instead of nine.
///
/// A measurement that wants blocks carrying real spends needs one seeded output
/// per key input it will ever build, since a key image may only be spent once.
pub fn seed_state_store_with_outputs(spendable: u32) -> (MemStore, Vec<Spendable>) {
    seed_state_store_at(TIP, spendable)
}

/// [`seed_state_store_with_outputs`] with the seeded top block at `tip` rather
/// than [`TIP`] — for a test that needs the chain just below some height, a
/// fork boundary say. The tip's timestamp is still [`TIP_TIME`].
pub fn seed_state_store_at(tip: u32, spendable: u32) -> (MemStore, Vec<Spendable>) {
    let base = tip - SEED_LEN + 1;
    let mut store = MemStore::default();
    let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
    for i in base..=tip {
        let back = (tip - i) as u64;
        let info = BlockInfo {
            block_hash: seed_hash(i),
            timestamp: TIP_TIME - back * SPACING,
            block_size: 300,
            cumulative_difficulty: TIP_CUMULATIVE - back,
            already_generated_coins: 30_000_000_000_000,
            already_generated_transactions: i as u64 + 1,
        };
        ops.push((keys::block_info(i), Some(info.encode())));
        ops.push((keys::hash_to_index(&info.block_hash), Some(i.to_le_bytes().to_vec())));
        ops.push((keys::block_tx_hashes(i), Some(records::encode_hashes(&[seed_hash(i)]))));
    }

    let owner = Wallet::from_seed(1);
    let (tx_secret, tx_public) = tx_keys(b"seed outputs");
    let mut outputs = Vec::new();
    for i in 0..spendable as u64 {
        let (public_key, secret_key) = derive_output(&owner, &tx_secret, &tx_public, i);
        let record = OutputRecord {
            public_key,
            unlock_time: 0,
            transaction_hash: seed_hash(base),
            output_index: i as u16,
            block_index: base,
        };
        ops.push((keys::output(AMOUNT, i as u32), Some(record.encode())));
        outputs.push(Spendable { amount: AMOUNT, global_index: i as u32, public_key, secret_key });
    }
    ops.push((keys::output_count(AMOUNT), Some(spendable.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_VERSION), Some(keys::STATE_SCHEMA_VERSION.to_le_bytes().to_vec())));
    ops.push((keys::meta(keys::META_TIP), Some(tip.to_le_bytes().to_vec())));
    // The mode tag a linear replay expects to find or to write.
    ops.push((keys::meta(keys::META_TAG), Some(wrkz_chain::replay::TAG_LINEAR.as_bytes().to_vec())));
    store.write_batch(ops).unwrap();
    (store, outputs)
}

/// Open a chain state on a seeded store, with the harness clock and the real
/// mainnet checkpoints — whose last entry is at 4,188,000, so [`TIP`] is
/// **outside** the zone and every rule runs.
pub fn open_state<S: KvStore>(store: S) -> ChainState<S> {
    open_state_with(store, Checkpoints::mainnet())
}

/// [`open_state`] with a checkpoint set of the caller's choosing.
pub fn open_state_with<S: KvStore>(store: S, checkpoints: Checkpoints) -> ChainState<S> {
    open_state_with_config(store, config(), checkpoints)
}

/// [`open_state`] with the whole [`Config`] chosen by the caller — the body
/// policy tests set `store_raw_blocks`, `lite_start_height` and `prune_depth`
/// here.
pub fn open_state_with_config<S: KvStore>(store: S, cfg: Config, checkpoints: Checkpoints) -> ChainState<S> {
    open_state_at(store, cfg, checkpoints, TIP)
}

/// [`open_state_with_config`] on a store seeded at `tip`
/// ([`seed_state_store_at`]).
pub fn open_state_at<S: KvStore>(store: S, cfg: Config, checkpoints: Checkpoints, tip: u32) -> ChainState<S> {
    let mut chain = ChainState::open(store, cfg, checkpoints).expect("the seeded state opens");
    chain.set_clock(Some(NOW));
    assert_eq!(chain.tip_index(), Some(tip));
    chain
}

/// A checkpoint set that puts this harness's blocks **inside** the checkpoint
/// zone, which is the regime the operator's first 30,000 blocks are in.
///
/// `Checkpoints::isInCheckpointZone(index)` is "at or below the highest
/// checkpointed index", and `checkBlock` passes any index the set does not name.
/// So one checkpoint far above the harness reproduces exactly what the C++ does
/// below 4,188,000: no proof of work, and `validateTransactionInputsExpensive`
/// and the transaction proof of work skipped, because the block hash the
/// checkpoint commits to already covers them.
pub fn checkpoint_zone() -> Checkpoints {
    let far_above = TIP + 1_000_000;
    Checkpoints::from_csv(&format!("{far_above},{}", hex::encode([0u8; 32]))).expect("one checkpoint")
}

/// The chain the tests build on: a real [`ChainState`] that every block is
/// applied to, so that its block infos are the ones the exported C++ database
/// carries.
pub struct Chain {
    pub state: ChainState<MemStore>,
    pub built: Vec<Built>,
    pub outputs: Vec<Spendable>,
    pub miner: Wallet,
    pub payee: Wallet,
    /// The seeded top block index; the blocks built on it are timed from it.
    pub seed_tip: u32,
}

impl Default for Chain {
    fn default() -> Self {
        Self::new()
    }
}

impl Chain {
    pub fn new() -> Self {
        Self::with_seeded_outputs(9)
    }

    /// [`Chain::new`] with a larger pool of seeded spendable outputs, for a
    /// measurement that builds blocks full of key inputs.
    pub fn with_seeded_outputs(spendable: u32) -> Self {
        Self::with_config(spendable, config())
    }

    /// [`Chain::with_seeded_outputs`] over a chosen [`Config`], for the tests
    /// that need a lite or pruned body policy.
    pub fn with_config(spendable: u32, cfg: Config) -> Self {
        let (mut store, outputs) = seed_state_store_with_outputs(spendable);
        // A lite database records its height when it is *created*, and refuses
        // to be opened against a different one afterwards. The seeded store is
        // already non-empty, so the marker goes in with the seed — which is
        // exactly the state a lite node syncing from genesis would reach.
        if cfg.lite_start_height != 0 {
            store
                .write_batch(vec![(
                    keys::meta(keys::META_LITE_HEIGHT),
                    Some(cfg.lite_start_height.to_le_bytes().to_vec()),
                )])
                .expect("the marker writes");
        }
        Self {
            state: open_state_with_config(store, cfg, Checkpoints::mainnet()),
            built: Vec::new(),
            outputs,
            miner: Wallet::from_seed(2),
            payee: Wallet::from_seed(3),
            seed_tip: TIP,
        }
    }

    /// A chain seeded at `tip` instead of [`TIP`], with `spendable` outputs.
    pub fn with_tip(tip: u32, spendable: u32) -> Self {
        let (store, outputs) = seed_state_store_at(tip, spendable);
        Self {
            state: open_state_at(store, config(), Checkpoints::mainnet(), tip),
            built: Vec::new(),
            outputs,
            miner: Wallet::from_seed(2),
            payee: Wallet::from_seed(3),
            seed_tip: tip,
        }
    }

    pub fn tip(&self) -> u32 {
        self.state.tip_index().expect("seeded")
    }

    pub fn tip_hash(&self) -> Hash {
        self.state.tip_info().expect("seeded").block_hash
    }

    /// Build and apply the next block, carrying `txs`.
    ///
    /// Returns the built block. Its coinbase output is recorded with the global
    /// index the state actually gave it, so a later block can spend it.
    pub fn push(&mut self, txs: &[Transaction]) -> &Built {
        let index = self.tip() + 1;
        let n = (index - self.seed_tip) as u64;
        let fee: u64 = txs.iter().map(|t| t.fee().expect("outputs do not exceed inputs")).sum();
        let reward = AMOUNT + fee;
        let tag = index.to_le_bytes().to_vec();
        let coinbase_gi = self.state.output_count_for_amount(reward).expect("counter reads");
        let built = build_block(self.tip_hash(), index, TIP_TIME + n * SPACING, 1, txs, &self.miner, &tag, reward);
        self.state.add_block(&built.blob, &built.tx_blobs).unwrap_or_else(|e| panic!("block {index}: {e}"));
        let (public_key, secret_key) = coinbase_keys(&self.miner, &tag);
        self.built.push(Built {
            index,
            blob: built.blob,
            tx_blobs: built.tx_blobs,
            coinbase_output: Spendable { amount: reward, global_index: coinbase_gi, public_key, secret_key },
        });
        self.built.last().expect("just pushed")
    }

    /// Build and apply an empty block.
    pub fn push_empty(&mut self) -> &Built {
        self.push(&[])
    }

    /// Build and apply an empty block whose coinbase pays the reward out in
    /// `outputs` denominations instead of one.
    ///
    /// This is the shape of a real early WrkzCoin block, and the shape the
    /// operator's import is chewing through: `Currency::constructMinerTx` hands
    /// the reward to `decompose_amount_into_digits`, so a coinbase carries one
    /// output per non-zero decimal digit of the reward. Over the first 150,000
    /// blocks that is 7 or 8 — the emission curve puts the reward at 11.56
    /// million atomic units falling to 11.16 million, and the vectors confirm
    /// it: block 1 pays `[1, 300, 3000, 60000, 500000, 1000000, 10000000]`.
    ///
    /// Every one of those outputs costs a `check_key`, so a synthetic chain
    /// with a one-output coinbase understates the real per-block cost of
    /// `validateBlock` by that factor. The denominations here are distinct, so
    /// the block also bumps `outputs` per-amount counters, which is what a real
    /// block makes `push_block` read.
    pub fn push_empty_with_coinbase_outputs(&mut self, outputs: usize) -> &Built {
        assert!(outputs >= 1);
        let index = self.tip() + 1;
        let n = (index - self.seed_tip) as u64;
        let tag = index.to_le_bytes().to_vec();
        let built = build_block_with(
            self.tip_hash(),
            index,
            TIP_TIME + n * SPACING,
            1,
            &[],
            &self.miner,
            &tag,
            &denominations(AMOUNT, outputs),
        );
        self.state.add_block(&built.blob, &built.tx_blobs).unwrap_or_else(|e| panic!("block {index}: {e}"));
        let (public_key, secret_key) = coinbase_keys(&self.miner, &tag);
        self.built.push(Built {
            index,
            blob: built.blob,
            tx_blobs: built.tx_blobs,
            // Not spendable through this path: the harness only tracks output
            // zero, and a measurement never spends these.
            coinbase_output: Spendable { amount: AMOUNT, global_index: u32::MAX, public_key, secret_key },
        });
        self.built.last().expect("just pushed")
    }

    /// Build the next block on this chain **without** applying it, so that a
    /// test can hand it to some other state and ask what that state makes of it.
    pub fn build_only(&self, txs: &[Transaction]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let index = self.tip() + 1;
        let n = (index - self.seed_tip) as u64;
        let fee: u64 = txs.iter().map(|t| t.fee().expect("outputs do not exceed inputs")).sum();
        let tag = index.to_le_bytes().to_vec();
        let built =
            build_block(self.tip_hash(), index, TIP_TIME + n * SPACING, 1, txs, &self.miner, &tag, AMOUNT + fee);
        (built.blob, built.tx_blobs)
    }

    /// An empty block at `index` on top of `previous_hash`, which need not be
    /// the tip — for a test that builds a competing branch. Not applied; the
    /// blob and the block's hash come back. `tag` must differ from every other
    /// block's at that height, or the two would be the same block.
    pub fn branch_block(&self, previous_hash: Hash, index: u32, tag: &[u8]) -> (Vec<u8>, Hash) {
        let n = (index - self.seed_tip) as u64;
        let built = build_block(previous_hash, index, TIP_TIME + n * SPACING, 2, &[], &self.miner, tag, AMOUNT);
        (built.blob, built.hash)
    }

    /// The block built at `index`, if this chain built it.
    pub fn at(&self, index: u32) -> &Built {
        self.built.iter().find(|b| b.index == index).expect("built here")
    }

    /// A transaction spending `real` with `decoy` as the other ring member,
    /// signed for a block whose parent is the current tip.
    pub fn spend(&self, real: &Spendable, decoy: &Spendable, tag: &[u8]) -> Transaction {
        build_spend(self.tip(), real, decoy, &self.payee, FEE, tag)
    }

    /// The C++-layout database a replay of `TIP+1 ..= tip` reads.
    pub fn export(&self) -> MemStore {
        let mut store = MemStore::default();
        let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        ops.push((codec::DB_VERSION_KEY.to_vec(), Some(b"4".to_vec())));
        let top = self.tip();
        for index in BASE..=top {
            let ours = self.state.block_info(index).unwrap().expect("seeded or applied");
            let theirs = CachedBlockInfo {
                block_hash: ours.block_hash,
                timestamp: ours.timestamp,
                block_size: ours.block_size,
                cumulative_difficulty: ours.cumulative_difficulty,
                already_generated_coins: ours.already_generated_coins,
                already_generated_transactions: ours.already_generated_transactions,
            };
            ops.push((codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(index)), Some(theirs.encode())));
            ops.push((
                codec::key(codec::BLOCK_HASH_TO_BLOCK_INDEX, KeyPart::Hash(ours.block_hash)),
                Some(codec::value_u32("5", index)),
            ));
        }
        for b in &self.built {
            ops.push((
                codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(b.index)),
                Some(RawBlockRecord { block: b.blob.clone(), transactions: b.tx_blobs.clone() }.encode()),
            ));
        }
        ops.push((
            codec::key(codec::BLOCK_INDEX_TO_BLOCK_HASH, KeyPart::Str(codec::LAST_BLOCK_INDEX_KEY)),
            Some(codec::value_u32("8", top)),
        ));
        store.write_batch(ops).unwrap();
        store
    }
}

/// `amount` split into `parts` distinct non-zero denominations summing to it,
/// the way `decompose_amount_into_digits` splits a real reward.
fn denominations(amount: u64, parts: usize) -> Vec<u64> {
    assert!(parts >= 1);
    let mut out = Vec::with_capacity(parts);
    let mut rest = amount;
    for i in 1..parts as u64 {
        assert!(rest > i, "amount too small to split into {parts} distinct parts");
        out.push(i);
        rest -= i;
    }
    out.push(rest);
    out
}

/// The one-time key pair of a coinbase output built with `tag`.
fn coinbase_keys(miner: &Wallet, tag: &[u8]) -> (Hash, Hash) {
    let (tx_secret, tx_public) = tx_keys(&[tag, b"coinbase"].concat());
    derive_output(miner, &tx_secret, &tx_public, 0)
}

/// A key input spending `real`, with `decoy` as the other ring member.
pub fn build_spend(
    previous_index: u32,
    real: &Spendable,
    decoy: &Spendable,
    to: &Wallet,
    fee: u64,
    tag: &[u8],
) -> Transaction {
    assert_ne!(real.global_index, decoy.global_index);
    assert_eq!(real.amount, decoy.amount, "a ring is drawn from one amount");
    let (lower, higher, secret_index) =
        if real.global_index < decoy.global_index { (real, decoy, 0usize) } else { (decoy, real, 1usize) };
    let ring = [lower.public_key, higher.public_key];
    let absolute = [lower.global_index as u64, higher.global_index as u64];
    let key_offsets = absolute_to_relative_offsets(&absolute).expect("ascending");
    let key_image = curve::generate_key_image(&real.public_key, &real.secret_key);

    let (tx_secret, tx_public) = tx_keys(tag);
    let (out_key, _) = derive_output(to, &tx_secret, &tx_public, 0);
    let mut tx = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            // `UNLOCK_TIME_HEIGHT`: at least `H + MINIMUM_UNLOCK_TIME_BLOCKS`.
            unlock_time: previous_index as u64 + 20,
            inputs: vec![Input::Key { amount: real.amount, key_offsets, key_image }],
            outputs: vec![Output { amount: real.amount - fee, key: out_key }],
            extra: build_extra(&tx_public, None, None).expect("extra"),
        },
        signatures: Vec::new(),
    };
    let prefix_hash = tx.prefix.hash();
    let signatures =
        curve::generate_ring_signature(&prefix_hash, &key_image, &ring, &real.secret_key, secret_index).expect("sign");
    tx.signatures.push(signatures);
    tx
}

/// A key input spending `real` in a ring with `decoys`, all of one amount: a
/// ring of `1 + decoys.len()` members, which is a mixin of `decoys.len()`.
pub fn build_spend_ring(
    previous_index: u32,
    real: &Spendable,
    decoys: &[&Spendable],
    to: &Wallet,
    fee: u64,
    tag: &[u8],
) -> Transaction {
    let mut members: Vec<&Spendable> = decoys.to_vec();
    members.push(real);
    members.sort_by_key(|s| s.global_index);
    assert!(members.windows(2).all(|w| w[0].global_index < w[1].global_index), "distinct ring members");
    assert!(members.iter().all(|s| s.amount == real.amount), "a ring is drawn from one amount");
    let secret_index = members.iter().position(|s| s.global_index == real.global_index).expect("real is in the ring");
    let ring: Vec<Hash> = members.iter().map(|s| s.public_key).collect();
    let absolute: Vec<u64> = members.iter().map(|s| s.global_index as u64).collect();
    let key_offsets = absolute_to_relative_offsets(&absolute).expect("ascending");
    let key_image = curve::generate_key_image(&real.public_key, &real.secret_key);

    let (tx_secret, tx_public) = tx_keys(tag);
    let (out_key, _) = derive_output(to, &tx_secret, &tx_public, 0);
    let mut tx = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: previous_index as u64 + 20,
            inputs: vec![Input::Key { amount: real.amount, key_offsets, key_image }],
            outputs: vec![Output { amount: real.amount - fee, key: out_key }],
            extra: build_extra(&tx_public, None, None).expect("extra"),
        },
        signatures: Vec::new(),
    };
    let prefix_hash = tx.prefix.hash();
    let signatures =
        curve::generate_ring_signature(&prefix_hash, &key_image, &ring, &real.secret_key, secret_index).expect("sign");
    tx.signatures.push(signatures);
    tx
}

/// [`build_spend`] with a plaintext long payment id in the extra, which is the
/// only kind the payment-id index carries.
pub fn build_spend_with_payment_id(
    previous_index: u32,
    real: &Spendable,
    decoy: &Spendable,
    to: &Wallet,
    fee: u64,
    tag: &[u8],
    payment_id: Hash,
) -> Transaction {
    let mut tx = build_spend(previous_index, real, decoy, to, fee, tag);
    let (_, tx_public) = tx_keys(tag);
    let nonce = wrkz_primitives::tx::build_nonce(Some(&wrkz_primitives::tx::PaymentId::Long(payment_id)), None);
    tx.prefix.extra = build_extra(&tx_public, Some(&nonce), None).expect("extra");
    // The extra is part of the prefix, so the ring signature has to be redone
    // over the new prefix hash or the block would be rejected for a bad
    // signature rather than accepted with a payment id.
    let Input::Key { key_offsets, key_image, .. } = &tx.prefix.inputs[0] else { panic!("a key input") };
    let key_image = *key_image;
    let absolute = wrkz_primitives::tx::relative_offsets_to_absolute(key_offsets).expect("relative offsets");
    let (lower, higher) = if real.global_index < decoy.global_index { (real, decoy) } else { (decoy, real) };
    let ring = [lower.public_key, higher.public_key];
    let secret_index = if absolute[0] == real.global_index as u64 { 0usize } else { 1usize };
    let prefix_hash = tx.prefix.hash();
    tx.signatures =
        vec![curve::generate_ring_signature(&prefix_hash, &key_image, &ring, &real.secret_key, secret_index)
            .expect("sign")];
    tx
}

struct RawBuilt {
    blob: Vec<u8>,
    tx_blobs: Vec<Vec<u8>>,
    hash: Hash,
}

/// The block a miner would submit at `index`: a v7 block with the daemon
/// template's parent block and the merge-mining tag adjusted the way
/// `MinerManager::adjustMergeMiningTag` does.
fn build_block(
    previous_hash: Hash,
    index: u32,
    timestamp: u64,
    nonce: u32,
    txs: &[Transaction],
    miner: &Wallet,
    tag: &[u8],
    reward: u64,
) -> RawBuilt {
    build_block_with(previous_hash, index, timestamp, nonce, txs, miner, tag, &[reward])
}

/// [`build_block`] with the coinbase reward paid out in the given
/// denominations, which must sum to the reward the rule computes.
fn build_block_with(
    previous_hash: Hash,
    index: u32,
    timestamp: u64,
    nonce: u32,
    txs: &[Transaction],
    miner: &Wallet,
    tag: &[u8],
    amounts: &[u64],
) -> RawBuilt {
    let (tx_secret, tx_public) = tx_keys(&[tag, b"coinbase"].concat());
    let extra = build_extra(&tx_public, None, None).expect("extra");
    let outputs: Vec<Output> = amounts
        .iter()
        .enumerate()
        .map(|(i, amount)| {
            let (key, _) = derive_output(miner, &tx_secret, &tx_public, i as u64);
            Output { amount: *amount, key }
        })
        .collect();
    let coinbase = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: index as u64 + COINBASE_LOCK as u64,
            inputs: vec![Input::Base { block_index: index as u64 }],
            outputs,
            extra,
        },
        signatures: Vec::new(),
    };

    let mut block = BlockTemplate {
        major_version: 7,
        minor_version: 0,
        timestamp,
        previous_block_hash: previous_hash,
        nonce,
        parent_block: None,
        base_transaction: coinbase,
        transaction_hashes: txs.iter().map(|t| t.hash().expect("tx hash")).collect(),
    };
    let aux = block.auxiliary_header_hash().expect("aux hash");
    let mut parent_extra = Vec::new();
    append_merge_mining_tag(&mut parent_extra, &MergeMiningTag { depth: 0, merkle_root: aux });
    let parent_coinbase = BaseTransaction {
        prefix: TransactionPrefix { version: 0, unlock_time: 0, inputs: vec![], outputs: vec![], extra: parent_extra },
    };
    block.parent_block = Some(ParentBlock::new(0, 0, previous_hash, 1, Vec::new(), parent_coinbase, Vec::new()));

    RawBuilt {
        blob: block.to_bytes().expect("block serializes"),
        tx_blobs: txs.iter().map(|t| t.to_bytes().expect("tx serializes")).collect(),
        hash: block.hash().expect("block hash"),
    }
}
