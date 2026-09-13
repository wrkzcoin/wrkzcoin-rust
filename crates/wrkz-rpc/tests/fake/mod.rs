// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! A [`NodeApi`] that answers from canned data, so every route's *shape* can be
//! tested without a chain, and so the middleware can be driven into each of its
//! failure modes.
//!
//! Not a test file of its own: `tests/endpoints.rs` includes it.

#![allow(dead_code)]

use wrkz_primitives::tx::{Input, Output, TransactionPrefix};
use wrkz_primitives::Hash;
use wrkz_rpc::api::*;

pub fn h(byte: u8) -> Hash {
    [byte; 32]
}

pub struct FakeNode {
    pub synced: bool,
    pub top: u64,
    pub info_fails: bool,
    pub pool: Vec<PoolTransactionSummary>,
    pub send_result: std::result::Result<(), String>,
    pub submit: SubmitOutcome,
    pub template_error: Option<String>,
    pub random_outs: std::result::Result<Vec<(u32, Hash)>, String>,
    pub sync_items: Vec<SyncBlock>,
    pub raw_items: Vec<RawBlockItem>,
    /// 0 on a full node; above 0 on one whose block bodies begin there, which
    /// is a C++ lite node and equally a state imported without `--store-raw`.
    pub lite_start_height: u64,
    /// What `wallet_sync_start_index` answers. `None`, the default, keeps the
    /// response cache out of every test that does not ask for it.
    pub sync_start: Option<u64>,
    /// How many times `/getwalletsyncdata` was actually built, so a test can
    /// tell a cached answer from a rebuilt one.
    pub sync_calls: std::sync::atomic::AtomicUsize,
}

