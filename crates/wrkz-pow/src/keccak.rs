// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Keccak as CryptoNote uses it (`keccak.c`, `hash.c`): Keccak-f\[1600\] with
//! 24 rounds, a 136-byte rate and the original `0x01 .. 0x80` padding. This is
//! the pre-standard Keccak, not SHA3-256, whose padding byte is `0x06`.

use crate::Hash;

/// The 200-byte `union hash_state`.
pub(crate) type State = [u8; 200];

const RATE: usize = 136;

fn permute(st: &mut [u64; 25]) {
    keccak::Keccak::new().with_f1600(|f| f(st));
}

fn xor_block(st: &mut [u64; 25], block: &[u8]) {
    for (w, bytes) in st.iter_mut().zip(block.as_chunks::<8>().0) {
        *w ^= u64::from_le_bytes(*bytes);
    }
}

/// `keccak(in, inlen, md, 200)` (`keccak.c:76`) up to the final permutation.
fn absorb(data: &[u8]) -> [u64; 25] {
    let mut st = [0u64; 25];
    let (blocks, rest) = data.as_chunks::<RATE>();
    for block in blocks {
        xor_block(&mut st, block);
        permute(&mut st);
    }
    // A full padding block when the input is a whole number of blocks, as the C.
    let mut last = [0u8; RATE];
    last[..rest.len()].copy_from_slice(rest);
    last[rest.len()] = 0x01;
    last[RATE - 1] |= 0x80;
    xor_block(&mut st, &last);
    permute(&mut st);
    st
}

fn to_bytes(st: &[u64; 25]) -> State {
    let mut out = [0u8; 200];
    for (bytes, w) in out.as_chunks_mut::<8>().0.iter_mut().zip(st) {
        *bytes = w.to_le_bytes();
    }
    out
}

/// `hash_process` / `keccak1600`: the whole state after absorbing `data`.
pub(crate) fn keccak1600(data: &[u8]) -> State {
    to_bytes(&absorb(data))
}

/// `hash_permutation` (`hash.c`): `keccakf(state, 24)` in place.
pub(crate) fn keccakf(state: &mut State) {
    let mut st = [0u64; 25];
    for (w, bytes) in st.iter_mut().zip(state.as_chunks::<8>().0) {
        *w = u64::from_le_bytes(*bytes);
    }
    permute(&mut st);
    *state = to_bytes(&st);
}

/// `cn_fast_hash`: the first 32 bytes of the state.
pub(crate) fn cn_fast_hash(data: &[u8]) -> Hash {
    let st = absorb(data);
    let mut out = [0u8; 32];
    for (bytes, w) in out.as_chunks_mut::<8>().0.iter_mut().zip(&st) {
        *bytes = w.to_le_bytes();
    }
    out
}
