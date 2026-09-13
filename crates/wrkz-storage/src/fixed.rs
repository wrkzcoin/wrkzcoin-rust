// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Fixed-layout fast paths for the three C++ records a lite node snapshot
//! carries: block info (`6`), spent key image (`7`) and key output (`j`).
//!
//! [`crate::codec`] builds and parses any KV document, which is what an
//! arbitrary record needs. These three are different: every field of them is a
//! fixed-width integer or a 32-byte POD, so the serializer emits the same
//! bytes in the same places for every record, and only the field values move.
//! A lite snapshot holds about 149 million of them (`LITESNAPSHOT.md`), and
//! decoding each through a generic [`wrkz_primitives::kv::Section`] — a
//! `String` allocation per field name — would cost more than the decompression
//! does.
//!
//! So each record here is a **template**: the document the generic encoder
//! produces for zero-valued fields, with the byte ranges ("slots") the values
//! occupy. An encoder copies the template and fills the slots; a decoder
//! checks every byte outside the slots against the template and reads the
//! slots. That makes the decoders stricter than [`crate::codec`], not looser:
//! a document with a field renamed, reordered, retyped or widened is refused
//! rather than read, which is what a reader of a stranger's file wants.
//!
//! The layouts, with `hdr` the 9-byte KV header `01 11 01 01 01 01 02 01 01`
//! and `P` the table letter (`DBUtils.h:62-85`, `BlockchainCache.cpp:108`,
//! `DatabaseCacheData.cpp:20`):
//!
//! | record | bytes | layout |
//! | --- | --- | --- |
//! | key, every table | 0..30 | `hdr 04 01 P 0C 08 05 "first" 0A 04 P 06 "second"` |
//! | `6` key | 35 | `… 06` + index (u32 LE) |
//! | `7` key | 64 | `… 0A 80` + 32-byte key image |
//! | `j` key | 59 | `… 0C 08 05 "first" 05` amount (u64 LE) `06 "second" 06` index (u32 LE) |
//! | `6` value | 203 | `hdr 04 01 '6' 0C 18`, then six fields (below) |
//! | `7` value | 17 | `hdr 04 01 '7' 06` + block index (u32 LE) |
//! | `j` value | 164 | `hdr 04 01 'j' 0C 14`, then five fields (below) |
//!
//! The `6` fields are `block_hash`, `timestamp`, `block_size`,
//! `cumulative_difficulty`, `already_generated_coins` and
//! `already_generated_transaction_count`; the `j` fields are `public_key`,
//! `transaction_hash`, `unlock_time`, `output_index` and `block_index`. Each is
//! `u8 name length, name, u8 type, value`, with a 32-byte value framed as
//! `0A 80`.
//!
//! Integers are little-endian inside the keys, so the engine's byte order of
//! these keys is **not** numeric order.

use crate::codec::{self, KeyPart};
use crate::records::{CachedBlockInfo, KeyOutputInfo};
use std::sync::OnceLock;

/// How many leading key bytes every record of one table shares: the KV header,
/// the root entry count, the root name's length and the name itself, which is
/// the table letter. What the C++ `tableKeyPrefix` returns
/// (`LiteSnapshotImporter.cpp:108`).
pub const TABLE_PREFIX_LEN: usize = 12;

pub const BLOCK_INFO_KEY_LEN: usize = 35;
pub const KEY_IMAGE_KEY_LEN: usize = 64;
pub const KEY_OUTPUT_KEY_LEN: usize = 59;
pub const BLOCK_INFO_VALUE_LEN: usize = 203;
pub const KEY_IMAGE_VALUE_LEN: usize = 17;
pub const KEY_OUTPUT_VALUE_LEN: usize = 164;

type Slot = (usize, usize);

