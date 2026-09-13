// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The Merkle tree of transaction hashes (`tree-hash.c`).
//!
//! A port of the vendored C with its working buffers on the heap. The C sizes
//! an `alloca` from the leaf count (`tree-hash.c:39`, ~16 bytes per leaf), and
//! the leaf count of a block comes off the wire; here a peer can only make the
//! node allocate, never run a thread's stack out.

use crate::{cn_fast_hash, Hash};

/// `cn_fast_hash(left || right)`.
fn hash_pair(left: &Hash, right: &Hash) -> Hash {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left);
    buf[32..].copy_from_slice(right);
    cn_fast_hash(&buf)
}

/// The largest power of two `<= n`, for `n >= 1`.
fn floor_pow2(n: usize) -> usize {
    1 << (usize::BITS - 1 - n.leading_zeros())
}

/// `tree_hash` (`tree-hash.c:18`). `hashes` must be non-empty.
pub fn tree_hash(hashes: &[Hash]) -> Hash {
    assert!(!hashes.is_empty(), "tree_hash of zero leaves");
    let count = hashes.len();
    match count {
        1 => hashes[0],
        2 => hash_pair(&hashes[0], &hashes[1]),
        _ => {
            // The largest power of two strictly below `count` (tree-hash.c:32).
            let mut cnt = floor_pow2(count - 1);
            let direct = 2 * cnt - count;
            let mut ints: Vec<Hash> = Vec::with_capacity(cnt);
            ints.extend_from_slice(&hashes[..direct]);
            ints.extend(hashes[direct..].as_chunks::<2>().0.iter().map(|[l, r]| hash_pair(l, r)));
            debug_assert_eq!(ints.len(), cnt);
            while cnt > 2 {
                cnt >>= 1;
                for j in 0..cnt {
                    ints[j] = hash_pair(&ints[2 * j], &ints[2 * j + 1]);
                }
            }
            hash_pair(&ints[0], &ints[1])
        }
    }
}

/// `tree_depth` (`tree-hash.c:58`): `floor(log2(count))`, and 0 for 0 as the
/// C gives with its assertion compiled out.
pub fn tree_depth(count: usize) -> usize {
    if count == 0 {
        0
    } else {
        (usize::BITS - 1 - count.leading_zeros()) as usize
    }
}

/// `tree_branch` (`tree-hash.c:74`): the Merkle branch of leaf 0, deepest
/// entry last. `hashes` must be non-empty.
pub fn tree_branch(hashes: &[Hash]) -> Vec<Hash> {
    assert!(!hashes.is_empty(), "tree_branch of zero leaves");
    let count = hashes.len();
    let mut cnt = floor_pow2(count);
    let mut depth = tree_depth(count);
    let mut branch = vec![[0u8; 32]; depth];
    // ints holds cnt - 1 entries: the leaves after the first that are not
    // paired at the bottom level, then the bottom-level pair hashes.
    let direct = 2 * cnt - count;
    let mut ints: Vec<Hash> = Vec::with_capacity(cnt - 1);
    ints.extend_from_slice(&hashes[1..direct]);
    ints.extend(hashes[direct..].as_chunks::<2>().0.iter().map(|[l, r]| hash_pair(l, r)));
    debug_assert_eq!(ints.len(), cnt - 1);
    while depth > 0 {
        cnt >>= 1;
        depth -= 1;
        branch[depth] = ints[0];
        for j in 0..cnt - 1 {
            ints[j] = hash_pair(&ints[2 * j + 1], &ints[2 * j + 2]);
        }
    }
    branch
}

/// `tree_hash_from_branch(branch, depth, leaf, path)` (`tree-hash.c:111`).
///
/// The C reads bit `d` of `path` for every `d` below the branch length, so a
/// 32-byte path only covers 256 entries; bits beyond the path read as zero,
/// which is what the zero-extended copy the FFI wrapper used to pass gave.
pub fn tree_hash_from_branch_with_path(branch: &[Hash], leaf: &Hash, path: Option<&Hash>) -> Hash {
    if branch.is_empty() {
        return *leaf;
    }
    let mut buffer = [[0u8; 32]; 2];
    let mut from_leaf = true;
    for depth in (0..branch.len()).rev() {
        let bit = path.and_then(|p| p.get(depth >> 3)).is_some_and(|byte| byte & (1 << (depth & 7)) != 0);
        let (leaf_side, branch_side) = if bit { (1, 0) } else { (0, 1) };
        buffer[leaf_side] = if from_leaf {
            from_leaf = false;
            *leaf
        } else {
            hash_pair(&buffer[0], &buffer[1])
        };
        buffer[branch_side] = branch[depth];
    }
    hash_pair(&buffer[0], &buffer[1])
}
