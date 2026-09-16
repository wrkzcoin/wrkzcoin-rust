// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The wallet container's JSON schema (`wrkz_wallet::file`): the bytes inside
//! the cipher, which decide what keys, subwallets, inputs and transactions a
//! wallet holds. A file is something a user can be talked into opening, so
//! this parser is as exposed as the ones that read a socket.
//!
//! The property is total parsing plus **idempotence**: a document that opens
//! must write back to a document that opens to the same bytes. That is what
//! makes "files written here open in the C++ wallet and re-export
//! byte-identically" a property and not an anecdote.
//!
//! The encrypted container around it (`decode_wallet_file`) is deliberately
//! not fuzzed: every input past its magic identifier costs 500,000 PBKDF2
//! iterations (spec/03), so a campaign would spend all of its time in the key
//! derivation and none in the parser. Its framing — identifier, salt, the
//! password check — is covered by `tests/wallet_file.rs`.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_wallet::file::Wallet;

fuzz_target!(|data: &[u8]| {
    let Ok(wallet) = Wallet::from_json_bytes(data) else { return };
    let Ok(written) = wallet.to_json_bytes() else { return };
    let reopened = Wallet::from_json_bytes(&written).expect("what we wrote must open");
    let again = reopened.to_json_bytes().expect("and must write back");
    assert_eq!(&written[..], &again[..], "the wallet document did not round-trip");
});