// Where the values sit. `tests::the_slots_are_where_the_generic_encoder_puts_the_values`
// derives each of these from the generic encoder rather than trusting them.
const BLOCK_INFO_KEY_INDEX: Slot = (31, 35);
const KEY_IMAGE_KEY_IMAGE: Slot = (32, 64);
const KEY_OUTPUT_KEY_AMOUNT: Slot = (39, 47);
const KEY_OUTPUT_KEY_INDEX: Slot = (55, 59);
const BLOCK_INFO_HASH: Slot = (27, 59);
const BLOCK_INFO_TIMESTAMP: Slot = (70, 78);
const BLOCK_INFO_SIZE: Slot = (90, 94);
const BLOCK_INFO_CUMULATIVE_DIFFICULTY: Slot = (117, 125);
const BLOCK_INFO_COINS: Slot = (150, 158);
const BLOCK_INFO_TRANSACTIONS: Slot = (195, 203);
const KEY_IMAGE_VALUE_INDEX: Slot = (13, 17);
const KEY_OUTPUT_PUBLIC_KEY: Slot = (27, 59);
const KEY_OUTPUT_TRANSACTION_HASH: Slot = (78, 110);
const KEY_OUTPUT_UNLOCK_TIME: Slot = (123, 131);
const KEY_OUTPUT_OUTPUT_INDEX: Slot = (145, 147);
const KEY_OUTPUT_BLOCK_INDEX: Slot = (160, 164);

/// The templates, built once from the generic encoder with every value zero.
struct Templates {
    block_info_key: Vec<u8>,
    key_image_key: Vec<u8>,
    key_output_key: Vec<u8>,
    block_info_value: Vec<u8>,
    key_image_value: Vec<u8>,
    key_output_value: Vec<u8>,
}

fn templates() -> &'static Templates {
    static T: OnceLock<Templates> = OnceLock::new();
    T.get_or_init(|| {
        let t = Templates {
            block_info_key: codec::key(codec::BLOCK_INDEX_TO_BLOCK_INFO, KeyPart::U32(0)),
            key_image_key: codec::key(codec::KEY_IMAGE_TO_BLOCK_INDEX, KeyPart::Hash([0; 32])),
            key_output_key: codec::key(codec::KEY_OUTPUT_KEY, KeyPart::AmountIndex(0, 0)),
            block_info_value: CachedBlockInfo::default().encode(),
            key_image_value: codec::value_u32(codec::KEY_IMAGE_TO_BLOCK_INDEX, 0),
            key_output_value: KeyOutputInfo::default().encode(),
        };
        assert_eq!(t.block_info_key.len(), BLOCK_INFO_KEY_LEN);
        assert_eq!(t.key_image_key.len(), KEY_IMAGE_KEY_LEN);
        assert_eq!(t.key_output_key.len(), KEY_OUTPUT_KEY_LEN);
        assert_eq!(t.block_info_value.len(), BLOCK_INFO_VALUE_LEN);
        assert_eq!(t.key_image_value.len(), KEY_IMAGE_VALUE_LEN);
        assert_eq!(t.key_output_value.len(), KEY_OUTPUT_VALUE_LEN);
        t
    })
}

/// The [`TABLE_PREFIX_LEN`] bytes every key of table `prefix` starts with.
pub fn table_prefix(prefix: &str) -> Vec<u8> {
    codec::key(prefix, KeyPart::U32(0))[..TABLE_PREFIX_LEN].to_vec()
}

/// Whether `candidate` is `template` everywhere outside `slots` (ascending,
/// non-overlapping).
fn fits(template: &[u8], candidate: &[u8], slots: &[Slot]) -> bool {
    if candidate.len() != template.len() {
        return false;
    }
    let mut at = 0;
    for &(start, end) in slots {
        if candidate[at..start] != template[at..start] {
            return false;
        }
        at = end;
    }
    candidate[at..] == template[at..]
}

fn fill<const N: usize>(template: &[u8]) -> [u8; N] {
    template.try_into().expect("the template has the record's length")
}

fn u16_at(b: &[u8], slot: Slot) -> u16 {
    u16::from_le_bytes(b[slot.0..slot.1].try_into().expect("2 bytes"))
}

fn u32_at(b: &[u8], slot: Slot) -> u32 {
    u32::from_le_bytes(b[slot.0..slot.1].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], slot: Slot) -> u64 {
    u64::from_le_bytes(b[slot.0..slot.1].try_into().expect("8 bytes"))
}

fn hash_at(b: &[u8], slot: Slot) -> [u8; 32] {
    b[slot.0..slot.1].try_into().expect("32 bytes")
}

/// `DB::serializeKey("6", index)`.
pub fn block_info_key(index: u32) -> [u8; BLOCK_INFO_KEY_LEN] {
    let mut k = fill(&templates().block_info_key);
    k[BLOCK_INFO_KEY_INDEX.0..BLOCK_INFO_KEY_INDEX.1].copy_from_slice(&index.to_le_bytes());
    k
}

