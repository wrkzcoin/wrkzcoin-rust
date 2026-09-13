// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Our own key namespace, deliberately distinct from the C++ layout of
//! spec/11-storage.md.
//!
//! Every key starts with [`NS`] (`W`) and a one-byte record tag, followed by the
//! key's own bytes in **big-endian** order. Two consequences, both wanted:
//!
//! - no key can collide with a C++ record. Those are KV-binary documents and
//!   start with the KV header byte `0x01`, and the only plain-ASCII C++ key is
//!   `db_scheme_version`, which starts with `d`. A port's state may therefore
//!   share a database with a C++ one without a column family;
//! - big-endian integers sort numerically in RocksDB, so a range scan over
//!   block indexes or over the outputs of one amount is a seek plus a forward
//!   iteration. The C++ keys cannot do that (spec/11: "KV-encoded keys do not
//!   sort numerically"), which is why the C++ carries extra index records.
//!
//! Record values are fixed-layout little-endian structs ([`crate::records`]),
//! not KV documents: this state is ours, nothing else reads it, and a 68-byte
//! block info beats a 200-byte document 4.2 million times over.

use wrkz_primitives::Hash;

/// First byte of every key this crate writes.
pub const NS: u8 = b'W';

/// Meta records: `W M <name>`.
pub const TAG_META: u8 = b'M';
/// `W b <index:be32>` → [`crate::records::BlockInfo`].
pub const TAG_BLOCK_INFO: u8 = b'b';
/// `W h <hash>` → block index (be32). Main chain only, like the C++ `5`.
pub const TAG_HASH_TO_INDEX: u8 = b'h';
/// `W t <index:be32>` → the block's transaction hashes, coinbase first.
pub const TAG_BLOCK_TX_HASHES: u8 = b't';
/// `W x <transaction hash>` → the main-chain block index that holds it (le32),
/// the lookup behind `Core::isTransactionInChain` (the C++ `1` record, minus
/// everything else that record carries).
///
/// 34 bytes of key and 4 of value per transaction. Mainnet held 7,719,824
/// transactions at index 4,209,418 (PLAN 3.1), so the whole index is about
/// **293 MB** of raw key and value bytes — the smallest record that can answer
/// the question, and a single point lookup to read. It is main chain only,
/// like [`TAG_HASH_TO_INDEX`]: an unwind deletes the block's entries, so a
/// transaction on an alternative segment is *not* in the chain, which is what
/// `findSegmentContainingTransaction` would say for a transaction that only a
/// disconnected leaf holds once that leaf is pruned.
pub const TAG_TRANSACTION_INDEX: u8 = b'x';
/// `W k <key image>` → the block index that spent it (the C++ `7`).
pub const TAG_KEY_IMAGE: u8 = b'k';
/// `W K <index:be32>` → the key images spent in that block (the C++ `0`, the
/// rewind index).
pub const TAG_BLOCK_KEY_IMAGES: u8 = b'K';
/// `W o <amount:be64> <global index:be32>` → [`crate::records::OutputRecord`]
/// (the C++ `j`).
pub const TAG_OUTPUT: u8 = b'o';
/// `W c <amount:be64>` → number of outputs of that amount so far (the C++ `b`).
pub const TAG_OUTPUT_COUNT: u8 = b'c';
/// `W O <index:be32>` → the `(amount, global index)` pairs the block created,
/// so an unwind can delete them without the block body. Pruned behind
/// [`crate::Config::unwind_history`].
pub const TAG_BLOCK_OUTPUTS: u8 = b'O';
/// `W r <index:be32>` → the raw block: the block blob and its transaction
/// blobs. Only written when [`crate::Config::store_raw_blocks`] is set, and
/// then only for the heights the body policy keeps: at or above
/// [`crate::Config::lite_start_height`], and within
/// [`crate::Config::prune_depth`] of the tip.
pub const TAG_RAW_BLOCK: u8 = b'r';
/// `W p <payment id>` → the **legacy** list of the main-chain transactions
/// that carried that **plaintext long** payment id, in the order they were
/// mined — all of them in one value.
///
/// Schema 3 wrote this, rewriting the whole list each time the id was reused.
/// That is O(n) bytes per reuse and O(n²) over an id's life, which a pool or
/// exchange id reused thousands of times turns into real write amplification.
/// Schema 4 no longer writes it; it only reads it, as the frozen **prefix** of
/// the answer, followed by the [`TAG_PAYMENT_ID_ENTRY`] entries. A state
/// imported under schema 3 therefore needs no re-import: its lists stay
/// where they are, and every transaction mined after the upgrade goes into an
/// entry. An unwind that reaches below the upgrade pops the legacy list, as
/// schema 3 did.
///
/// The C++ `paymentIdIndex` is a multimap and its lookup
/// (`Core::getTransactionHashesByPaymentId`) is a range over one key; this
/// store has no ordered scan in its [`wrkz_storage::KvStore`] trait, hence a
/// counter and numbered entries instead of a range.
///
/// Encrypted short ids are **never** indexed: they are ciphertext only the
/// sender and receiver can read, so an index over them would answer a question
/// nobody can ask correctly.
pub const TAG_PAYMENT_ID: u8 = b'p';
/// `W q <payment id> <n:be32>` → the 32-byte hash of the `n`-th transaction
/// mined with that payment id **after** the ones in [`TAG_PAYMENT_ID`]
/// (schema 4). Appending one is one 70-byte write, however often the id has
/// been used.
pub const TAG_PAYMENT_ID_ENTRY: u8 = b'q';
/// `W Q <payment id>` → how many [`TAG_PAYMENT_ID_ENTRY`] entries the id has
/// (le32). Absent means none; an unwind deletes it when it reaches zero.
pub const TAG_PAYMENT_ID_COUNT: u8 = b'Q';
/// `W P <index:be32>` → the `(payment id, transaction hash)` pairs the block
/// contributed to [`TAG_PAYMENT_ID`], so an unwind can take them back out
/// without the block body — the same reason [`TAG_BLOCK_KEY_IMAGES`] and
/// [`TAG_BLOCK_OUTPUTS`] exist. Written only for blocks that had at least one.
pub const TAG_BLOCK_PAYMENT_IDS: u8 = b'P';

