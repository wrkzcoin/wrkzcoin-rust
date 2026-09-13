// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The reference C code of wrkzcoin commit `8d89d7bf`, compiled unchanged.
//!
//! Two uses, kept apart by two features. **With neither, this crate compiles
//! no C**, which is the point of the split: the day `wrkz_pow::curve` is
//! native Rust, `wrkz-pow` changes its dependency here to
//! `default-features = false` and the C compiler leaves the build
//! instructions with it.
//!
//! - [`curve`] (feature `curve`, on by default) is the ref10 curve code and
//!   the `crypto.cpp` port in `c/cn_shim.c`. `wrkz_pow::curve` still calls it
//!   in production, until a native curve port lands. Keccak is compiled with
//!   it, and exposed as [`cn_fast_hash`], [`keccak1600`] and [`keccakf`],
//!   because the shim hashes with `cn_fast_hash`.
//! - `pow` (feature `pow`, which implies `curve`) is the proof-of-work C: the
//!   CryptoNight family and its finalizers, argon2 and the tree hash.
//!   `wrkz-pow` implements all of it in Rust; this module is the oracle its
//!   differential tests and the fuzz targets compare against, and no shipped
//!   binary links it.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(feature = "curve")]
use std::os::raw::{c_char, c_void};

/// `malloc` and `free` for the C in a browser build, where
/// wasm32-unknown-unknown has no C library (`compat/wasm32/stdlib.h`). Each
/// block carries its size in a 16-byte header so `free` can rebuild the
/// layout; 16 is also the alignment C expects from `malloc`.
#[cfg(all(feature = "curve", target_arch = "wasm32", target_os = "unknown"))]
mod wasm_libc {
    use std::alloc::{alloc, dealloc, Layout};

    const HEADER: usize = 16;

    fn layout(size: usize) -> Option<Layout> {
        Layout::from_size_align(size.checked_add(HEADER)?, HEADER).ok()
    }

    /// # Safety
    /// C `malloc`: the result is freed with [`free`] or leaked, never both.
    #[no_mangle]
    pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
        let Some(layout) = layout(size) else { return std::ptr::null_mut() };
        // SAFETY: `layout` has a non-zero size (at least HEADER).
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            return base;
        }
        // SAFETY: `base` is valid for HEADER bytes, aligned to 16.
        unsafe {
            (base as *mut usize).write(size);
            base.add(HEADER)
        }
    }

    /// # Safety
    /// C `free`: `p` is null or came from [`malloc`] and was not freed yet.
    #[no_mangle]
    pub unsafe extern "C" fn free(p: *mut u8) {
        if p.is_null() {
            return;
        }
        // SAFETY: `malloc` returned `base + HEADER` and wrote the size at `base`.
        unsafe {
            let base = p.sub(HEADER);
            let size = (base as *const usize).read();
            dealloc(base, layout(size).expect("layout malloc accepted"));
        }
    }
}

#[cfg(feature = "curve")]
mod ffi {
    use super::*;

    extern "C" {
        pub fn cn_fast_hash(data: *const c_void, length: usize, hash: *mut c_char);
        pub fn hash_process(state: *mut u8, buf: *const u8, count: usize);
        pub fn hash_permutation(state: *mut u8);
    }
}

/// C `cn_fast_hash`: Keccak-256 with the original padding.
#[cfg(feature = "curve")]
pub fn cn_fast_hash(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    // SAFETY: C reads `data.len()` bytes and writes exactly 32.
    unsafe { ffi::cn_fast_hash(data.as_ptr() as *const c_void, data.len(), out.as_mut_ptr() as *mut c_char) };
    out
}

/// C `hash_process`: the whole 200-byte Keccak-1600 state after absorbing `data`.
#[cfg(feature = "curve")]
pub fn keccak1600(data: &[u8]) -> [u8; 200] {
    let mut state = [0u8; 200];
    // SAFETY: `state` is the 200-byte `union hash_state`; C reads `data.len()` bytes.
    unsafe { ffi::hash_process(state.as_mut_ptr(), data.as_ptr(), data.len()) };
    state
}

/// C `hash_permutation`: Keccak-f\[1600\], 24 rounds, in place.
#[cfg(feature = "curve")]
pub fn keccakf(state: &mut [u8; 200]) {
    // SAFETY: `state` is the 200-byte `union hash_state`, permuted in place.
    unsafe { ffi::hash_permutation(state.as_mut_ptr()) };
}

/// The `crypto.cpp` primitives of `c/cn_shim.c`, as raw declarations.
/// `wrkz_pow::curve` wraps every one of them in a safe function.
#[cfg(feature = "curve")]
pub mod curve {
    use std::os::raw::c_int;