impl Default for FakeNode {
    fn default() -> Self {
        Self {
            synced: true,
            top: 4_213_000,
            info_fails: false,
            pool: vec![PoolTransactionSummary { hash: h(9), fee: 1000, amount_out: 50_000, size: 250 }],
            send_result: Ok(()),
            submit: SubmitOutcome::Added { relay: true },
            template_error: None,
            random_outs: Ok(vec![(1, h(0x22)), (2, h(0x33)), (3, h(0x44))]),
            sync_items: vec![sync_block(4_213_000)],
            raw_items: vec![RawBlockItem { block: vec![7, 0, 0], transactions: vec![vec![1, 2, 3]] }],
            lite_start_height: 0,
            sync_start: None,
            sync_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

pub fn sync_block(height: u64) -> SyncBlock {
    SyncBlock {
        block_hash: h(0xc5),
        block_height: height,
        block_timestamp: 1_788_894_799,
        coinbase: Some(SyncTransaction {
            hash: h(0xaf),
            outputs: vec![SyncOutput { amount: 1_000_000, key: h(0x07), global_index: None }],
            tx_public_key: h(0xa9),
            unlock_time: height + 40,
            payment_id: String::new(),
            inputs: Vec::new(),
        }),
        transactions: vec![SyncTransaction {
            hash: h(0xbb),
            outputs: vec![SyncOutput { amount: 10_000, key: h(0x08), global_index: None }],
            tx_public_key: h(0xaa),
            unlock_time: 0,
            payment_id: "ab".repeat(32),
            inputs: vec![SyncInput { amount: 10_000, key_image: h(0xcc), key_offsets: vec![3, 1, 1] }],
        }],
    }
}

fn header(index: u64) -> BlockHeaderInfo {
    BlockHeaderInfo {
        major_version: 7,
        minor_version: 0,
        timestamp: 1_788_894_799,
        prev_hash: h(0x3d),
        nonce: 7822,
        orphan_status: false,
        height: index,
        hash: h(0xc5),
        difficulty: 24_880_685,
        reward: 1_000_000,
        num_txes: 1,
        block_size: 211,
    }
}

/// A pool transaction's prefix: key inputs only, which is what a pool ever
/// holds (a coinbase never enters it). `tests/endpoints.rs` builds a prefix
/// with a base input of its own to cover the `"type":"ff"` arm.
fn prefix() -> TransactionPrefix {
    TransactionPrefix {
        version: 1,
        unlock_time: 0,
        inputs: vec![
            Input::Key { amount: 10_000, key_offsets: vec![3, 1], key_image: h(0xcc) },
            Input::Key { amount: 50_000, key_offsets: vec![7, 2], key_image: h(0xcd) },
        ],
        outputs: vec![Output { amount: 10_000, key: h(0x08) }],
        extra: vec![1, 2, 3],
    }
}

/// The same prefix with a base input first, for the `queryblockslite` probe and
/// the offline coverage of the `"type":"ff"` arm.
pub fn prefix_with_base_input() -> TransactionPrefix {
    let mut p = prefix();
    p.inputs.insert(0, Input::Base { block_index: 7 });
    p
}

impl NodeApi for FakeNode {
    fn info(&self) -> Result<InfoSnapshot> {
        if self.info_fails {
            return Err(ApiError::Busy("reorganising".into()));
        }
        Ok(InfoSnapshot {
            height: self.top + 1,
            top_block_hash: h(0x2c),
            difficulty: 52_006_338,
            tx_count: 3_510_834,
            tx_pool_size: self.pool.len() as u64,
            alt_blocks_count: 0,
            outgoing_connections_count: 3,
            incoming_connections_count: 8,
            white_peerlist_size: 10,
            grey_peerlist_size: 112,
            seed_nodes_count: 4,
            last_seed_bootstrap: 1_788_797_165,
            last_known_block_index: self.top,
            network_height: self.top + 1,
            pruned: false,
            prune_depth: 10080,
            prune_capability_active: false,
            lite_start_height: self.lite_start_height,
            sync_active_peers: 0,
            sync_avg_batch_size: 120,
            sync_demoted_peers: 0,
            major_version: 7,
            minor_version: 0,
            version: "0.4.8".into(),
            start_time: 1_788_704_062,
        })
    }

    fn height(&self) -> HeightSnapshot {
        HeightSnapshot { height: self.top + 1, network_height: self.top + 1 }
    }

    fn peers(&self) -> PeerLists {
        PeerLists { white: vec!["1.2.3.4:17855".into()], gray: vec!["5.6.7.8:17855".into()] }
    }

    fn is_synced(&self) -> bool {
        self.synced
    }

    fn top_index(&self) -> u64 {
        self.top
    }

    fn block_hash_by_index(&self, index: u64) -> Result<Option<Hash>> {
        Ok((index <= self.top).then(|| h(0xc5)))
    }

    fn block_header_by_hash(&self, hash: &Hash) -> Result<Option<BlockHeaderInfo>> {
        Ok((*hash == h(0xc5)).then(|| header(self.top)))
    }

    fn block_header_by_index(&self, index: u64) -> Result<Option<BlockHeaderInfo>> {
        Ok((index <= self.top).then(|| header(index)))
    }

    fn block_list(&self, height: u64) -> Result<Vec<BlockListEntry>> {
        Ok((height.saturating_sub(30)..=height)
            .rev()
            .map(|i| BlockListEntry {
                cumul_size: 211,
                difficulty: 24_880_685,
                hash: h(0xc5),
                height: i,
                timestamp: 1_788_894_799,
                tx_count: 1,
            })
            .collect())
    }

    fn block_details(&self, hash: &Hash) -> Result<Option<BlockDetails>> {
        if *hash != h(0xc5) {
            return Ok(None);
        }
        Ok(Some(BlockDetails {
            header: header(self.top),
            transactions_cumulative_size: 157,
            already_generated_coins: 30_000_000_000_000,
            already_generated_transactions: 3_510_834,
            size_median: 100_000,
            base_reward: 1_000_000,
            penalty: 0.0,
            total_fee_amount: 0,
            transactions: vec![BlockTransactionSummary { hash: h(0xaf), fee: 0, amount_out: 1_000_000, size: 157 }],
        }))
    }

    fn wallet_sync_start_index(&self, _request: &SyncRequest) -> Result<Option<u64>> {
        Ok(self.sync_start)
    }

    fn wallet_sync_data(&self, _request: &SyncRequest) -> Result<WalletSyncData> {
        self.sync_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(WalletSyncData {
            items: self.sync_items.clone(),
            top_block: self.sync_items.is_empty().then_some(TopBlock { hash: h(0x2c), height: self.top }),
            scanned_to_height: self.sync_items.last().map(|b| b.block_height).unwrap_or(0),
        })
    }

    fn raw_blocks(&self, _request: &SyncRequest) -> Result<RawBlocks> {
        Ok(RawBlocks {
            items: self.raw_items.clone(),
            top_block: self.raw_items.is_empty().then_some(TopBlock { hash: h(0x2c), height: self.top }),
        })
    }

    fn global_indexes_for_range(&self, _start: u64, _end: u64) -> Result<Vec<(Hash, Vec<u64>)>> {
        Ok(vec![(h(0xaf), vec![3_808_773])])
    }

    fn transaction_global_indexes(&self, hash: &Hash) -> Result<Option<Vec<u32>>> {
        Ok((*hash == h(0xaf)).then(|| vec![3_808_773]))
    }

    fn random_outputs(&self, _amount: u64, _count: u16) -> Result<std::result::Result<Vec<(u32, Hash)>, String>> {
        Ok(self.random_outs.clone())
    }

    fn add_transaction_to_pool(&self, _blob: &[u8]) -> std::result::Result<(), String> {
        self.send_result.clone()
    }

    fn transactions_status(&self, hashes: &[Hash]) -> Result<TransactionsStatus> {
        let mut s = TransactionsStatus::default();
        for hash in hashes {
            if *hash == h(0xaf) {
                s.in_block.push(*hash);
            } else if *hash == h(9) {
                s.in_pool.push(*hash);
            } else {
                s.unknown.push(*hash);
            }
        }
        Ok(s)
    }

    fn pool_transactions(&self) -> Result<Vec<PoolTransactionSummary>> {
        Ok(self.pool.clone())
    }

    fn pool_changes_lite(&self, tail: &Hash, _known: &[Hash]) -> Result<PoolChanges> {
        Ok(PoolChanges {
            added: vec![TxPrefixInfo { hash: h(9), prefix: prefix() }],
            deleted: vec![h(0x11)],
            is_tail_block_actual: *tail == h(0x2c),
        })
    }

    fn query_blocks_lite(&self, _known: &[Hash], _timestamp: u64) -> Result<QueryBlocksLite> {
        Ok(QueryBlocksLite {
            start_index: 0,
            current_index: self.top,
            full_offset: 0,
            items: vec![BlockShortInfo {
                block_id: h(0xc5),
                block: vec![7, 0, 1],
                tx_prefixes: vec![TxPrefixInfo { hash: h(9), prefix: prefix_with_base_input() }],
            }],
        })
    }

    fn transaction_blob(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        Ok((*hash == h(0xaf)).then(|| vec![1, 2, 3]))
    }

    fn block_template(
        &self,
        _wallet_address: &str,
        reserve: &[u8],
    ) -> Result<std::result::Result<BlockTemplateAnswer, String>> {
        if let Some(e) = &self.template_error {
            return Ok(Err(e.clone()));
        }
        // A blob that carries the public key followed by two tag bytes and the
        // reserve, so the offset search finds it.
        let key = h(0x5a);
        let mut blob = vec![0xaa; 8];
        blob.extend_from_slice(&key);
        blob.extend_from_slice(&[0x02, reserve.len() as u8]);
        blob.extend_from_slice(reserve);
        Ok(Ok(BlockTemplateAnswer { blob, difficulty: 52_006_338, height: self.top + 1, tx_public_key: key }))
    }

    fn submit_block(&self, _blob: &[u8]) -> Result<SubmitOutcome> {
        Ok(self.submit)
    }
}