/// `W M version` → our state schema version (le32).
pub const META_VERSION: &str = "version";
/// `W M tip` → the applied top block index (le32). Absent means an empty state.
pub const META_TIP: &str = "tip";
/// `W M tag` → a short UTF-8 label naming what produced this state, so two
/// tools that leave incompatible states behind refuse to share a directory.
/// The windowed replay seeds history it did not validate; a linear replay must
/// never be resumed on top of that.
pub const META_TAG: &str = "tag";
/// The [`META_TAG`] of a state imported from a C++ lite node snapshot
/// (`--import-lite-snapshot`). Below its lite height such a state holds exactly
/// what the snapshot carries — block infos, spent key images, key outputs and
/// the per-amount counts derived from them — and **no** transaction records:
/// no [`TAG_BLOCK_TX_HASHES`], no [`TAG_TRANSACTION_INDEX`], no payment-id
/// entries, no per-block unwind records, and a zero `transaction_hash` in every
/// output record (the snapshot zeroes it). Consensus reads none of those, so
/// the state validates as any lite node does; what it cannot do is answer a
/// transaction question below the line, and
/// [`crate::ChainState::transactions_floor`] is how a reader finds that out.
pub const TAG_LITE_SNAPSHOT: &str = "lite-snapshot";
/// The [`META_TAG`] an import writes before its first record and replaces with
/// [`TAG_LITE_SNAPSHOT`] after its last, so a crash in between leaves a state
/// that says it is half written rather than one that looks whole.
pub const TAG_LITE_SNAPSHOT_IMPORTING: &str = "lite-snapshot-importing";
/// `W M lite_height` → the height at and above which this database stores full
/// block data (le32). Written the first time a state is opened in lite mode and
/// **never changed afterwards**: the blocks below it have no bodies and no
/// later run can conjure them, so the C++ calls the choice "Permanent for this
/// database" (`DaemonConfiguration.cpp:105`). Absent means a full database.
pub const META_LITE_HEIGHT: &str = "lite_height";
/// `W M prune_depth` → the retention depth a pruned node was last opened with
/// (le32), for reporting. Unlike [`META_LITE_HEIGHT`] this is *not* a promise:
/// raising the depth cannot bring back a body that was already deleted, so what
/// a pruned state can actually serve is discovered by probing for the lowest
/// stored body, not read from here.
pub const META_PRUNE_DEPTH: &str = "prune_depth";
/// `W M prune_floor` → the height below which a pruned node has already deleted
/// every body (le32), so a catch-up pass resumes instead of rescanning the
/// chain. The C++ keeps no such record and pays a full scan on every pass:
/// "Pruning records no height it pruned to" (`Core.cpp:2987`).
pub const META_PRUNE_FLOOR: &str = "prune_floor";

/// Bumped whenever a record layout in [`crate::records`] changes, or whenever a
/// new record makes an older state *incomplete* rather than wrong. An older or
/// newer state directory is refused rather than misread.
///
/// 2 adds [`TAG_TRANSACTION_INDEX`]. A version-1 state has every consensus
/// record but no transaction index, so
/// [`crate::ChainState::transaction_block_index`] would answer `None` for every
/// block it already holds; refusing it is the only honest reading.
///
/// 3 adds [`TAG_PAYMENT_ID`] and [`TAG_BLOCK_PAYMENT_IDS`], for the same
/// reason: a version-2 state has no payment-id index, so
/// [`crate::ChainState::transaction_hashes_by_payment_id`] would answer "no
/// transactions" for a payment id the chain does hold.
///
/// 4 moves new payment-id entries to [`TAG_PAYMENT_ID_ENTRY`] and
/// [`TAG_PAYMENT_ID_COUNT`]. A version-3 state is **complete** under version 4
/// — its [`TAG_PAYMENT_ID`] lists are read as they are — so it is opened, not
/// refused, and the first block applied to it records version 4. From then on
/// a version-3 build refuses it, as it must: it would not see the new entries.
pub const STATE_SCHEMA_VERSION: u32 = 4;