    extern "C" {
        pub fn wrkz_hash_to_scalar(data: *const u8, len: usize, out: *mut u8);
        pub fn wrkz_sc_reduce32(s: *mut u8);
        pub fn wrkz_scalar_from_64_bytes(rnd: *const u8, out: *mut u8);
        pub fn wrkz_sc_check(s: *const u8) -> c_int;
        pub fn wrkz_sc_add(r: *mut u8, a: *const u8, b: *const u8);
        pub fn wrkz_sc_sub(r: *mut u8, a: *const u8, b: *const u8);
        pub fn wrkz_sc_mul(r: *mut u8, a: *const u8, b: *const u8);
        pub fn wrkz_sc_mulsub(r: *mut u8, a: *const u8, b: *const u8, c: *const u8);
        pub fn wrkz_check_key(pub_: *const u8) -> c_int;
        pub fn wrkz_secret_key_to_public_key(sec: *const u8, pub_: *mut u8) -> c_int;
        pub fn wrkz_generate_deterministic_keys(seed: *const u8, sec: *mut u8, pub_: *mut u8);
        pub fn wrkz_generate_view_from_spend(spend: *const u8, view_sec: *mut u8, view_pub: *mut u8);
        pub fn wrkz_generate_deterministic_subwallet_key(base: *const u8, index: u64, out: *mut u8);
        pub fn wrkz_generate_key_derivation(pub_: *const u8, sec: *const u8, out: *mut u8) -> c_int;
        pub fn wrkz_derivation_to_scalar(der: *const u8, idx: u64, out: *mut u8);
        pub fn wrkz_derive_public_key(der: *const u8, idx: u64, base: *const u8, out: *mut u8) -> c_int;
        pub fn wrkz_derive_secret_key(der: *const u8, idx: u64, base: *const u8, out: *mut u8);
        pub fn wrkz_underive_public_key(der: *const u8, idx: u64, derived: *const u8, out: *mut u8) -> c_int;
        pub fn wrkz_hash_data_to_ec(data: *const u8, len: usize, out: *mut u8);
        pub fn wrkz_generate_key_image(pub_: *const u8, sec: *const u8, out: *mut u8);
        pub fn wrkz_scalarmult_key(p: *const u8, a: *const u8, out: *mut u8) -> c_int;
        pub fn wrkz_generate_signature(prefix: *const u8, pub_: *const u8, sec: *const u8, k: *const u8, sig: *mut u8);
        pub fn wrkz_check_signature(prefix: *const u8, pub_: *const u8, sig: *const u8) -> c_int;
        pub fn wrkz_generate_ring_signature(
            prefix: *const u8,
            image: *const u8,
            pubs: *const u8,
            n: usize,
            sec: *const u8,
            real: u64,
            k: *const u8,
            sigs: *mut u8,
        ) -> c_int;
        pub fn wrkz_check_ring_signature(
            prefix: *const u8,
            image: *const u8,
            pubs: *const u8,
            n: usize,
            sigs: *const u8,
        ) -> c_int;
    }
}

/// The proof-of-work C, wrapped just enough to be called from tests.
#[cfg(feature = "pow")]
pub mod pow {
    use std::os::raw::{c_char, c_int, c_void};

    mod ffi {
        use super::*;

        extern "C" {
            pub fn cn_slow_hash(
                data: *const c_void,
                length: usize,
                hash: *mut c_char,
                light: c_int,
                variant: c_int,
                prehashed: c_int,
                page_size: u32,
                scratchpad: u32,
                iterations: u32,
                mask: u64,
            );
            pub fn hash_extra_blake(data: *const c_void, length: usize, hash: *mut c_char);
            pub fn hash_extra_groestl(data: *const c_void, length: usize, hash: *mut c_char);
            pub fn hash_extra_jh(data: *const c_void, length: usize, hash: *mut c_char);
            pub fn hash_extra_skein(data: *const c_void, length: usize, hash: *mut c_char);
            pub fn tree_hash(hashes: *const [u8; 32], count: usize, root: *mut c_char);
            pub fn tree_depth(count: usize) -> usize;
            pub fn tree_branch(hashes: *const [u8; 32], count: usize, branch: *mut [u8; 32]);
            pub fn tree_hash_from_branch(
                branch: *const [u8; 32],
                depth: usize,
                leaf: *const c_char,
                path: *const c_void,
                root: *mut c_char,
            );
            pub fn argon2id_hash_raw(
                t_cost: u32,
                m_cost: u32,
                parallelism: u32,
                pwd: *const c_void,
                pwdlen: usize,
                salt: *const c_void,
                saltlen: usize,
                hash: *mut c_void,
                hashlen: usize,
            ) -> c_int;
            pub fn wrkz_release_scratchpad();
        }
    }

