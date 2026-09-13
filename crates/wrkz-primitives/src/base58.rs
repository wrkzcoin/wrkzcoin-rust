// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! CryptoNote block base58 and address encoding (spec/05; `src/common/Base58.cpp`,
//! `src/utilities/Addresses.cpp`).

use crate::constants::{
    CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX, INTEGRATED_ADDRESS_LENGTH, INTEGRATED_ADDRESS_LENGTH_LONG,
    STANDARD_ADDRESS_LENGTH,
};
use crate::varint;
use wrkz_pow::cn_fast_hash;

pub const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const FULL_BLOCK_SIZE: usize = 8;
const FULL_ENCODED_BLOCK_SIZE: usize = 11;
/// Encoded characters for a partial block of `n` bytes.
const ENCODED_BLOCK_SIZES: [usize; 9] = [0, 2, 3, 5, 6, 7, 9, 10, 11];
const ADDR_CHECKSUM_SIZE: usize = 4;

/// Why a base58 string or an address failed to decode.
///
/// The C++ `decode_addr` / `parseAccountAddressString` pair returns `bool` and
/// pushes the reason into a log line, so nothing here is consensus-visible; a
/// wallet needs the distinction to tell a user which character to fix. No other
/// crate in the workspace calls these functions, so the whole set moved from
/// `Option` to `Result` at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Base58Error {
    /// A character outside the 58-character alphabet, at this byte offset.
    InvalidCharacter { position: usize, character: char },
    /// The encoded length is not one that any sequence of blocks can produce
    /// (the tail block must encode to 2, 3, 5, 6, 7, 9, 10 or 11 characters).
    InvalidLength(usize),
    /// A block decoded to a number too large for the bytes it stands for.
    BlockOverflow,
    /// Fewer bytes than the 4-byte checksum.
    TooShort,
    /// The 4-byte keccak checksum did not match the payload.
    BadChecksum,
    /// The leading varint tag was malformed.
    BadTag,
    /// The address tag is not this network's prefix.
    WrongPrefix(u64),
    /// The character count is not 98, 120 or 186.
    WrongAddressLength(usize),
    /// The payload is not `payment id ‖ 32-byte spend key ‖ 32-byte view key`.
    WrongPayloadLength(usize),
    /// The embedded payment id is not ASCII hex of the expected length.
    BadPaymentId,
    /// A key in the payload is not a valid curve point.
    BadPublicKey,
}

impl std::fmt::Display for Base58Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Base58Error::InvalidCharacter { position, character } => {
                write!(f, "character {character:?} at offset {position} is not in the base58 alphabet")
            }
            Base58Error::InvalidLength(n) => write!(f, "{n} characters is not a valid base58 length"),
            Base58Error::BlockOverflow => write!(f, "base58 block decodes to more than its byte width"),
            Base58Error::TooShort => write!(f, "shorter than the 4-byte checksum"),
            Base58Error::BadChecksum => write!(f, "checksum mismatch"),
            Base58Error::BadTag => write!(f, "malformed address tag varint"),
            Base58Error::WrongPrefix(t) => write!(f, "address prefix {t} is not this network"),
            Base58Error::WrongAddressLength(n) => write!(f, "{n} characters is not a standard or integrated address"),
            Base58Error::WrongPayloadLength(n) => write!(f, "{n} payload bytes do not match the address form"),
            Base58Error::BadPaymentId => write!(f, "payment id is not ASCII hex"),
            Base58Error::BadPublicKey => write!(f, "public key is not a curve point"),
        }
    }
}

impl std::error::Error for Base58Error {}

fn reverse(c: u8) -> Option<u64> {
    ALPHABET.iter().position(|&a| a == c).map(|p| p as u64)
}

fn encode_block(block: &[u8], out: &mut [u8]) {
    let mut num: u64 = 0;
    for &b in block {
        num = (num << 8) | b as u64;
    }
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(num % 58) as usize];
        num /= 58;
    }
    // leading positions stay '1' (ALPHABET[0]) because num reaches 0
}

pub fn encode(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    let full = data.len() / FULL_BLOCK_SIZE;
    let last = data.len() % FULL_BLOCK_SIZE;
    let mut out = vec![b'1'; full * FULL_ENCODED_BLOCK_SIZE + ENCODED_BLOCK_SIZES[last]];
    for i in 0..full {
        encode_block(&data[i * 8..i * 8 + 8], &mut out[i * 11..i * 11 + 11]);
    }
    if last > 0 {
        encode_block(&data[full * 8..], &mut out[full * 11..]);
    }
    String::from_utf8(out).unwrap()
}

