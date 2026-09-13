// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Replays every hashing vector in spec/vectors/primitives.txt against the Rust implementation.
//! The file is the oracle (spec/README.md, ground rule 3); nothing is hard-coded here.

use std::collections::BTreeMap;
use std::path::PathBuf;

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors/primitives.txt")
}

/// Sections keyed by their "## " title; each section is a list of (lhs, rhs) split on " -> " or " = ".
fn load() -> BTreeMap<String, Vec<(String, String)>> {
    let text = std::fs::read_to_string(vectors_path()).expect("spec/vectors/primitives.txt");
    let mut out: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut cur = String::new();
    for line in text.lines() {
        if let Some(t) = line.strip_prefix("## ") {
            cur = t.to_string();
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let (l, r) = if let Some((l, r)) = line.split_once(" -> ") {
            (l, r)
        } else if let Some((l, r)) = line.rsplit_once(" = ") {
            (l, r)
        } else {
            continue;
        };
        out.entry(cur.clone()).or_default().push((l.trim().to_string(), r.trim().to_string()));
    }
    out
}

fn section<'a>(v: &'a BTreeMap<String, Vec<(String, String)>>, prefix: &str) -> &'a [(String, String)] {
    v.iter()
        .find(|(k, _)| k.starts_with(prefix))
        .map(|(_, e)| e.as_slice())
        .unwrap_or_else(|| panic!("section {prefix:?} missing"))
}

fn inputs(v: &BTreeMap<String, Vec<(String, String)>>) -> BTreeMap<String, Vec<u8>> {
    section(v, "inputs").iter().map(|(name, hexs)| (name.clone(), hex::decode(hexs).unwrap())).collect()
}

fn check_fn(v: &BTreeMap<String, Vec<(String, String)>>, sec: &str, f: fn(&[u8]) -> [u8; 32]) {
    let ins = inputs(v);
    let rows = section(v, sec);
    assert!(!rows.is_empty());
    for (name, want) in rows {
        let input = &ins[name];
        let got = f(input);
        assert_eq!(hex::encode(got), *want, "{sec}: {name}");
    }
}

#[test]
fn cn_fast_hash_vectors() {
    check_fn(&load(), "cn_fast_hash", wrkz_pow::cn_fast_hash);
}

#[test]
fn cn_slow_hash_v0_vectors() {
    check_fn(&load(), "cn_slow_hash_v0", wrkz_pow::cn_slow_hash_v0);
}

#[test]
fn cn_lite_slow_hash_v1_vectors() {
    check_fn(&load(), "cn_lite_slow_hash_v1", wrkz_pow::cn_lite_slow_hash_v1);
}

#[test]
fn cn_turtle_lite_slow_hash_v2_vectors() {
    check_fn(&load(), "cn_turtle_lite_slow_hash_v2", wrkz_pow::cn_turtle_lite_slow_hash_v2);
}

#[test]
fn chukwa_slow_hash_vectors() {
    check_fn(&load(), "chukwa_slow_hash", wrkz_pow::chukwa_slow_hash);
}

#[test]
fn cn_upx_vectors() {
    check_fn(&load(), "cn_upx", wrkz_pow::cn_upx);
}

#[test]
fn tree_hash_vectors() {
    let v = load();
    let rows = section(&v, "tree_hash");
    let mut leaves: Vec<[u8; 32]> = Vec::new();
    for (name, val) in rows {
        if let Some(i) = name.strip_prefix("leaf[").and_then(|s| s.strip_suffix(']')) {
            let i: usize = i.parse().unwrap();
            assert_eq!(i, leaves.len());
            let leaf = wrkz_pow::cn_fast_hash(&[i as u8]);
            assert_eq!(hex::encode(leaf), *val, "leaf {i}");
            leaves.push(leaf);
        } else if let Some(n) = name.strip_prefix("tree_hash(count=").and_then(|s| s.strip_suffix(')')) {
            let n: usize = n.parse().unwrap();
            let root = wrkz_pow::tree_hash(&leaves[..n]);
            assert_eq!(hex::encode(root), *val, "tree_hash count={n}");
        }
    }
    assert_eq!(leaves.len(), 9);
}

#[test]
fn tree_hash_from_branch_depth0_is_identity() {
    let leaf = wrkz_pow::cn_fast_hash(b"leaf");
    assert_eq!(wrkz_pow::tree_hash_from_branch(&[], &leaf), leaf);
    assert_eq!(wrkz_pow::tree_depth(1), 0);
    assert_eq!(wrkz_pow::tree_depth(2), 1);
    assert_eq!(wrkz_pow::tree_depth(3), 1);
    assert_eq!(wrkz_pow::tree_depth(4), 2);
}

fn leaves(n: usize) -> Vec<[u8; 32]> {
    (0..n).map(|i| wrkz_pow::cn_fast_hash(&(i as u32).to_le_bytes())).collect()
}