/// The block index of a `6` key, or `None` for any other bytes.
pub fn decode_block_info_key(key: &[u8]) -> Option<u32> {
    fits(&templates().block_info_key, key, &[BLOCK_INFO_KEY_INDEX]).then(|| u32_at(key, BLOCK_INFO_KEY_INDEX))
}

/// `DB::serializeKey("7", keyImage)`.
pub fn key_image_key(image: &[u8; 32]) -> [u8; KEY_IMAGE_KEY_LEN] {
    let mut k = fill(&templates().key_image_key);
    k[KEY_IMAGE_KEY_IMAGE.0..KEY_IMAGE_KEY_IMAGE.1].copy_from_slice(image);
    k
}

/// The key image of a `7` key, or `None` for any other bytes.
pub fn decode_key_image_key(key: &[u8]) -> Option<[u8; 32]> {
    fits(&templates().key_image_key, key, &[KEY_IMAGE_KEY_IMAGE]).then(|| hash_at(key, KEY_IMAGE_KEY_IMAGE))
}

/// `DB::serializeKey("j", pair(amount, globalIndex))`.
pub fn key_output_key(amount: u64, global_index: u32) -> [u8; KEY_OUTPUT_KEY_LEN] {
    let mut k = fill(&templates().key_output_key);
    k[KEY_OUTPUT_KEY_AMOUNT.0..KEY_OUTPUT_KEY_AMOUNT.1].copy_from_slice(&amount.to_le_bytes());
    k[KEY_OUTPUT_KEY_INDEX.0..KEY_OUTPUT_KEY_INDEX.1].copy_from_slice(&global_index.to_le_bytes());
    k
}

/// The `(amount, global index)` of a `j` key, or `None` for any other bytes.
pub fn decode_key_output_key(key: &[u8]) -> Option<(u64, u32)> {
    fits(&templates().key_output_key, key, &[KEY_OUTPUT_KEY_AMOUNT, KEY_OUTPUT_KEY_INDEX])
        .then(|| (u64_at(key, KEY_OUTPUT_KEY_AMOUNT), u32_at(key, KEY_OUTPUT_KEY_INDEX)))
}

/// `DB::serialize(CachedBlockInfo, "6")`, byte for byte what
/// [`CachedBlockInfo::encode`] produces.
pub fn block_info_value(info: &CachedBlockInfo) -> [u8; BLOCK_INFO_VALUE_LEN] {
    let mut v = fill(&templates().block_info_value);
    v[BLOCK_INFO_HASH.0..BLOCK_INFO_HASH.1].copy_from_slice(&info.block_hash);
    v[BLOCK_INFO_TIMESTAMP.0..BLOCK_INFO_TIMESTAMP.1].copy_from_slice(&info.timestamp.to_le_bytes());
    v[BLOCK_INFO_SIZE.0..BLOCK_INFO_SIZE.1].copy_from_slice(&info.block_size.to_le_bytes());
    v[BLOCK_INFO_CUMULATIVE_DIFFICULTY.0..BLOCK_INFO_CUMULATIVE_DIFFICULTY.1]
        .copy_from_slice(&info.cumulative_difficulty.to_le_bytes());
    v[BLOCK_INFO_COINS.0..BLOCK_INFO_COINS.1].copy_from_slice(&info.already_generated_coins.to_le_bytes());
    v[BLOCK_INFO_TRANSACTIONS.0..BLOCK_INFO_TRANSACTIONS.1]
        .copy_from_slice(&info.already_generated_transactions.to_le_bytes());
    v
}

/// A `6` value in exactly the layout the C++ writes, or `None`.
pub fn decode_block_info_value(value: &[u8]) -> Option<CachedBlockInfo> {
    let slots = [
        BLOCK_INFO_HASH,
        BLOCK_INFO_TIMESTAMP,
        BLOCK_INFO_SIZE,
        BLOCK_INFO_CUMULATIVE_DIFFICULTY,
        BLOCK_INFO_COINS,
        BLOCK_INFO_TRANSACTIONS,
    ];
    fits(&templates().block_info_value, value, &slots).then(|| CachedBlockInfo {
        block_hash: hash_at(value, BLOCK_INFO_HASH),
        timestamp: u64_at(value, BLOCK_INFO_TIMESTAMP),
        block_size: u32_at(value, BLOCK_INFO_SIZE),
        cumulative_difficulty: u64_at(value, BLOCK_INFO_CUMULATIVE_DIFFICULTY),
        already_generated_coins: u64_at(value, BLOCK_INFO_COINS),
        already_generated_transactions: u64_at(value, BLOCK_INFO_TRANSACTIONS),
    })
}