/// `offset` is where `block` starts in the whole string, so a bad character can
/// be reported at its real position.
fn decode_block(block: &[u8], res_size: usize, offset: usize) -> Result<Vec<u8>, Base58Error> {
    let mut num: u128 = 0;
    let mut order: u128 = 1;
    for (i, &c) in block.iter().enumerate().rev() {
        let d = reverse(c).ok_or(Base58Error::InvalidCharacter { position: offset + i, character: c as char })? as u128;
        num += order * d;
        if num > u64::MAX as u128 {
            return Err(Base58Error::BlockOverflow);
        }
        order *= 58;
    }
    if res_size < FULL_BLOCK_SIZE && (1u128 << (8 * res_size)) <= num {
        return Err(Base58Error::BlockOverflow);
    }
    let bytes = (num as u64).to_be_bytes();
    Ok(bytes[8 - res_size..].to_vec())
}

pub fn decode(s: &str) -> Result<Vec<u8>, Base58Error> {
    let s = s.as_bytes();
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let full = s.len() / FULL_ENCODED_BLOCK_SIZE;
    let last = s.len() % FULL_ENCODED_BLOCK_SIZE;
    let last_decoded = if last == 0 {
        0
    } else {
        ENCODED_BLOCK_SIZES.iter().position(|&n| n == last).ok_or(Base58Error::InvalidLength(s.len()))?
    };
    let mut out = Vec::with_capacity(full * 8 + last_decoded);
    for i in 0..full {
        out.extend(decode_block(&s[i * 11..i * 11 + 11], FULL_BLOCK_SIZE, i * 11)?);
    }
    if last > 0 {
        out.extend(decode_block(&s[full * 11..], last_decoded, full * 11)?);
    }
    Ok(out)
}

/// `encode_addr(tag, data)`: base58 of `varint(tag) ‖ data ‖ keccak(...)[0..4]`.
pub fn encode_addr(tag: u64, data: &[u8]) -> String {
    let mut buf = varint::encode(tag);
    buf.extend_from_slice(data);
    let h = cn_fast_hash(&buf);
    buf.extend_from_slice(&h[..ADDR_CHECKSUM_SIZE]);
    encode(&buf)
}

/// `decode_addr`: returns `(tag, data)` after checking the checksum.
pub fn decode_addr(addr: &str) -> Result<(u64, Vec<u8>), Base58Error> {
    let mut data = decode(addr)?;
    if data.len() <= ADDR_CHECKSUM_SIZE {
        return Err(Base58Error::TooShort);
    }
    let checksum = data.split_off(data.len() - ADDR_CHECKSUM_SIZE);
    let h = cn_fast_hash(&data);
    if h[..ADDR_CHECKSUM_SIZE] != checksum[..] {
        return Err(Base58Error::BadChecksum);
    }
    let (tag, n) = varint::read(&data).map_err(|_| Base58Error::BadTag)?;
    Ok((tag, data[n..].to_vec()))
}

/// A parsed standard or integrated address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub spend_public_key: [u8; 32],
    pub view_public_key: [u8; 32],
    /// The payment id as 16 or 64 hex characters for an integrated address.
    pub payment_id: Option<String>,
}

/// `getAccountAddressAsStr`: 98 characters starting with `Wrkz`.
pub fn standard_address(spend_public_key: &[u8; 32], view_public_key: &[u8; 32]) -> String {
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(spend_public_key);
    data.extend_from_slice(view_public_key);
    encode_addr(CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX, &data)
}

/// `createIntegratedAddress` (`Addresses.cpp:129`): the payment id is packed
/// as its **ASCII hex string** (16 or 64 characters), placed before the keys.
/// That is why the lengths are 120 and 186: `3 + 16 + 64 + 4 = 87` bytes and
/// `3 + 64 + 64 + 4 = 135` bytes. Returns `None` unless `payment_id_hex` is
/// 16 or 64 hex characters.
pub fn integrated_address(
    spend_public_key: &[u8; 32],
    view_public_key: &[u8; 32],
    payment_id_hex: &str,
) -> Result<String, Base58Error> {
    if !(payment_id_hex.len() == 16 || payment_id_hex.len() == 64)
        || !payment_id_hex.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(Base58Error::BadPaymentId);
    }
    let mut data = Vec::with_capacity(64 + payment_id_hex.len());
    data.extend_from_slice(payment_id_hex.as_bytes());
    data.extend_from_slice(spend_public_key);
    data.extend_from_slice(view_public_key);
    Ok(encode_addr(CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX, &data))
}

/// `isIntegratedAddress`: decided by length alone.
pub fn is_integrated_address(addr: &str) -> bool {
    addr.len() == INTEGRATED_ADDRESS_LENGTH || addr.len() == INTEGRATED_ADDRESS_LENGTH_LONG
}