/// The oldest schema this build opens. See [`STATE_SCHEMA_VERSION`] for why 3
/// needs no migration.
pub const OLDEST_READABLE_SCHEMA_VERSION: u32 = 3;

fn head(tag: u8, extra: usize) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + extra);
    k.push(NS);
    k.push(tag);
    k
}

pub fn meta(name: &str) -> Vec<u8> {
    let mut k = head(TAG_META, name.len());
    k.extend_from_slice(name.as_bytes());
    k
}

pub fn block_info(index: u32) -> Vec<u8> {
    let mut k = head(TAG_BLOCK_INFO, 4);
    k.extend_from_slice(&index.to_be_bytes());
    k
}

pub fn hash_to_index(hash: &Hash) -> Vec<u8> {
    let mut k = head(TAG_HASH_TO_INDEX, 32);
    k.extend_from_slice(hash);
    k
}

pub fn block_tx_hashes(index: u32) -> Vec<u8> {
    let mut k = head(TAG_BLOCK_TX_HASHES, 4);
    k.extend_from_slice(&index.to_be_bytes());
    k
}

pub fn transaction_index(hash: &Hash) -> Vec<u8> {
    let mut k = head(TAG_TRANSACTION_INDEX, 32);
    k.extend_from_slice(hash);
    k
}

pub fn key_image(image: &Hash) -> Vec<u8> {
    let mut k = head(TAG_KEY_IMAGE, 32);
    k.extend_from_slice(image);
    k
}

pub fn block_key_images(index: u32) -> Vec<u8> {
    let mut k = head(TAG_BLOCK_KEY_IMAGES, 4);
    k.extend_from_slice(&index.to_be_bytes());
    k
}

pub fn output(amount: u64, global_index: u32) -> Vec<u8> {
    let mut k = head(TAG_OUTPUT, 12);
    k.extend_from_slice(&amount.to_be_bytes());
    k.extend_from_slice(&global_index.to_be_bytes());
    k
}

pub fn output_count(amount: u64) -> Vec<u8> {
    let mut k = head(TAG_OUTPUT_COUNT, 8);
    k.extend_from_slice(&amount.to_be_bytes());
    k
}

pub fn block_outputs(index: u32) -> Vec<u8> {
    let mut k = head(TAG_BLOCK_OUTPUTS, 4);
    k.extend_from_slice(&index.to_be_bytes());
    k
}

pub fn raw_block(index: u32) -> Vec<u8> {
    let mut k = head(TAG_RAW_BLOCK, 4);
    k.extend_from_slice(&index.to_be_bytes());
    k
}

pub fn payment_id(id: &Hash) -> Vec<u8> {
    let mut k = head(TAG_PAYMENT_ID, 32);
    k.extend_from_slice(id);
    k
}

pub fn payment_id_entry(id: &Hash, n: u32) -> Vec<u8> {
    let mut k = head(TAG_PAYMENT_ID_ENTRY, 36);
    k.extend_from_slice(id);
    k.extend_from_slice(&n.to_be_bytes());
    k
}

pub fn payment_id_count(id: &Hash) -> Vec<u8> {
    let mut k = head(TAG_PAYMENT_ID_COUNT, 32);
    k.extend_from_slice(id);
    k
}

pub fn block_payment_ids(index: u32) -> Vec<u8> {
    let mut k = head(TAG_BLOCK_PAYMENT_IDS, 4);
    k.extend_from_slice(&index.to_be_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_namespaced_and_sort_numerically() {
        assert_eq!(&block_info(1)[..2], b"Wb");
        assert!(block_info(9) < block_info(10), "big-endian indexes sort numerically");
        assert!(output(10, 4_000_000_000) < output(11, 0));
        assert!(output(10, 9) < output(10, 10));
        // No key can start like a C++ KV document (0x01) or `db_scheme_version`.
        for k in [
            meta(META_TIP),
            block_info(0),
            key_image(&[0; 32]),
            output(0, 0),
            raw_block(0),
            transaction_index(&[0; 32]),
            payment_id(&[0; 32]),
            block_payment_ids(0),
            payment_id_entry(&[0; 32], 0),
            payment_id_count(&[0; 32]),
        ] {
            assert_eq!(k[0], NS);
            assert_ne!(k[0], 0x01);
            assert_ne!(k[0], b'd');
        }
    }
}
