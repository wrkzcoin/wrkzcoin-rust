# Licence

WrkzCoin (Rust) is licensed as the C++ WrkzCoin is: under the GNU General Public License, version 3 or later, keeping the notices of the projects the code grew from.

Those are the CryptoNote developers and the Bytecoin developers (LGPL-3.0), the Monero Project (BSD-3-Clause) and the TurtleCoin developers (GPL-3.0). [`LICENSE`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/LICENSE) is the C++ repository's own file, unchanged, and every release archive carries it. Every crate declares `GPL-3.0-or-later`.

## Vendored C

[`crates/wrkz-pow-ref/c/`](https://github.com/wrkzcoin/wrkzcoin-rust/tree/development/crates/wrkz-pow-ref/c) is C code copied unchanged from the C++ repository, under the same notices, each file's header and [`c/LICENSE`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/crates/wrkz-pow-ref/c/LICENSE); its argon2 is MIT ([`crates/wrkz-pow-ref/c/argon2/LICENSE`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/crates/wrkz-pow-ref/c/argon2/LICENSE)).

Two files there are not vendored and are this port's own, under this repository's licence: `cn_shim.c`, a byte-oriented C port of the C++ `src/crypto/crypto.cpp`, and `cn_pow_shim.c`, which replaces upstream's C++ `src/crypto/slow-hash-state.cpp`. [`crates/wrkz-pow-ref/compat/`](https://github.com/wrkzcoin/wrkzcoin-rust/tree/development/crates/wrkz-pow-ref/compat) is ours too.

## Rust Pluton Wallet

Rust Pluton Wallet draws with [Slint](https://slint.dev), used under Slint's GPL-3.0 licence, so a Pluton build is distributed under GPL-3.0; its About page credits Slint.

## Dependencies

The licences of the dependencies are checked against an allow list by the audit in [`deny.toml`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/deny.toml), which CI runs on every push and every week ([Testing](contributing/testing.md#dependency-audit)).