/// Parse a standard (98) or integrated (120/186) address; checks the prefix
/// and that both keys decompress (`parseAccountAddressString`).
pub fn parse_address(addr: &str) -> Result<Address, Base58Error> {
    let (tag, data) = decode_addr(addr)?;
    if tag != CRYPTONOTE_PUBLIC_ADDRESS_BASE58_PREFIX {
        return Err(Base58Error::WrongPrefix(tag));
    }
    let pid_len = match addr.len() {
        STANDARD_ADDRESS_LENGTH => 0,
        INTEGRATED_ADDRESS_LENGTH => 16,
        INTEGRATED_ADDRESS_LENGTH_LONG => 64,
        n => return Err(Base58Error::WrongAddressLength(n)),
    };
    if data.len() != pid_len + 64 {
        return Err(Base58Error::WrongPayloadLength(data.len()));
    }
    let payment_id = if pid_len > 0 {
        let pid = std::str::from_utf8(&data[..pid_len]).map_err(|_| Base58Error::BadPaymentId)?;
        if !pid.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(Base58Error::BadPaymentId);
        }
        Some(pid.to_string())
    } else {
        None
    };
    let spend_public_key: [u8; 32] = data[pid_len..pid_len + 32].try_into().unwrap();
    let view_public_key: [u8; 32] = data[pid_len + 32..].try_into().unwrap();
    if !wrkz_pow::curve::check_key(&spend_public_key) || !wrkz_pow::curve::check_key(&view_public_key) {
        return Err(Base58Error::BadPublicKey);
    }
    Ok(Address { spend_public_key, view_public_key, payment_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58_vectors() {
        assert_eq!(encode(&[0, 0, 0, 0]), "111111");
        assert_eq!(encode(b"a"), "2g");
        assert_eq!(encode(b"hello"), "Cn8eVZg");
        assert_eq!(encode(b"hello world!"), "JTmsyNwG6XQ3vdzkp");
        assert_eq!(encode(&[0, 0, 0, 1]), "111112");
        for s in [&b"hello world!"[..], &[0, 0, 0, 1], &[0xff; 8], &[0xff; 9], &[]] {
            assert_eq!(decode(&encode(s)).unwrap(), s);
        }
        // the length is checked before any character: 1 is not an encoded block size
        assert_eq!(decode("0"), Err(Base58Error::InvalidLength(1)));
        assert_eq!(decode("0g"), Err(Base58Error::InvalidCharacter { position: 0, character: '0' }));
        assert_eq!(decode("zzzzzzzzzzz"), Err(Base58Error::BlockOverflow), "overflow block");
        assert_eq!(decode("1111"), Err(Base58Error::InvalidLength(4)));
        assert_eq!(decode("11111111111Og"), Err(Base58Error::InvalidCharacter { position: 11, character: 'O' }));
    }

    #[test]
    fn address_vector() {
        let spend = hex::decode("857eed804ff087b97f87848f6493e87257a8c5203cb9f422f6e7a7d8a4d299f3").unwrap();
        let view = hex::decode("0489cb98c7108372eaff2cdeddc5e76166b017a847537bf8499d61465395e942").unwrap();
        let spend: [u8; 32] = spend.try_into().unwrap();
        let view: [u8; 32] = view.try_into().unwrap();
        let addr = standard_address(&spend, &view);
        assert_eq!(
            addr,
            "WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue"
        );
        assert_eq!(addr.len(), 98);
        let (tag, data) = decode_addr(&addr).unwrap();
        assert_eq!(tag, 999730);
        assert_eq!(data.len(), 64);
        let parsed = parse_address(&addr).unwrap();
        assert_eq!(parsed.spend_public_key, spend);
        assert_eq!(parsed.payment_id, None);
        // harness packing vector: raw 8-byte id through encode_addr gives 109 chars
        let mut d = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        d.extend_from_slice(&spend);
        d.extend_from_slice(&view);
        let ia = encode_addr(999730, &d);
        assert_eq!(ia, "WrkzKmKSCDz21UVh4DEcHUhESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVknoDapN");
        assert_eq!(ia.len(), 109);
        // wallet path (Addresses.cpp:129): the id travels as ASCII hex, 120 / 186 chars
        let short = integrated_address(&spend, &view, "0102030405060708").unwrap();
        let long = integrated_address(&spend, &view, &"ab".repeat(32)).unwrap();
        assert_eq!(short.len(), 120);
        assert_eq!(long.len(), 186);
        assert!(is_integrated_address(&short) && is_integrated_address(&long) && !is_integrated_address(&addr));
        let p = parse_address(&short).unwrap();
        assert_eq!(p.payment_id.as_deref(), Some("0102030405060708"));
        assert_eq!(p.spend_public_key, spend);
        assert_eq!(parse_address(&long).unwrap().payment_id.as_deref(), Some(&*"ab".repeat(32)));
        assert_eq!(integrated_address(&spend, &view, "zz"), Err(Base58Error::BadPaymentId));
        // a corrupted checksum is rejected
        let mut bad = addr.clone();
        bad.replace_range(97..98, if addr.ends_with('e') { "f" } else { "e" });
        assert_eq!(parse_address(&bad), Err(Base58Error::BadChecksum));
        // a well-formed string under another network's prefix
        let foreign = encode_addr(1, &[&spend[..], &view[..]].concat());
        assert!(matches!(
            parse_address(&foreign),
            Err(Base58Error::WrongPrefix(1)) | Err(Base58Error::WrongAddressLength(_))
        ));
    }
}