    /// `cn_slow_hash` (`hash-ops.h:64`) with `prehashed = 0`. The C aborts the
    /// process for variant 1 below 43 bytes, so that is refused here instead.
    pub fn cn_slow_hash(
        data: &[u8],
        light: i32,
        variant: i32,
        page_size: u32,
        scratchpad: u32,
        iterations: u32,
        mask: u64,
    ) -> [u8; 32] {
        assert!(variant != 1 || data.len() >= 43, "variant 1 requires >= 43 bytes of input");
        let mut out = [0u8; 32];
        // SAFETY: C reads `data.len()` bytes and writes 32; the scratchpad is its own.
        unsafe {
            ffi::cn_slow_hash(
                data.as_ptr() as *const c_void,
                data.len(),
                out.as_mut_ptr() as *mut c_char,
                light,
                variant,
                0,
                page_size,
                scratchpad,
                iterations,
                mask,
            )
        };
        out
    }

    /// The four CryptoNight finalizers, indexed as `extra_hashes[]` is
    /// (`slow-hash-x86.c:578`): 0 Blake-256, 1 Groestl-256, 2 JH-256, 3 Skein-512-256.
    pub fn hash_extra(which: usize, data: &[u8]) -> [u8; 32] {
        let f = [ffi::hash_extra_blake, ffi::hash_extra_groestl, ffi::hash_extra_jh, ffi::hash_extra_skein][which & 3];
        let mut out = [0u8; 32];
        // SAFETY: C reads `data.len()` bytes and writes 32.
        unsafe { f(data.as_ptr() as *const c_void, data.len(), out.as_mut_ptr() as *mut c_char) };
        out
    }

    /// The vendored `tree_hash`, which `alloca`s ~16 bytes per leaf; refused
    /// above 4096 leaves so a test cannot run the stack out.
    pub fn tree_hash(hashes: &[[u8; 32]]) -> [u8; 32] {
        assert!(!hashes.is_empty() && hashes.len() <= 4096, "reference tree_hash takes 1..=4096 leaves");
        let mut out = [0u8; 32];
        // SAFETY: `hashes` holds `count` entries; C writes 32 bytes.
        unsafe { ffi::tree_hash(hashes.as_ptr(), hashes.len(), out.as_mut_ptr() as *mut c_char) };
        out
    }

    pub fn tree_depth(count: usize) -> usize {
        assert!(count > 0);
        // SAFETY: pure function of an integer.
        unsafe { ffi::tree_depth(count) }
    }

    /// The vendored `tree_branch` (also `alloca`-bound, same limit).
    pub fn tree_branch(hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
        assert!(!hashes.is_empty() && hashes.len() <= 4096, "reference tree_branch takes 1..=4096 leaves");
        let mut out = vec![[0u8; 32]; tree_depth(hashes.len())];
        // SAFETY: `out` has `tree_depth(count)` entries, as C writes.
        unsafe { ffi::tree_branch(hashes.as_ptr(), hashes.len(), out.as_mut_ptr()) };
        out
    }

    /// The vendored `tree_hash_from_branch`. C reads `path[depth >> 3]` for every
    /// depth below the branch length, so `path` must cover `ceil(len / 8)` bytes.
    pub fn tree_hash_from_branch(branch: &[[u8; 32]], leaf: &[u8; 32], path: Option<&[u8]>) -> [u8; 32] {
        if let Some(p) = path {
            assert!(p.len() >= branch.len().div_ceil(8), "path shorter than the branch");
        }
        let mut out = [0u8; 32];
        let path = path.map_or(std::ptr::null(), |p| p.as_ptr() as *const c_void);
        // SAFETY: `branch` has `depth` entries, `path` is NULL or long enough, 32-byte leaf/out.
        unsafe {
            ffi::tree_hash_from_branch(
                branch.as_ptr(),
                branch.len(),
                leaf.as_ptr() as *const c_char,
                path,
                out.as_mut_ptr() as *mut c_char,
            )
        };
        out
    }

    /// `argon2id_hash_raw` with a 32-byte tag.
    pub fn argon2id(t_cost: u32, m_cost_kib: u32, parallelism: u32, pwd: &[u8], salt: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: pointers and lengths describe live slices; the tag is 32 bytes.
        let rc = unsafe {
            ffi::argon2id_hash_raw(
                t_cost,
                m_cost_kib,
                parallelism,
                pwd.as_ptr() as *const c_void,
                pwd.len(),
                salt.as_ptr() as *const c_void,
                salt.len(),
                out.as_mut_ptr() as *mut c_void,
                32,
            )
        };
        assert_eq!(rc, 0, "argon2id_hash_raw failed: {rc}");
        out
    }

    /// Frees this thread's C scratchpad (`slow_hash_release_state`).
    pub fn release_scratchpad() {
        // SAFETY: no arguments; frees only this thread's scratchpad.
        unsafe { ffi::wrkz_release_scratchpad() };
    }
}
