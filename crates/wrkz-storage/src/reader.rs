// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Typed read access over any [`KvStore`], enough for `getblockheaderbyheight`,
//! ring-member resolution and the replay test (spec/11 acceptance 1–3).

use crate::codec::{self, KeyPart};
use crate::records::{CachedBlockInfo, ExtendedTransactionInfo, KeyOutputInfo, RawBlockRecord};
use crate::{KvStore, Result, StorageError};
use wrkz_primitives::block::BlockTemplate;
use wrkz_primitives::tx::{relative_offsets_to_absolute, Input, Transaction};

pub struct ChainReader<S: KvStore> {
    pub store: S,
}

/// What `getblockheaderbyheight` reports, computed from the stored records.
///
/// The fields that only the raw block (`4`) carries live in [`HeaderBody`] and
/// are absent in a pruned or lite region: pruned nodes delete `4` below a depth
/// and lite nodes never write it (spec/11, "What else lives in the database"),
/// where the `6` record alone still answers hash, timestamp, difficulty, size
/// and emission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub index: u32,
    pub hash: [u8; 32],
    pub timestamp: u64,
    /// This block's own difficulty (cumulative delta).
    pub difficulty: u64,
    pub cumulative_difficulty: u64,
    pub block_size: u32,
    pub already_generated_coins: u64,
    /// `None` when the raw block record is not stored for this height.
    pub body: Option<HeaderBody>,
}

/// The half of a header that only the raw block record can answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderBody {
    pub prev_hash: [u8; 32],
    pub major_version: u8,
    pub minor_version: u8,
    pub nonce: u32,
    /// Coinbase output total.
    pub reward: u64,
    /// Transactions including the coinbase.
    pub num_txes: u64,
}

/// One key input resolved against the `j` table: its key image and the ring of
/// one-time public keys the offsets name.
pub type ResolvedRing = ([u8; 32], Vec<[u8; 32]>);

impl Header {
    /// The raw-block-derived half, or a `Missing` error naming why it is absent.
    pub fn body(&self) -> Result<&HeaderBody> {
        self.body.as_ref().ok_or(StorageError::Missing("raw block (pruned or lite region)"))
    }
}

/// Narrow a stored 64-bit integer to the uint32 the C++ field actually is.
fn u32_field(v: u64, what: &'static str) -> Result<u32> {
    u32::try_from(v).map_err(|_| StorageError::Decode(format!("{what}: {v} does not fit uint32")))
}