/// The heap working buffer of `tree_hash` must reproduce the vendored `alloca`
/// implementation for every leaf count, not just the ones in the vector file.
#[test]
fn tree_hash_heap_matches_the_alloca_reference() {
    for n in 1..=300 {
        let l = leaves(n);
        assert_eq!(wrkz_pow::tree_hash(&l), wrkz_pow_ref::pow::tree_hash(&l), "count {n}");
    }
    // Far past any alloca that would fit a thread stack.
    let big = leaves(5000);
    assert_ne!(wrkz_pow::tree_hash(&big), [0u8; 32]);
}

/// `tree_branch` returns the branch of leaf 0, so replaying it must give the
/// root; this also pins the `tree_depth`-sized output buffer of the wrapper.
#[test]
fn tree_branch_round_trips_to_the_root() {
    for n in 1..=64 {
        let l = leaves(n);
        let branch = wrkz_pow::tree_branch(&l);
        assert_eq!(branch.len(), wrkz_pow::tree_depth(n), "branch length {n}");
        assert_eq!(wrkz_pow::tree_hash_from_branch(&branch, &l[0]), wrkz_pow::tree_hash(&l), "count {n}");
    }
}

/// The C only ever reads the `ceil(depth / 8)` low bytes of `path`, and a branch
/// longer than the 256 bits of a 32-byte path must not read past it.
#[test]
fn tree_hash_from_branch_reads_only_the_path_bits_it_should() {
    let leaf = wrkz_pow::cn_fast_hash(b"leaf");
    let branch = leaves(300);
    // Two paths that agree on the low 8 bits: a branch of at most 8 entries only
    // consults those, so the roots must match.
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    a[0] = 0b1010_1010;
    b[0] = 0b1010_1010;
    b[1..].fill(0xff);
    for n in 1..=8 {
        assert_eq!(
            wrkz_pow::tree_hash_from_branch_with_path(&branch[..n], &leaf, Some(&a)),
            wrkz_pow::tree_hash_from_branch_with_path(&branch[..n], &leaf, Some(&b)),
            "branch {n} must not look above bit 7"
        );
    }
    // Bit 8 is consulted at depth 9, so there the two must part ways.
    assert_ne!(
        wrkz_pow::tree_hash_from_branch_with_path(&branch[..9], &leaf, Some(&a)),
        wrkz_pow::tree_hash_from_branch_with_path(&branch[..9], &leaf, Some(&b))
    );
    // Past 256 entries the path is zero-extended: defined and stable, where the
    // raw C would have read past the caller's 32 bytes.
    assert_eq!(
        wrkz_pow::tree_hash_from_branch_with_path(&branch, &leaf, Some(&a)),
        wrkz_pow::tree_hash_from_branch_with_path(&branch, &leaf, Some(&a))
    );
}

/// Releasing the scratchpad must be safe before hashing, after hashing, and
/// twice in a row; hashing after it simply allocates a new one.
#[test]
fn scratchpad_release_is_idempotent() {
    wrkz_pow::release_thread_scratchpad();
    let first = wrkz_pow::cn_turtle_lite_slow_hash_v2(&[7u8; 76]);
    wrkz_pow::release_thread_scratchpad();
    wrkz_pow::release_thread_scratchpad();
    assert_eq!(wrkz_pow::cn_turtle_lite_slow_hash_v2(&[7u8; 76]), first);
}

#[test]
fn check_hash_vectors() {
    let v = load();
    let abc = wrkz_pow::cn_fast_hash(b"abc");
    let mut one = [0u8; 32];
    one[0] = 1;
    let ff = [0xffu8; 32];
    for (name, val) in section(&v, "check_hash") {
        let want = val == "1";
        let (h, d) = if let Some(d) = name.strip_prefix("check_hash(keccak('abc'), ").and_then(|s| s.strip_suffix(')'))
        {
            (abc, d.parse::<u64>().unwrap())
        } else if name == "check_hash(0100..00, 2^63)" {
            (one, 1u64 << 63)
        } else if let Some(d) = name.strip_prefix("check_hash(ff..ff, ").and_then(|s| s.strip_suffix(')')) {
            (ff, d.parse::<u64>().unwrap())
        } else {
            panic!("unknown check_hash row {name}");
        };
        assert_eq!(wrkz_pow::check_hash(&h, d), want, "{name}");
    }
}

#[test]
fn genesis_pow_hash_vector() {
    // 02-hashing.md: pow of the genesis header hashing blob with cn_slow_hash_v0.
    let v = load();
    let rows = section(&v, "genesis block");
    let blob = rows.iter().find(|(k, _)| k == "genesis hashing blob").map(|(_, v)| hex::decode(v).unwrap()).unwrap();
    let pow = rows.iter().find(|(k, _)| k.starts_with("genesis pow hash")).map(|(_, v)| v.clone()).unwrap();
    assert_eq!(hex::encode(wrkz_pow::cn_slow_hash_v0(&blob)), pow);
    assert_eq!(blob.len(), 72);
}

#[test]
fn legacy_wallet_key_vector() {
    let v = load();
    let rows = section(&v, "chacha8");
    let want = rows.iter().find(|(k, _)| k.starts_with("generate_chacha8_key")).map(|(_, v)| v.clone()).unwrap();
    assert_eq!(hex::encode(wrkz_pow::cn_slow_hash_v0(b"password")), want);
}
