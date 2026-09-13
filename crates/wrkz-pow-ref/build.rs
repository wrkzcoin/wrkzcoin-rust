// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Compiles the vendored reference C code (spec/02-hashing.md, "Keeping the C code").
//!
//! Every file under `c/` except `c/cn_shim.c` and `c/cn_pow_shim.c` is a
//! byte-for-byte copy of `src/crypto/*`, `src/common/int-util.h` and
//! `external/argon2` from wrkzcoin commit 8d89d7bf, verified with `diff` against
//! a checkout. Nothing vendored is modified; any change must be proven against
//! `spec/vectors/` first.
//!
//! Two sets of files, and neither is compiled unless a feature asks for it:
//!
//!   * feature `curve` (on by default): Keccak (`keccak.c`, `hash.c`) and the
//!     ref10 curve code (`crypto-ops.c`, `crypto-ops-data.c`) with
//!     `c/cn_shim.c`, the byte-oriented port of `src/crypto/crypto.cpp` (which
//!     is C++, so it cannot be vendored), function by function with the line
//!     cited. `wrkz_pow::curve` calls these in production until the curve is
//!     ported. Turning the feature off compiles no C at all, which is what
//!     removes the C compiler from the build the day the port lands.
//!   * feature `pow` (implies `curve`): the CryptoNight family and its
//!     finalizers, `tree-hash.c`, argon2 and `c/cn_pow_shim.c` (a
//!     `pthread_key`/`FlsAlloc` destructor replacing upstream's C++
//!     `slow-hash-state.cpp`, which frees the per-thread CryptoNight
//!     scratchpad on thread exit). `wrkz-pow` has all of this in Rust; only
//!     its differential tests and the fuzz targets build it.

use std::env;
use std::path::PathBuf;

fn main() {
    let c = PathBuf::from("c");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let pow = env::var_os("CARGO_FEATURE_POW").is_some();
    // `pow` implies `curve` in Cargo.toml, so this covers both sets.
    let curve = env::var_os("CARGO_FEATURE_CURVE").is_some();

    if !curve {
        // No C in this build. Say what the sources are so a later `cargo build`
        // that turns the feature back on still reruns.
        println!("cargo:rerun-if-changed=c");
        println!("cargo:rerun-if-changed=compat");
        return;
    }

    // ---- Keccak + curve ops, and with `pow` the CryptoNote hashing ----------
    // Match the flags the C++ daemon is built with (CMakeLists.txt:395:
    // `-O2 -DNDEBUG -fno-strict-aliasing`). slow-hash-x86.c reads its byte
    // buffers through `uint64_t *` (the U64 macro, line 57), which is undefined
    // under strict aliasing. GCC on Ubuntu at -O3 produced a wrong cn_slow_hash_v0
    // for one vector (2026-09-09; MinGW at -O3 did not), so the reference flags
    // are used unconditionally: consensus code is compiled the way the C++ is.
    let mut cn = cc::Build::new();
    cn.include(&c).warnings(false).opt_level(2).define("NDEBUG", None).files([
        c.join("keccak.c"),
        c.join("hash.c"),
        c.join("crypto-ops.c"),
        c.join("crypto-ops-data.c"),
        c.join("cn_shim.c"),
    ]);
    if pow {
        cn.files([
            c.join("tree-hash.c"),
            c.join("aesb.c"),
            c.join("oaes_lib.c"),
            c.join("hash-extra-blake.c"),
            c.join("hash-extra-groestl.c"),
            c.join("hash-extra-jh.c"),
            c.join("hash-extra-skein.c"),
            c.join("blake256.c"),
            c.join("groestl.c"),
            c.join("jh.c"),
            c.join("skein.c"),
            c.join("cn_pow_shim.c"),
        ]);
        // The three slow-hash files select themselves by preprocessor guards
        // (slow-hash-x86.c:11, slow-hash-arm.c:11, slow-hash-portable.c:12); the
        // guards are mutually exclusive and exhaustive, so compiling all three
        // yields exactly one definition of cn_slow_hash.
        cn.file(c.join("slow-hash-x86.c")).file(c.join("slow-hash-arm.c")).file(c.join("slow-hash-portable.c"));
    }

    if target_arch == "wasm32" {
        // A browser (the Rust Pluton Wallet web build). wasm32-unknown-unknown
        // has no C library, so compat/wasm32 declares the few libc functions
        // the curve and Keccak C use: memcpy and memset come from Rust's
        // compiler-builtins, malloc and free from `src/lib.rs`. Only the
        // always-built set is supported; `pow` is a test oracle.
        assert!(!pow, "the `pow` feature is not built for wasm32");
        cn.include("compat/wasm32");
    }

    let x86_64 = target_arch == "x86_64";
    if target_env != "msvc" {
        cn.flag("-fno-strict-aliasing");
    }
    if pow && x86_64 && target_env != "msvc" {
        // AES-NI + SSE2 intrinsics used by slow-hash-x86.c under GCC/Clang.
        // Every use is behind the runtime check_aes_hw()/force_software_aes()
        // dispatch (slow-hash-x86.c:576), so this cannot fault on an older CPU.
        cn.flag("-maes").flag("-msse2");
    }
    if pow && !x86_64 {
        // Without this, slow-hash-portable.c and both paths of slow-hash-arm.c
        // put the whole scratchpad on the stack (a 2 MiB array for
        // cn_slow_hash_v0), which overflows a spawned Rust thread's 2 MiB stack,
        // and MSVC cannot compile the VLA at all. Where the bytes live does not
        // change the hash. (slow-hash-x86.c manages its own heap scratchpad.)
        cn.define("FORCE_USE_HEAP", None);
    }
    if pow && env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default() == "apple" {
        // oaes_lib.c includes <sys/timeb.h>, which the macOS headers zig
        // carries lack (a macOS build from Linux, docs/CROSS-COMPILE.md).
        // Not vendored code: see compat/apple/sys/timeb.h for why it is safe.
        cn.include("compat/apple");
    }
    if target_env == "msvc" {
        cn.define("_CRT_SECURE_NO_WARNINGS", None);
        // hash-ops.h:28 uses C11 static_assert; MSVC only defines it in C11 mode.
        cn.std("c11");
    }
    cn.compile("wrkz_cn");
    if pow && env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        // SetLockPagesPrivilege in slow-hash-x86.c (large pages for the scratchpad).
        println!("cargo:rustc-link-lib=advapi32");
    }

    // ---- argon2 (Chukwa, block major 6) -------------------------------------
    // The generic (non-SIMD) reference path: it is only an oracle here.
    if pow {
        let a2 = c.join("argon2");
        let mut argon = cc::Build::new();
        argon
            .include(a2.join("include"))
            .include(a2.join("lib"))
            .define("A2_VISCTL", None)
            .warnings(false)
            .opt_level(3)
            .files([
                a2.join("lib/argon2.c"),
                a2.join("lib/core.c"),
                a2.join("lib/encoding.c"),
                a2.join("lib/genkat.c"),
                a2.join("lib/impl-select.c"),
                a2.join("lib/thread.c"),
                a2.join("lib/blake2/blake2.c"),
                a2.join("arch/generic/lib/argon2-arch.c"),
            ]);
        argon.compile("wrkz_argon2");
    }

    println!("cargo:rerun-if-changed=c");
    println!("cargo:rerun-if-changed=compat");
}