/// `DB::serialize(uint32_t blockIndex, "7")`.
pub fn key_image_value(spent_at: u32) -> [u8; KEY_IMAGE_VALUE_LEN] {
    let mut v = fill(&templates().key_image_value);
    v[KEY_IMAGE_VALUE_INDEX.0..KEY_IMAGE_VALUE_INDEX.1].copy_from_slice(&spent_at.to_le_bytes());
    v
}

/// The spending block index of a `7` value, or `None`.
pub fn decode_key_image_value(value: &[u8]) -> Option<u32> {
    fits(&templates().key_image_value, value, &[KEY_IMAGE_VALUE_INDEX]).then(|| u32_at(value, KEY_IMAGE_VALUE_INDEX))
}

/// `DB::serialize(KeyOutputInfo, "j")`, byte for byte what
/// [`KeyOutputInfo::encode`] produces.
pub fn key_output_value(info: &KeyOutputInfo) -> [u8; KEY_OUTPUT_VALUE_LEN] {
    let mut v = fill(&templates().key_output_value);
    v[KEY_OUTPUT_PUBLIC_KEY.0..KEY_OUTPUT_PUBLIC_KEY.1].copy_from_slice(&info.public_key);
    v[KEY_OUTPUT_TRANSACTION_HASH.0..KEY_OUTPUT_TRANSACTION_HASH.1].copy_from_slice(&info.transaction_hash);
    v[KEY_OUTPUT_UNLOCK_TIME.0..KEY_OUTPUT_UNLOCK_TIME.1].copy_from_slice(&info.unlock_time.to_le_bytes());
    v[KEY_OUTPUT_OUTPUT_INDEX.0..KEY_OUTPUT_OUTPUT_INDEX.1].copy_from_slice(&info.output_index.to_le_bytes());
    v[KEY_OUTPUT_BLOCK_INDEX.0..KEY_OUTPUT_BLOCK_INDEX.1].copy_from_slice(&info.block_index.to_le_bytes());
    v
}

