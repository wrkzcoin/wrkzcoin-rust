// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Wallet file cipher (`src/crypto/WalletCrypto.{h,cpp}`, spec/03 "Wallet file
//! encryption", spec/10 "Wallet file").
//!
//! PBKDF2-HMAC-SHA256 to a 16-byte key, AES-128-CBC with PKCS#7 padding, and
//! the salt reused as the IV. These are fixed by the on-disk format.
//!
//! Everything derived from a password — the key and any decrypted plaintext,
//! which for a wallet file holds the private spend and view keys — is returned
//! in a [`Zeroizing`] wrapper so it is wiped when the caller drops it.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::Hmac;
use sha2::Sha256;
use wrkz_primitives::constants::{PBKDF2_ITERATIONS, WALLET_API_PBKDF2_ITERATIONS};
use zeroize::Zeroizing;

pub const KEY_SIZE: usize = 16;
pub const SALT_SIZE: usize = 16;

type Enc = cbc::Encryptor<aes::Aes128>;
type Dec = cbc::Decryptor<aes::Aes128>;

/// `WalletCrypto::deriveKey`.
///
/// # Panics
///
/// On `iterations == 0`, which would reduce the derivation to a single HMAC.
/// `pbkdf2` itself accepts it silently, so the check has to live here; both
/// spec'd wrappers below pass a constant.
pub fn derive_key(password: &[u8], salt: &[u8], iterations: u32, key_length: usize) -> Zeroizing<Vec<u8>> {
    assert!(iterations > 0, "pbkdf2 iterations must be positive");
    let mut key = Zeroizing::new(vec![0u8; key_length]);
    pbkdf2::pbkdf2::<Hmac<Sha256>>(password, salt, iterations, &mut key).expect("hmac accepts any key length");
    key
}

/// The wallet file key: 500,000 iterations, 16 bytes.
pub fn wallet_file_key(password: &[u8], salt: &[u8; SALT_SIZE]) -> Zeroizing<[u8; KEY_SIZE]> {
    let derived = derive_key(password, salt, PBKDF2_ITERATIONS, KEY_SIZE);
    let mut key = Zeroizing::new([0u8; KEY_SIZE]);
    key.copy_from_slice(&derived);
    key
}

/// The wallet API password hash: 10,000 iterations, 16 bytes. Not interchangeable with the file key.
pub fn api_password_hash(password: &[u8], salt: &[u8; SALT_SIZE]) -> Zeroizing<[u8; KEY_SIZE]> {
    let derived = derive_key(password, salt, WALLET_API_PBKDF2_ITERATIONS, KEY_SIZE);
    let mut hash = Zeroizing::new([0u8; KEY_SIZE]);
    hash.copy_from_slice(&derived);
    hash
}

/// `WalletCrypto::encrypt`: AES-128-CBC, PKCS#7.
///
/// The on-disk format always uses `iv = salt` — prefer
/// [`encrypt_wallet_file`], which cannot be called with any other IV.
pub fn encrypt(plaintext: &[u8], key: &[u8; KEY_SIZE], iv: &[u8; KEY_SIZE]) -> Vec<u8> {
    Enc::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

/// `WalletCrypto::decrypt`. Returns `None` for any failure (wrong length or
/// padding); callers MUST report only "wrong password" (no padding oracle).
///
/// Prefer [`decrypt_wallet_file`], which cannot be called with the wrong IV.
pub fn decrypt(ciphertext: &[u8], key: &[u8; KEY_SIZE], iv: &[u8; KEY_SIZE]) -> Option<Zeroizing<Vec<u8>>> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return None;
    }
    Dec::new(key.into(), iv.into()).decrypt_padded_vec_mut::<Pkcs7>(ciphertext).ok().map(Zeroizing::new)
}

/// Encrypt with the wallet-file rules: key = PBKDF2(password, salt, 500,000),
/// iv = salt (spec/03 "Wallet file encryption").
pub fn encrypt_wallet_file(plaintext: &[u8], password: &[u8], salt: &[u8; SALT_SIZE]) -> Vec<u8> {
    encrypt(plaintext, &wallet_file_key(password, salt), salt)
}

/// Decrypt a wallet file body. `None` means "wrong password" and nothing more
/// specific — see [`decrypt`].
pub fn decrypt_wallet_file(ciphertext: &[u8], password: &[u8], salt: &[u8; SALT_SIZE]) -> Option<Zeroizing<Vec<u8>>> {
    decrypt(ciphertext, &wallet_file_key(password, salt), salt)
}

/// Fresh 16 random salt bytes from the platform CSPRNG.
pub fn random_salt() -> [u8; SALT_SIZE] {
    let mut s = [0u8; SALT_SIZE];
    getrandom::fill(&mut s).expect("platform CSPRNG");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salt() -> [u8; 16] {
        let mut s = [0u8; 16];
        for (i, b) in s.iter_mut().enumerate() {
            *b = i as u8;
        }
        s
    }

    #[test]
    fn spec_vectors() {
        // spec/03 "Wallet file encryption" vectors (primitives.txt)
        let key = wallet_file_key(b"password", &salt());
        assert_eq!(hex::encode(*key), "c21c37b6728f17a31765f45fecd1e583");
        assert_eq!(hex::encode(*api_password_hash(b"password", &salt())), "eb6c81535592203c092b158f8d390967");
        let ct = encrypt(b"hello wallet", &key, &salt());
        assert_eq!(hex::encode(&ct), "7f97edf047d16b130baebd5d15e88ac3");
        assert_eq!(decrypt(&ct, &key, &salt()).as_deref().map(|v| &v[..]), Some(&b"hello wallet"[..]));
        let mut bad_key = key.clone();
        bad_key[0] ^= 1;
        assert!(decrypt(&ct, &bad_key, &salt()).is_none());
        assert!(decrypt(&ct[..15], &key, &salt()).is_none());
        assert!(decrypt(&[], &key, &salt()).is_none());
    }

    #[test]
    fn wallet_file_wrappers_pin_the_iv_to_the_salt() {
        let ct = encrypt_wallet_file(b"hello wallet", b"password", &salt());
        assert_eq!(hex::encode(&ct), "7f97edf047d16b130baebd5d15e88ac3");
        assert_eq!(
            decrypt_wallet_file(&ct, b"password", &salt()).as_deref().map(|v| &v[..]),
            Some(&b"hello wallet"[..])
        );
        assert!(decrypt_wallet_file(&ct, b"wrong", &salt()).is_none());
    }

    #[test]
    #[should_panic(expected = "pbkdf2 iterations must be positive")]
    fn zero_iterations_is_rejected() {
        let _ = derive_key(b"password", &salt(), 0, KEY_SIZE);
    }
}
