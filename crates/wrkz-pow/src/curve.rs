// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! ed25519 operations exactly as `src/crypto/crypto.cpp` performs them
//! (`spec/03-crypto-primitives.md`), through the C shim `c/cn_shim.c` of
//! `wrkz-pow-ref` over the vendored ref10 code. This is the one part of the
//! crate that is still C; its port is gated on the dual run (spec/12, stage 3 step 6).
//!
//! All values are 32-byte little-endian encodings. Functions that take
//! randomness take it explicitly so that the signing paths are deterministic
//! and testable; [`random_scalar`] draws from the platform CSPRNG.

pub type Scalar = [u8; 32];
pub type Point = [u8; 32];
pub type PublicKey = [u8; 32];
pub type SecretKey = [u8; 32];
pub type KeyDerivation = [u8; 32];
pub type KeyImage = [u8; 32];
pub type Signature = [u8; 64];

/// The group order `l`, little-endian, as used by the key image domain check.
pub const L: Scalar = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
];

/// Encoding of the identity point.
pub const IDENTITY: Point = {
    let mut p = [0u8; 32];
    p[0] = 1;
    p
};

/// `NULL_SECRET_KEY`: what a view-only wallet stores as its spend key.
pub const NULL_SECRET_KEY: SecretKey = [0u8; 32];

use wrkz_pow_ref::curve as ffi;

/// `Hs(data)` = `sc_reduce32(keccak(data))`.
pub fn hash_to_scalar(data: &[u8]) -> Scalar {
    let mut out = [0u8; 32];
    // SAFETY: C reads `data.len()` bytes and writes 32 bytes.
    unsafe { ffi::wrkz_hash_to_scalar(data.as_ptr(), data.len(), out.as_mut_ptr()) };
    out
}

pub fn sc_reduce32(mut s: Scalar) -> Scalar {
    // SAFETY: in-place on a 32-byte buffer.
    unsafe { ffi::wrkz_sc_reduce32(s.as_mut_ptr()) };
    s
}

/// `random_scalar` given 64 bytes of entropy: `sc_reduce` of the 64-byte value.
pub fn scalar_from_64_bytes(rnd: &[u8; 64]) -> Scalar {
    let mut out = [0u8; 32];
    // SAFETY: reads 64 bytes, writes 32.
    unsafe { ffi::wrkz_scalar_from_64_bytes(rnd.as_ptr(), out.as_mut_ptr()) };
    out
}

/// A random scalar from the platform CSPRNG (`Random::randomBytes`, `spec/03`, "Randomness").
pub fn random_scalar() -> Scalar {
    let mut rnd = [0u8; 64];
    getrandom::fill(&mut rnd).expect("platform CSPRNG");
    scalar_from_64_bytes(&rnd)
}

/// True iff `s < l` (a canonical scalar).
pub fn sc_check(s: &Scalar) -> bool {
    // SAFETY: reads 32 bytes.
    unsafe { ffi::wrkz_sc_check(s.as_ptr()) == 0 }
}