/// A `j` value in exactly the layout the C++ writes, or `None`.
pub fn decode_key_output_value(value: &[u8]) -> Option<KeyOutputInfo> {
    let slots = [
        KEY_OUTPUT_PUBLIC_KEY,
        KEY_OUTPUT_TRANSACTION_HASH,
        KEY_OUTPUT_UNLOCK_TIME,
        KEY_OUTPUT_OUTPUT_INDEX,
        KEY_OUTPUT_BLOCK_INDEX,
    ];
    fits(&templates().key_output_value, value, &slots).then(|| KeyOutputInfo {
        public_key: hash_at(value, KEY_OUTPUT_PUBLIC_KEY),
        transaction_hash: hash_at(value, KEY_OUTPUT_TRANSACTION_HASH),
        unlock_time: u64_at(value, KEY_OUTPUT_UNLOCK_TIME),
        output_index: u16_at(value, KEY_OUTPUT_OUTPUT_INDEX),
        block_index: u32_at(value, KEY_OUTPUT_BLOCK_INDEX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrkz_primitives::kv::HEADER;

    /// `u8 length, name, u8 type`: one KV entry's framing.
    fn entry(out: &mut Vec<u8>, name: &str, kind: u8) {
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.push(kind);
    }

    /// The shared key preamble, offsets 0..30, written out by hand from the
    /// table in the module documentation rather than by any encoder.
    fn key_preamble(table: u8) -> Vec<u8> {
        let mut k = HEADER.to_vec();
        k.extend_from_slice(&[0x04, 0x01, table, 0x0C, 0x08]);
        entry(&mut k, "first", 0x0A);
        k.extend_from_slice(&[0x04, table]);
        k.push(6);
        k.extend_from_slice(b"second");
        assert_eq!(k.len(), 30);
        k
    }

    #[test]
    fn a_block_info_record_is_the_bytes_the_cpp_writes() {
        let mut want_key = key_preamble(b'6');
        want_key.push(0x06);
        want_key.extend_from_slice(&[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(block_info_key(0x0102_0304).as_slice(), want_key.as_slice());
        assert_eq!(decode_block_info_key(&want_key), Some(0x0102_0304));

        let info = CachedBlockInfo {
            block_hash: [0xAB; 32],
            timestamp: 0x1122_3344_5566_7788,
            block_size: 0x99AA_BBCC,
            cumulative_difficulty: 0x0102_0304_0506_0708,
            already_generated_coins: 0x1112_1314_1516_1718,
            already_generated_transactions: 0x2122_2324_2526_2728,
        };
        let mut want = HEADER.to_vec();
        want.extend_from_slice(&[0x04, 0x01, b'6', 0x0C, 0x18]);
        entry(&mut want, "block_hash", 0x0A);
        want.push(0x80);
        want.extend_from_slice(&[0xAB; 32]);
        entry(&mut want, "timestamp", 0x05);
        want.extend_from_slice(&info.timestamp.to_le_bytes());
        entry(&mut want, "block_size", 0x06);
        want.extend_from_slice(&info.block_size.to_le_bytes());
        entry(&mut want, "cumulative_difficulty", 0x05);
        want.extend_from_slice(&info.cumulative_difficulty.to_le_bytes());
        entry(&mut want, "already_generated_coins", 0x05);
        want.extend_from_slice(&info.already_generated_coins.to_le_bytes());
        entry(&mut want, "already_generated_transaction_count", 0x05);
        want.extend_from_slice(&info.already_generated_transactions.to_le_bytes());
        assert_eq!(want.len(), BLOCK_INFO_VALUE_LEN);
        assert_eq!(block_info_value(&info).as_slice(), want.as_slice());
        assert_eq!(info.encode(), want, "and the generic encoder agrees");
        assert_eq!(decode_block_info_value(&want), Some(info));
    }

    #[test]
    fn a_key_image_record_is_the_bytes_the_cpp_writes() {
        let image: [u8; 32] = std::array::from_fn(|i| i as u8);
        let mut want_key = key_preamble(b'7');
        want_key.extend_from_slice(&[0x0A, 0x80]);
        want_key.extend_from_slice(&image);
        assert_eq!(want_key.len(), KEY_IMAGE_KEY_LEN);
        assert_eq!(key_image_key(&image).as_slice(), want_key.as_slice());
        assert_eq!(decode_key_image_key(&want_key), Some(image));

        let mut want = HEADER.to_vec();
        want.extend_from_slice(&[0x04, 0x01, b'7', 0x06]);
        want.extend_from_slice(&3_999_999u32.to_le_bytes());
        assert_eq!(key_image_value(3_999_999).as_slice(), want.as_slice());
        assert_eq!(decode_key_image_value(&want), Some(3_999_999));
    }

    #[test]
    fn a_key_output_record_is_the_bytes_the_cpp_writes() {
        let mut want_key = key_preamble(b'j');
        want_key.extend_from_slice(&[0x0C, 0x08]);
        entry(&mut want_key, "first", 0x05);
        want_key.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        entry(&mut want_key, "second", 0x06);
        want_key.extend_from_slice(&0x0A0B_0C0Du32.to_le_bytes());
        assert_eq!(want_key.len(), KEY_OUTPUT_KEY_LEN);
        assert_eq!(key_output_key(0x0102_0304_0506_0708, 0x0A0B_0C0D).as_slice(), want_key.as_slice());
        assert_eq!(decode_key_output_key(&want_key), Some((0x0102_0304_0506_0708, 0x0A0B_0C0D)));

        let info = KeyOutputInfo {
            public_key: [0x11; 32],
            transaction_hash: [0x22; 32],
            unlock_time: 4_000_040,
            output_index: 0xBEEF,
            block_index: 3_999_990,
        };
        let mut want = HEADER.to_vec();
        want.extend_from_slice(&[0x04, 0x01, b'j', 0x0C, 0x14]);
        entry(&mut want, "public_key", 0x0A);
        want.push(0x80);
        want.extend_from_slice(&[0x11; 32]);
        entry(&mut want, "transaction_hash", 0x0A);
        want.push(0x80);
        want.extend_from_slice(&[0x22; 32]);
        entry(&mut want, "unlock_time", 0x05);
        want.extend_from_slice(&info.unlock_time.to_le_bytes());
        entry(&mut want, "output_index", 0x07);
        want.extend_from_slice(&info.output_index.to_le_bytes());
        entry(&mut want, "block_index", 0x06);
        want.extend_from_slice(&info.block_index.to_le_bytes());
        assert_eq!(want.len(), KEY_OUTPUT_VALUE_LEN);
        assert_eq!(key_output_value(&info).as_slice(), want.as_slice());
        assert_eq!(info.encode(), want, "and the generic encoder agrees");
        assert_eq!(decode_key_output_value(&want), Some(info));
    }

    /// The slot constants are checked against the generic encoder with values
    /// whose every byte is distinct, so a slot one byte off cannot pass.
    #[test]
    fn the_slots_are_where_the_generic_encoder_puts_the_values() {
        for i in [0u32, 1, 255, 256, 0xDEAD_BEEF, u32::MAX] {
            assert_eq!(block_info_key(i).to_vec(), codec::key("6", KeyPart::U32(i)));
        }
        let image = [0x5A; 32];
        assert_eq!(key_image_key(&image).to_vec(), codec::key("7", KeyPart::Hash(image)));
        for (a, g) in [(0u64, 0u32), (1, 1), (u64::MAX, u32::MAX), (10_000, 1_787_441)] {
            assert_eq!(key_output_key(a, g).to_vec(), codec::key("j", KeyPart::AmountIndex(a, g)));
        }
        assert_eq!(key_image_value(7).to_vec(), codec::value_u32("7", 7));
        let info = CachedBlockInfo {
            block_hash: std::array::from_fn(|i| 100 + i as u8),
            timestamp: u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]),
            block_size: u32::from_le_bytes([9, 10, 11, 12]),
            cumulative_difficulty: u64::from_le_bytes([13, 14, 15, 16, 17, 18, 19, 20]),
            already_generated_coins: u64::from_le_bytes([21, 22, 23, 24, 25, 26, 27, 28]),
            already_generated_transactions: u64::from_le_bytes([29, 30, 31, 32, 33, 34, 35, 36]),
        };
        assert_eq!(block_info_value(&info).to_vec(), info.encode());
        let output = KeyOutputInfo {
            public_key: std::array::from_fn(|i| 50 + i as u8),
            transaction_hash: std::array::from_fn(|i| 150 + i as u8),
            unlock_time: u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]),
            output_index: u16::from_le_bytes([9, 10]),
            block_index: u32::from_le_bytes([11, 12, 13, 14]),
        };
        assert_eq!(key_output_value(&output).to_vec(), output.encode());
    }

    #[test]
    fn the_table_prefixes_sort_six_before_seven_before_j() {
        let (six, seven, j) = (table_prefix("6"), table_prefix("7"), table_prefix("j"));
        assert_eq!(six.len(), TABLE_PREFIX_LEN);
        assert!(six < seven && seven < j);
        assert!(block_info_key(u32::MAX).starts_with(&six));
        assert!(key_image_key(&[0xFF; 32]).starts_with(&seven));
        assert!(key_output_key(0, 0).starts_with(&j));
        assert!(block_info_key(u32::MAX).as_slice() < key_image_key(&[0; 32]).as_slice());
        assert!(key_image_key(&[0xFF; 32]).as_slice() < key_output_key(0, 0).as_slice());
    }

    /// The decoders refuse anything that is not the exact layout: another
    /// table's record, a truncated one, a retyped field.
    #[test]
    fn the_decoders_refuse_every_other_shape() {
        let key = block_info_key(5);
        assert_eq!(decode_key_image_key(&key), None);
        assert_eq!(decode_key_output_key(&key), None);
        assert_eq!(decode_block_info_key(&key[..34]), None);
        let mut retyped = key;
        retyped[30] = 0x05;
        assert_eq!(decode_block_info_key(&retyped), None, "a u64 where the index is a u32");
        let mut other_table = key;
        other_table[11] = b'8';
        assert_eq!(decode_block_info_key(&other_table), None);

        let value = key_output_value(&KeyOutputInfo::default());
        assert_eq!(decode_block_info_value(&value), None);
        let mut renamed = value;
        renamed[15] = b'P';
        assert_eq!(decode_key_output_value(&renamed), None, "a field name differs");
        let mut longer = value.to_vec();
        longer.push(0);
        assert_eq!(decode_key_output_value(&longer), None);
        assert_eq!(decode_key_image_value(&key_image_value(1)[..16]), None);
    }
}