impl<S: KvStore> ChainReader<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    fn get_required(&self, key: &[u8], what: &'static str) -> Result<Vec<u8>> {
        self.store.get(key)?.ok_or(StorageError::Missing(what))
    }

    /// `db_scheme_version` as a decimal string; `None` on an empty database.
    ///
    /// A key that is present but unreadable is an error rather than a `None`:
    /// spec/11 makes a version mismatch fatal, so "no database here" must not be
    /// confusable with "a version I could not parse".
    pub fn schema_version(&self) -> Result<Option<u32>> {
        let Some(raw) = self.store.get(codec::DB_VERSION_KEY)? else { return Ok(None) };
        let text = String::from_utf8(raw).map_err(|_| StorageError::Decode("db_scheme_version: not text".into()))?;
        let text = text.trim();
        let version =
            text.parse().map_err(|_| StorageError::Decode(format!("db_scheme_version: {text:?} is not a number")))?;
        Ok(Some(version))
    }

    pub fn last_block_index(&self) -> Result<u32> {
        let doc = self.get_required(
            &codec::key(codec::BLOCK_INDEX_TO_BLOCK_HASH, KeyPart::Str(codec::LAST_BLOCK_INDEX_KEY)),
            "last_block_index",
        )?;
        u32_field(codec::decode_u64(codec::BLOCK_INDEX_TO_BLOCK_HASH, &doc)?, "last_block_index")
    }

    pub fn transactions_count(&self) -> Result<u64> {
        let doc = self.get_required(
            &codec::key(codec::TRANSACTION_HASH_TO_TRANSACTION_INFO, KeyPart::Str(codec::TRANSACTIONS_COUNT_KEY)),
            "txs_count",
        )?;
        codec::decode_u64(codec::TRANSACTION_HASH_TO_TRANSACTION_INFO, &doc)
    }

    pub fn block_info(&self, index: u32) -> Result<Option<CachedBlockInfo>> {
        match self.store.get(&codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(index)))? {
            Some(doc) => Ok(Some(CachedBlockInfo::decode(&doc)?)),
            None => Ok(None),
        }
    }

    pub fn block_index_by_hash(&self, hash: &[u8; 32]) -> Result<Option<u32>> {
        match self.store.get(&codec::key(codec::BLOCK_HASH_TO_BLOCK_INDEX, KeyPart::Hash(*hash)))? {
            Some(doc) => {
                Ok(Some(u32_field(codec::decode_u64(codec::BLOCK_HASH_TO_BLOCK_INDEX, &doc)?, "block index")?))
            }
            None => Ok(None),
        }
    }

    /// Transaction hashes of a block, coinbase first.
    pub fn block_tx_hashes(&self, index: u32) -> Result<Vec<[u8; 32]>> {
        match self.store.get(&codec::key(codec::BLOCK_INDEX_TO_TX_HASHES, KeyPart::U32(index)))? {
            Some(doc) => codec::decode_hashes(codec::BLOCK_INDEX_TO_TX_HASHES, &doc),
            None => Ok(Vec::new()),
        }
    }

    pub fn raw_block(&self, index: u32) -> Result<Option<RawBlockRecord>> {
        match self.store.get(&codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(index)))? {
            Some(doc) => Ok(Some(RawBlockRecord::decode(&doc)?)),
            None => Ok(None),
        }
    }

    pub fn transaction(&self, hash: &[u8; 32]) -> Result<Option<ExtendedTransactionInfo>> {
        match self.store.get(&codec::key(codec::TRANSACTION_HASH_TO_TRANSACTION_INFO, KeyPart::Hash(*hash)))? {
            Some(doc) => Ok(Some(ExtendedTransactionInfo::decode(&doc)?)),
            None => Ok(None),
        }
    }

    pub fn key_output(&self, amount: u64, global_index: u32) -> Result<Option<KeyOutputInfo>> {
        match self.store.get(&codec::key(codec::KEY_OUTPUT_KEY, KeyPart::AmountIndex(amount, global_index)))? {
            Some(doc) => Ok(Some(KeyOutputInfo::decode(&doc)?)),
            None => Ok(None),
        }
    }

    /// Number of outputs of `amount` so far (the next global index).
    pub fn outputs_count_for_amount(&self, amount: u64) -> Result<u32> {
        match self.store.get(&codec::key(codec::KEY_OUTPUT_AMOUNT, KeyPart::U64(amount)))? {
            Some(doc) => u32_field(codec::decode_u64(codec::KEY_OUTPUT_AMOUNT, &doc)?, "output count for amount"),
            None => Ok(0),
        }
    }

    /// The block index a key image was spent in, if any.
    pub fn spent_key_image_block(&self, key_image: &[u8; 32]) -> Result<Option<u32>> {
        match self.store.get(&codec::key(codec::KEY_IMAGE_TO_BLOCK_INDEX, KeyPart::Hash(*key_image)))? {
            Some(doc) => {
                Ok(Some(u32_field(codec::decode_u64(codec::KEY_IMAGE_TO_BLOCK_INDEX, &doc)?, "spending block index")?))
            }
            None => Ok(None),
        }
    }

    /// Key images spent in a block (the rewind index, prefix `0`).
    pub fn key_images_of_block(&self, index: u32) -> Result<Vec<[u8; 32]>> {
        match self.store.get(&codec::key(codec::BLOCK_INDEX_TO_KEY_IMAGE, KeyPart::U32(index)))? {
            Some(doc) => codec::decode_hashes(codec::BLOCK_INDEX_TO_KEY_IMAGE, &doc),
            None => Ok(Vec::new()),
        }
    }

    /// Assemble the `getblockheaderbyheight` view for `index` from `6`, `4` and
    /// the parent's `6`. [`Header::body`] is `None` where `4` is not stored.
    pub fn header(&self, index: u32) -> Result<Option<Header>> {
        let block = match self.raw_block(index)? {
            Some(raw) => Some(BlockTemplate::from_bytes(&raw.block)?),
            None => None,
        };
        self.header_with_block(index, block.as_ref())
    }

    /// [`ChainReader::header`] for a caller that has already read and parsed the
    /// raw block, so the `4` record is not fetched and decoded twice (the record
    /// is a varint per byte, so decoding it is not free). Pass `None` for a
    /// height whose raw block is not stored.
    pub fn header_with_block(&self, index: u32, block: Option<&BlockTemplate>) -> Result<Option<Header>> {
        let Some(info) = self.block_info(index)? else { return Ok(None) };
        let parent_cum = if index == 0 {
            0
        } else {
            self.block_info(index - 1)?
                .map(|p| p.cumulative_difficulty)
                .ok_or(StorageError::Missing("parent block info"))?
        };
        // Cumulative difficulty only ever grows, so a value below the parent's
        // means the two records disagree; saying so beats wrapping into a
        // nonsense difficulty that then fails a proof-of-work check somewhere
        // far away from the actual corruption.
        let difficulty = info.cumulative_difficulty.checked_sub(parent_cum).ok_or_else(|| {
            let cum = info.cumulative_difficulty;
            StorageError::Decode(format!(
                "block {index}: cumulative difficulty {cum} is below its parent's {parent_cum}"
            ))
        })?;
        let body = block.map(|block| HeaderBody {
            prev_hash: block.previous_block_hash,
            major_version: block.major_version,
            minor_version: block.minor_version,
            nonce: block.nonce,
            reward: block.coinbase_output_total().unwrap_or(0),
            num_txes: block.transaction_hashes.len() as u64 + 1,
        });
        Ok(Some(Header {
            index,
            hash: info.block_hash,
            timestamp: info.timestamp,
            difficulty,
            cumulative_difficulty: info.cumulative_difficulty,
            block_size: info.block_size,
            already_generated_coins: info.already_generated_coins,
            body,
        }))
    }

    // -- batched reads -------------------------------------------------------
    //
    // A replay walks the chain one index at a time and asks for the same three
    // records at every one of them. Each of those is a point lookup into a
    // 40 GB database whose keys are KV documents holding *little-endian*
    // integers, so consecutive block indexes are not consecutive keys and every
    // one of them lands somewhere else. `MultiGet` shares one snapshot, one
    // pass over the block cache and one set of filter-block lookups across the
    // whole group, and lets the engine coalesce the reads that do fall in the
    // same SST file. These are the shapes the replay's read-ahead uses.
    //
    // They return the **undecoded** documents on purpose: decoding a record for
    // an index the run has not reached yet would report a corrupt record at the
    // wrong block. The caller decodes each one when it gets there.

    /// The raw `4` documents for a run of indexes, in the order asked.
    pub fn raw_block_docs(&self, indexes: &[u32]) -> Result<Vec<Option<Vec<u8>>>> {
        let keys: Vec<Vec<u8>> =
            indexes.iter().map(|i| codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(*i))).collect();
        self.store.multi_get(&keys)
    }

    /// The raw `6` documents for a run of indexes, in the order asked.
    pub fn block_info_docs(&self, indexes: &[u32]) -> Result<Vec<Option<Vec<u8>>>> {
        let keys: Vec<Vec<u8>> =
            indexes.iter().map(|i| codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(*i))).collect();
        self.store.multi_get(&keys)
    }

    /// The `5` records (block hash to index) for a set of hashes, in the order
    /// asked.
    pub fn block_indexes_by_hash(&self, hashes: &[[u8; 32]]) -> Result<Vec<Option<u32>>> {
        let keys: Vec<Vec<u8>> =
            hashes.iter().map(|h| codec::key(codec::BLOCK_HASH_TO_BLOCK_INDEX, KeyPart::Hash(*h))).collect();
        self.store
            .multi_get(&keys)?
            .into_iter()
            .map(|doc| match doc {
                Some(doc) => {
                    Ok(Some(u32_field(codec::decode_u64(codec::BLOCK_HASH_TO_BLOCK_INDEX, &doc)?, "block index")?))
                }
                None => Ok(None),
            })
            .collect()
    }

    /// [`ChainReader::key_output`] for a whole list of `(amount, global index)`
    /// pairs, in one batched read, answering in the order asked.
    ///
    /// This is a genuine batch rather than a read-ahead: seeding one block of a
    /// windowed replay resolves every ring member of every key input of every
    /// transaction in it, which is two to eight outputs per input and often
    /// hundreds per block.
    pub fn key_outputs(&self, pairs: &[(u64, u32)]) -> Result<Vec<Option<KeyOutputInfo>>> {
        let keys: Vec<Vec<u8>> = pairs
            .iter()
            .map(|(amount, gi)| codec::key(codec::KEY_OUTPUT_KEY, KeyPart::AmountIndex(*amount, *gi)))
            .collect();
        self.store
            .multi_get(&keys)?
            .into_iter()
            .map(|doc| match doc {
                Some(doc) => Ok(Some(KeyOutputInfo::decode(&doc)?)),
                None => Ok(None),
            })
            .collect()
    }

    /// [`ChainReader::spent_key_image_block`] for a whole list, in one batched
    /// read, answering in the order asked.
    pub fn spent_key_image_blocks(&self, images: &[[u8; 32]]) -> Result<Vec<Option<u32>>> {
        let keys: Vec<Vec<u8>> =
            images.iter().map(|i| codec::key(codec::KEY_IMAGE_TO_BLOCK_INDEX, KeyPart::Hash(*i))).collect();
        self.store
            .multi_get(&keys)?
            .into_iter()
            .map(|doc| match doc {
                Some(doc) => Ok(Some(u32_field(
                    codec::decode_u64(codec::KEY_IMAGE_TO_BLOCK_INDEX, &doc)?,
                    "spending block index",
                )?)),
                None => Ok(None),
            })
            .collect()
    }

    /// Resolve the ring members of every key input of a transaction through the
    /// `j` table: one [`ResolvedRing`] per key input, in input order (inputs
    /// that are not key inputs are skipped, as a coinbase has none).
    pub fn resolve_rings(&self, tx: &Transaction) -> Result<Vec<ResolvedRing>> {
        let mut out = Vec::new();
        for input in &tx.prefix.inputs {
            let Input::Key { amount, key_offsets, key_image } = input else { continue };
            let abs =
                relative_offsets_to_absolute(key_offsets).ok_or(StorageError::Decode("offset overflow".into()))?;
            let mut ring = Vec::with_capacity(abs.len());
            for gi in abs {
                // Global indexes are uint32 in the `j` key. Truncating a larger
                // offset would resolve a different output that does exist.
                let gi = u32_field(gi, "ring member global index")?;
                let ko = self.key_output(*amount, gi)?.ok_or(StorageError::Missing("ring member output"))?;
                ring.push(ko.public_key);
            }
            out.push((*key_image, ring));
        }
        Ok(out)
    }

    /// Verify every ring signature of a transaction against the stored outputs.
    pub fn verify_ring_signatures(&self, tx: &Transaction) -> Result<bool> {
        let prefix_hash = tx.prefix.hash();
        let rings = self.resolve_rings(tx)?;
        if rings.len() != tx.signatures.len() {
            return Ok(false);
        }
        for ((image, ring), sigs) in rings.iter().zip(&tx.signatures) {
            if !wrkz_pow::curve::check_ring_signature(&prefix_hash, image, ring, sigs) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::KeyInputRecord;
    use crate::MemStore;

    fn put(m: &mut MemStore, key: Vec<u8>, value: Vec<u8>) {
        m.put(key, value).unwrap();
    }

    /// Build a tiny database with the genesis block the way the C++ writer would.
    fn genesis_store() -> MemStore {
        let g = wrkz_primitives::block::genesis_block();
        let blob = g.to_bytes().unwrap();
        let hash = g.hash().unwrap();
        let mut m = MemStore::default();
        put(&mut m, codec::DB_VERSION_KEY.to_vec(), b"4".to_vec());
        put(
            &mut m,
            codec::key(codec::BLOCK_INDEX_TO_BLOCK_HASH, KeyPart::Str(codec::LAST_BLOCK_INDEX_KEY)),
            codec::value_u32("8", 0),
        );
        let info = CachedBlockInfo {
            block_hash: hash,
            timestamp: 0,
            block_size: 197,
            cumulative_difficulty: 1,
            already_generated_coins: 1_500_000_000_000,
            already_generated_transactions: 1,
        };
        put(&mut m, codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(0)), info.encode());
        put(&mut m, codec::key(codec::BLOCK_HASH_TO_BLOCK_INDEX, KeyPart::Hash(hash)), codec::value_u32("5", 0));
        put(
            &mut m,
            codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(0)),
            RawBlockRecord { block: blob, transactions: vec![] }.encode(),
        );
        let cb_hash = g.base_transaction.hash().unwrap();
        put(&mut m, codec::key(codec::BLOCK_INDEX_TO_TX_HASHES, KeyPart::U32(0)), codec::value_hashes("1", &[cb_hash]));
        let tx = ExtendedTransactionInfo {
            info: crate::records::CachedTransactionInfo {
                block_index: 0,
                transaction_index: 0,
                transaction_hash: cb_hash,
                unlock_time: 40,
                output_keys: g.base_transaction.prefix.outputs.iter().map(|o| o.key).collect(),
                output_amounts: g.base_transaction.prefix.outputs.iter().map(|o| o.amount).collect(),
                global_indexes: vec![0, 1, 2],
                key_inputs: vec![],
                transaction_public_key: [0; 32],
                payment_id: vec![],
            },
            amount_to_key_indexes: vec![(500_000_000_000, vec![0, 1, 2])],
        };
        put(&mut m, codec::key(codec::TRANSACTION_HASH_TO_TRANSACTION_INFO, KeyPart::Hash(cb_hash)), tx.encode());
        put(
            &mut m,
            codec::key(codec::TRANSACTION_HASH_TO_TRANSACTION_INFO, KeyPart::Str(codec::TRANSACTIONS_COUNT_KEY)),
            codec::value_u64("a", 1),
        );
        for (i, o) in g.base_transaction.prefix.outputs.iter().enumerate() {
            let ko = KeyOutputInfo {
                public_key: o.key,
                transaction_hash: cb_hash,
                unlock_time: 40,
                output_index: i as u16,
                block_index: 0,
            };
            put(&mut m, codec::key(codec::KEY_OUTPUT_KEY, KeyPart::AmountIndex(o.amount, i as u32)), ko.encode());
        }
        put(&mut m, codec::key(codec::KEY_OUTPUT_AMOUNT, KeyPart::U64(500_000_000_000)), codec::value_u32("b", 3));
        let _ = KeyInputRecord { amount: 0, key_offsets: vec![], key_image: [0; 32] };
        m
    }

    #[test]
    fn reads_genesis_like_the_rpc() {
        let r = ChainReader::new(genesis_store());
        assert_eq!(r.schema_version().unwrap(), Some(4));
        assert_eq!(r.last_block_index().unwrap(), 0);
        assert_eq!(r.transactions_count().unwrap(), 1);
        let h = r.header(0).unwrap().unwrap();
        assert_eq!(hex::encode(h.hash), "877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce");
        assert_eq!(h.difficulty, 1);
        assert_eq!(h.body().unwrap().reward, 1_500_000_000_000);
        assert_eq!(h.body().unwrap().nonce, 70);
        assert_eq!(h.body().unwrap().num_txes, 1);
        assert_eq!(r.block_index_by_hash(&h.hash).unwrap(), Some(0));
        assert_eq!(r.block_tx_hashes(0).unwrap().len(), 1);
        assert_eq!(r.outputs_count_for_amount(500_000_000_000).unwrap(), 3);
        assert_eq!(r.key_output(500_000_000_000, 2).unwrap().unwrap().output_index, 2);
        assert_eq!(r.key_output(500_000_000_000, 3).unwrap(), None);
        assert_eq!(r.spent_key_image_block(&[1; 32]).unwrap(), None);
        assert_eq!(r.header(1).unwrap(), None);
    }

    /// A pruned or lite region keeps `6` without `4`; the header must still
    /// answer everything `6` knows instead of failing outright.
    #[test]
    fn reads_a_header_without_its_raw_block() {
        let mut m = genesis_store();
        m.delete(codec::key(codec::BLOCK_INDEX_TO_RAW_BLOCK, KeyPart::U32(0))).unwrap();
        let r = ChainReader::new(m);
        let h = r.header(0).unwrap().unwrap();
        assert_eq!(h.difficulty, 1);
        assert_eq!(h.block_size, 197);
        assert_eq!(h.already_generated_coins, 1_500_000_000_000);
        assert!(h.body.is_none());
        assert!(matches!(h.body(), Err(StorageError::Missing(_))));
    }

    #[test]
    fn rejects_a_database_whose_difficulty_goes_backwards() {
        let mut m = genesis_store();
        // The parent (genesis) is at cumulative difficulty 1.
        let child = CachedBlockInfo {
            block_hash: [7; 32],
            timestamp: 1,
            block_size: 200,
            cumulative_difficulty: 0,
            already_generated_coins: 2,
            already_generated_transactions: 2,
        };
        m.put(codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(1)), child.encode()).unwrap();
        let r = ChainReader::new(m);
        assert!(matches!(r.header(1), Err(StorageError::Decode(_))));
    }

    #[test]
    fn schema_version_tells_absent_from_unreadable() {
        let mut m = genesis_store();
        m.delete(codec::DB_VERSION_KEY.to_vec()).unwrap();
        assert_eq!(ChainReader::new(m).schema_version().unwrap(), None);
        let mut m = genesis_store();
        m.put(codec::DB_VERSION_KEY.to_vec(), b"four".to_vec()).unwrap();
        assert!(ChainReader::new(m).schema_version().is_err());
    }

    #[test]
    fn write_batch_is_all_or_nothing_per_call() {
        let mut m = MemStore::default();
        m.write_batch(vec![(b"a".to_vec(), Some(b"1".to_vec())), (b"b".to_vec(), Some(b"2".to_vec()))]).unwrap();
        assert_eq!(m.get(b"a").unwrap(), Some(b"1".to_vec()));
        m.write_batch(vec![(b"a".to_vec(), None), (b"b".to_vec(), Some(b"3".to_vec()))]).unwrap();
        assert_eq!(m.get(b"a").unwrap(), None);
        assert_eq!(m.get(b"b").unwrap(), Some(b"3".to_vec()));
    }

    #[test]
    fn ring_verification_round_trip() {
        // Store a fake output, sign a transaction that spends it, verify through the reader.
        let mut m = genesis_store();
        let (sec, pk) = wrkz_pow::curve::generate_keys();
        let (dsec, dpk) = wrkz_pow::curve::generate_keys();
        let _ = dsec;
        for (i, key) in [pk, dpk].iter().enumerate() {
            let ko = KeyOutputInfo {
                public_key: *key,
                transaction_hash: [0; 32],
                unlock_time: 0,
                output_index: 0,
                block_index: 1,
            };
            put(&mut m, codec::key(codec::KEY_OUTPUT_KEY, KeyPart::AmountIndex(10000, i as u32)), ko.encode());
        }
        let image = wrkz_pow::curve::generate_key_image(&pk, &sec);
        let mut tx = Transaction::default();
        tx.prefix.version = 1;
        tx.prefix.inputs.push(Input::Key { amount: 10000, key_offsets: vec![0, 1], key_image: image });
        tx.prefix.outputs.push(wrkz_primitives::tx::Output { amount: 9990, key: [3; 32] });
        let prefix_hash = tx.prefix.hash();
        let sigs = wrkz_pow::curve::generate_ring_signature(&prefix_hash, &image, &[pk, dpk], &sec, 0).unwrap();
        tx.signatures.push(sigs);
        let r = ChainReader::new(m);
        assert!(r.verify_ring_signatures(&tx).unwrap());
        tx.signatures[0][1][0] ^= 1;
        assert!(!r.verify_ring_signatures(&tx).unwrap());
    }
}