pub fn sc_add(a: &Scalar, b: &Scalar) -> Scalar {
    let mut r = [0u8; 32];
    // SAFETY: all three are 32-byte buffers.
    unsafe { ffi::wrkz_sc_add(r.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
    r
}

pub fn sc_sub(a: &Scalar, b: &Scalar) -> Scalar {
    let mut r = [0u8; 32];
    // SAFETY: all three are 32-byte buffers.
    unsafe { ffi::wrkz_sc_sub(r.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
    r
}

pub fn sc_mul(a: &Scalar, b: &Scalar) -> Scalar {
    let mut r = [0u8; 32];
    // SAFETY: all three are 32-byte buffers.
    unsafe { ffi::wrkz_sc_mul(r.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
    r
}

/// `c - a*b mod l`.
pub fn sc_mulsub(a: &Scalar, b: &Scalar, c: &Scalar) -> Scalar {
    let mut r = [0u8; 32];
    // SAFETY: all four are 32-byte buffers.
    unsafe { ffi::wrkz_sc_mulsub(r.as_mut_ptr(), a.as_ptr(), b.as_ptr(), c.as_ptr()) };
    r
}

/// `check_key`: the encoding decompresses to a curve point (no subgroup check).
pub fn check_key(pk: &PublicKey) -> bool {
    // SAFETY: reads 32 bytes.
    unsafe { ffi::wrkz_check_key(pk.as_ptr()) != 0 }
}

/// `sec * G`; `None` if `sec` is not a canonical scalar.
pub fn secret_key_to_public_key(sec: &SecretKey) -> Option<PublicKey> {
    let mut out = [0u8; 32];
    // SAFETY: reads 32 bytes, writes 32 bytes.
    let ok = unsafe { ffi::wrkz_secret_key_to_public_key(sec.as_ptr(), out.as_mut_ptr()) };
    (ok != 0).then_some(out)
}

/// `generate_deterministic_keys`: `sec = sc_reduce32(seed)`, `pub = sec*G`.
pub fn generate_deterministic_keys(seed: &[u8; 32]) -> (SecretKey, PublicKey) {
    let mut sec = [0u8; 32];
    let mut pk = [0u8; 32];
    // SAFETY: all 32-byte buffers.
    unsafe { ffi::wrkz_generate_deterministic_keys(seed.as_ptr(), sec.as_mut_ptr(), pk.as_mut_ptr()) };
    (sec, pk)
}

/// `generate_keys`: a fresh random key pair.
pub fn generate_keys() -> (SecretKey, PublicKey) {
    let sec = random_scalar();
    let pk = secret_key_to_public_key(&sec).expect("reduced scalar");
    (sec, pk)
}

/// `generateViewFromSpend`: `view_sec = sc_reduce32(keccak(spend_sec))`.
pub fn generate_view_from_spend(spend: &SecretKey) -> (SecretKey, PublicKey) {
    let mut sec = [0u8; 32];
    let mut pk = [0u8; 32];
    // SAFETY: all 32-byte buffers.
    unsafe { ffi::wrkz_generate_view_from_spend(spend.as_ptr(), sec.as_mut_ptr(), pk.as_mut_ptr()) };
    (sec, pk)
}

/// `generate_deterministic_subwallet_key`; index 0 is the primary wallet itself.
pub fn generate_deterministic_subwallet_key(base_spend: &SecretKey, index: u64) -> SecretKey {
    let mut out = [0u8; 32];
    // SAFETY: 32-byte buffers.
    unsafe { ffi::wrkz_generate_deterministic_subwallet_key(base_spend.as_ptr(), index, out.as_mut_ptr()) };
    out
}

/// `D = 8 * sec * pub`; `None` if `pub` does not decompress.
pub fn generate_key_derivation(pk: &PublicKey, sec: &SecretKey) -> Option<KeyDerivation> {
    let mut out = [0u8; 32];
    // SAFETY: 32-byte buffers.
    let ok = unsafe { ffi::wrkz_generate_key_derivation(pk.as_ptr(), sec.as_ptr(), out.as_mut_ptr()) };
    (ok != 0).then_some(out)
}

/// `Hs(D || varint(index))`.
pub fn derivation_to_scalar(d: &KeyDerivation, output_index: u64) -> Scalar {
    let mut out = [0u8; 32];
    // SAFETY: 32-byte buffers.
    unsafe { ffi::wrkz_derivation_to_scalar(d.as_ptr(), output_index, out.as_mut_ptr()) };
    out
}

/// `P = Hs(D||i)*G + B`.
pub fn derive_public_key(d: &KeyDerivation, output_index: u64, base: &PublicKey) -> Option<PublicKey> {
    let mut out = [0u8; 32];
    // SAFETY: 32-byte buffers.
    let ok = unsafe { ffi::wrkz_derive_public_key(d.as_ptr(), output_index, base.as_ptr(), out.as_mut_ptr()) };
    (ok != 0).then_some(out)
}

/// `x = Hs(D||i) + b`.
pub fn derive_secret_key(d: &KeyDerivation, output_index: u64, base: &SecretKey) -> SecretKey {
    let mut out = [0u8; 32];
    // SAFETY: 32-byte buffers.
    unsafe { ffi::wrkz_derive_secret_key(d.as_ptr(), output_index, base.as_ptr(), out.as_mut_ptr()) };
    out
}

/// `B = P - Hs(D||i)*G`: the spend public key an output was sent to.
pub fn underive_public_key(d: &KeyDerivation, output_index: u64, derived: &PublicKey) -> Option<PublicKey> {
    let mut out = [0u8; 32];
    // SAFETY: 32-byte buffers.
    let ok = unsafe { ffi::wrkz_underive_public_key(d.as_ptr(), output_index, derived.as_ptr(), out.as_mut_ptr()) };
    (ok != 0).then_some(out)
}

/// `Hp(data) = 8 * fromfe(keccak(data))`.
pub fn hash_to_ec(data: &[u8]) -> Point {
    let mut out = [0u8; 32];
    // SAFETY: reads `data.len()` bytes, writes 32.
    unsafe { ffi::wrkz_hash_data_to_ec(data.as_ptr(), data.len(), out.as_mut_ptr()) };
    out
}

/// `I = x * Hp(P)`.
pub fn generate_key_image(pk: &PublicKey, sec: &SecretKey) -> KeyImage {
    let mut out = [0u8; 32];
    // SAFETY: 32-byte buffers.
    unsafe { ffi::wrkz_generate_key_image(pk.as_ptr(), sec.as_ptr(), out.as_mut_ptr()) };
    out
}

/// `scalarmultKey(P, a) = a*P` with no canonicality check on the scalar
/// (`crypto.cpp:429`). `None` when `P` does not decompress: the C++ ignores
/// that failure and multiplies an uninitialised point, which this port refuses
/// to reproduce (`c/cn_shim.c`, `wrkz_scalarmult_key`).
pub fn scalarmult_key(p: &Point, a: &Scalar) -> Option<Point> {
    let mut out = [0u8; 32];
    // SAFETY: 32-byte buffers; C reports whether it wrote a real point.
    let ok = unsafe { ffi::wrkz_scalarmult_key(p.as_ptr(), a.as_ptr(), out.as_mut_ptr()) };
    (ok != 0).then_some(out)
}

/// The consensus key image domain check (`ValidateTransaction.cpp:260`):
/// `l * I` must be the identity. A key image that does not decompress fails the
/// check, which is what the C++ arrives at as well (an uninitialised point does
/// not encode the identity), only here it is deterministic.
pub fn key_image_in_prime_subgroup(image: &KeyImage) -> bool {
    scalarmult_key(image, &L) == Some(IDENTITY)
}

/// Schnorr-style signature over `(prefix_hash, pub, k*G)` with nonce `k`.
pub fn generate_signature_with_nonce(prefix_hash: &[u8; 32], pk: &PublicKey, sec: &SecretKey, k: &Scalar) -> Signature {
    let mut sig = [0u8; 64];
    // SAFETY: 32-byte inputs, 64-byte output.
    unsafe {
        ffi::wrkz_generate_signature(prefix_hash.as_ptr(), pk.as_ptr(), sec.as_ptr(), k.as_ptr(), sig.as_mut_ptr())
    };
    sig
}

pub fn generate_signature(prefix_hash: &[u8; 32], pk: &PublicKey, sec: &SecretKey) -> Signature {
    generate_signature_with_nonce(prefix_hash, pk, sec, &random_scalar())
}

pub fn check_signature(prefix_hash: &[u8; 32], pk: &PublicKey, sig: &Signature) -> bool {
    // SAFETY: 32-byte inputs and a 64-byte signature.
    unsafe { ffi::wrkz_check_signature(prefix_hash.as_ptr(), pk.as_ptr(), sig.as_ptr()) != 0 }
}

/// `generateRingSignatures` with explicit randomness: `k` is the signer's
/// nonce and `decoys[i]` the `(c_i, r_i)` scalars for every `i != real`
/// (ignored at `real`). Returns `None` if the image or a ring member does not
/// decompress or `real` is out of range.
pub fn generate_ring_signature_with_randomness(
    prefix_hash: &[u8; 32],
    image: &KeyImage,
    pubs: &[PublicKey],
    sec: &SecretKey,
    real: usize,
    k: &Scalar,
    decoys: &[(Scalar, Scalar)],
) -> Option<Vec<Signature>> {
    if pubs.is_empty() || real >= pubs.len() || decoys.len() != pubs.len() {
        return None;
    }
    let mut sigs = vec![[0u8; 64]; pubs.len()];
    for (i, (c, r)) in decoys.iter().enumerate() {
        sigs[i][..32].copy_from_slice(c);
        sigs[i][32..].copy_from_slice(r);
    }
    let flat: Vec<u8> = pubs.iter().flatten().copied().collect();
    // SAFETY: `flat` holds 32*n bytes, `sigs` 64*n bytes, `real < n`.
    let ok = unsafe {
        ffi::wrkz_generate_ring_signature(
            prefix_hash.as_ptr(),
            image.as_ptr(),
            flat.as_ptr(),
            pubs.len(),
            sec.as_ptr(),
            real as u64,
            k.as_ptr(),
            sigs.as_mut_ptr() as *mut u8,
        )
    };
    (ok != 0).then_some(sigs)
}

/// `generateRingSignatures` with fresh CSPRNG randomness.
pub fn generate_ring_signature(
    prefix_hash: &[u8; 32],
    image: &KeyImage,
    pubs: &[PublicKey],
    sec: &SecretKey,
    real: usize,
) -> Option<Vec<Signature>> {
    let k = random_scalar();
    let decoys: Vec<(Scalar, Scalar)> = pubs.iter().map(|_| (random_scalar(), random_scalar())).collect();
    generate_ring_signature_with_randomness(prefix_hash, image, pubs, sec, real, &k, &decoys)
}

/// `checkRingSignature` (`crypto.cpp:631`), including the subgroup check on the image.
pub fn check_ring_signature(prefix_hash: &[u8; 32], image: &KeyImage, pubs: &[PublicKey], sigs: &[Signature]) -> bool {
    if pubs.len() != sigs.len() {
        return false;
    }
    let flat: Vec<u8> = pubs.iter().flatten().copied().collect();
    // SAFETY: `flat` holds 32*n bytes and `sigs` 64*n bytes.
    unsafe {
        ffi::wrkz_check_ring_signature(
            prefix_hash.as_ptr(),
            image.as_ptr(),
            flat.as_ptr(),
            pubs.len(),
            sigs.as_ptr() as *const u8,
        ) != 0
    }
}
