// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The Rust hashing of wrkz-pow against the reference C it replaced
//! (wrkz-pow-ref, feature `pow`): identical output on every input. The first
//! byte picks the function. The 1-2 MiB CryptoNight variants are too slow to
//! fuzz usefully; `crates/wrkz-pow/tests/reference_diff.rs` covers them.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_pow::{CN_TURTLE_LITE_V2, CN_UPX};
use wrkz_pow_ref::pow as c;

fuzz_target!(|data: &[u8]| {
    let Some((&select, rest)) = data.split_first() else { return };
    match select % 6 {
        0 => {
            assert_eq!(wrkz_pow::cn_fast_hash(rest), wrkz_pow_ref::cn_fast_hash(rest));
            assert_eq!(wrkz_pow::keccak1600(rest), wrkz_pow_ref::keccak1600(rest));
        }
        1 => {
            let which = (select as usize / 6) & 3;
            assert_eq!(wrkz_pow::cn_finalizer(which, rest), c::hash_extra(which, rest));
        }
        2 => {
            let leaves: Vec<[u8; 32]> = rest.chunks_exact(32).take(4096).map(|l| l.try_into().unwrap()).collect();
            if leaves.is_empty() {
                return;
            }
            assert_eq!(wrkz_pow::tree_hash(&leaves), c::tree_hash(&leaves));
            assert_eq!(wrkz_pow::tree_branch(&leaves), c::tree_branch(&leaves));
            if leaves.len() >= 2 {
                let (path, leaf) = (&leaves[0], &leaves[1]);
                let branch = &leaves[2..leaves.len().min(2 + 256)];
                assert_eq!(
                    wrkz_pow::tree_hash_from_branch_with_path(branch, leaf, Some(path)),
                    c::tree_hash_from_branch(branch, leaf, Some(path))
                );
            }
        }
        3 => {
            if rest.len() >= 16 {
                assert_eq!(wrkz_pow::chukwa_slow_hash(rest), c::argon2id(4, 256, 1, rest, &rest[..16]));
            }
        }
        4 => {
            let p = CN_TURTLE_LITE_V2;
            let want = c::cn_slow_hash(rest, p.light, p.variant, p.page_size, p.scratchpad, p.iterations, p.mask);
            assert_eq!(wrkz_pow::cn_slow_hash(rest, p), want);
        }
        _ => {
            let p = CN_UPX;
            let want = c::cn_slow_hash(rest, p.light, p.variant, p.page_size, p.scratchpad, p.iterations, p.mask);
            assert_eq!(wrkz_pow::cn_slow_hash(rest, p), want);
        }
    }
});
