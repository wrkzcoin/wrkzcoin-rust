// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Transaction structures, wire format, hashes and `tx_extra`
//! (spec/04-serialization.md "Transaction", "tx_extra"; spec/06-transactions.md).

use crate::ser::{Reader, Writer};
use crate::{varint, Error, Hash, Result};
use wrkz_pow::cn_fast_hash;

/// Upper bound on the capacity reserved up front for a vector whose element
/// count came off the wire. [`Reader::count`] already bounds the count by the
/// bytes that remain, but the ratio between the wire cost of an element (as
/// little as one byte for a varint offset, two for an input) and its in-memory
/// size (64 bytes for an [`Input`]) is a 32x memory amplification. Reserving in
/// bounded steps costs one reallocation per 4096 elements on the honest path
/// and nothing on the hostile one.
const MAX_RESERVE: usize = 4096;

/// `Vec::with_capacity` capped at [`MAX_RESERVE`].
fn reserve<T>(n: usize) -> Vec<T> {
    Vec::with_capacity(n.min(MAX_RESERVE))
}

pub const CURRENT_TRANSACTION_VERSION: u64 = 1;
pub const TAG_BASE_INPUT: u8 = 0xff;
pub const TAG_KEY_INPUT: u8 = 0x02;
pub const TAG_KEY_OUTPUT: u8 = 0x02;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Coinbase input: the block index.
    Base { block_index: u64 },
    /// Ring input: amount, relative global output offsets, key image.
    Key { amount: u64, key_offsets: Vec<u64>, key_image: [u8; 32] },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub amount: u64,
    /// The only target type on this chain is `KeyOutput`.
    pub key: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TransactionPrefix {
    pub version: u64,
    pub unlock_time: u64,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub extra: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Transaction {
    pub prefix: TransactionPrefix,
    /// One vector per input, one 64-byte signature per ring member.
    pub signatures: Vec<Vec<[u8; 64]>>,
}

impl TransactionPrefix {
    pub fn write(&self, w: &mut Writer) {
        w.varint(self.version).varint(self.unlock_time);
        w.varint(self.inputs.len() as u64);
        for input in &self.inputs {
            match input {
                Input::Base { block_index } => {
                    w.u8_raw(TAG_BASE_INPUT).varint(*block_index);
                }
                Input::Key { amount, key_offsets, key_image } => {
                    w.u8_raw(TAG_KEY_INPUT).varint(*amount).varint(key_offsets.len() as u64);
                    for o in key_offsets {
                        w.varint(*o);
                    }
                    w.raw(key_image);
                }
            }
        }
        w.varint(self.outputs.len() as u64);
        for o in &self.outputs {
            w.varint(o.amount).u8_raw(TAG_KEY_OUTPUT).raw(&o.key);
        }
        w.bytes(&self.extra);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.write(&mut w);
        w.into_inner()
    }

    /// Read a prefix from the reader position (does not require the buffer to end).
    pub fn read(r: &mut Reader<'_>) -> Result<Self> {
        let p = Self::read_any_version(r)?;
        if p.version > CURRENT_TRANSACTION_VERSION {
            return Err(Error::Malformed("transaction version"));
        }
        Ok(p)
    }

    /// Prefix reader without the version check, shared with [`BaseTransaction`].
    fn read_any_version(r: &mut Reader<'_>) -> Result<Self> {
        let version = r.varint_bits(8)?;
        let unlock_time = r.varint()?;
        let n_in = r.count(2)?;
        let mut inputs = reserve(n_in);
        for _ in 0..n_in {
            match r.u8_raw()? {
                TAG_BASE_INPUT => inputs.push(Input::Base { block_index: r.varint_bits(32)? }),
                TAG_KEY_INPUT => {
                    let amount = r.varint()?;
                    let n = r.count(1)?;
                    let mut key_offsets = reserve(n);
                    for _ in 0..n {
                        key_offsets.push(r.varint_bits(32)?);
                    }
                    let key_image = r.hash()?;
                    inputs.push(Input::Key { amount, key_offsets, key_image });
                }
                _ => return Err(Error::Malformed("input tag")),
            }
        }
        let n_out = r.count(34)?;
        let mut outputs = reserve(n_out);
        for _ in 0..n_out {
            let amount = r.varint()?;
            if r.u8_raw()? != TAG_KEY_OUTPUT {
                return Err(Error::Malformed("output tag"));
            }
            outputs.push(Output { amount, key: r.hash()? });
        }
        let extra = r.bytes()?.to_vec();
        Ok(Self { version, unlock_time, inputs, outputs, extra })
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let p = Self::read(&mut r)?;
        r.finish()?;
        Ok(p)
    }

    /// `getTransactionPrefixHash`: what ring signatures sign and what the
    /// transaction proof of work hashes.
    pub fn hash(&self) -> Hash {
        cn_fast_hash(&self.to_bytes())
    }

    pub fn is_coinbase(&self) -> bool {
        self.inputs.len() == 1 && matches!(self.inputs[0], Input::Base { .. })
    }

    /// Ring size of each key input, in order.
    pub fn ring_sizes(&self) -> Vec<usize> {
        self.inputs
            .iter()
            .filter_map(|i| match i {
                Input::Key { key_offsets, .. } => Some(key_offsets.len()),
                _ => None,
            })
            .collect()
    }

    pub fn sum_inputs(&self) -> Option<u64> {
        self.inputs.iter().try_fold(0u64, |acc, i| match i {
            Input::Key { amount, .. } => acc.checked_add(*amount),
            Input::Base { .. } => Some(acc),
        })
    }

    pub fn sum_outputs(&self) -> Option<u64> {
        self.outputs.iter().try_fold(0u64, |acc, o| acc.checked_add(o.amount))
    }
}

/// `BaseTransaction` (`CryptoNoteSerialization.cpp:222`): the coinbase of a
/// merge-mining **parent** block. Unlike `Transaction`, any `version` is
/// accepted, and a version >= 2 carries one extra `ignored` varint **after**
/// `extra` (foreign chains' v2 coinbases appear on this chain, e.g. block
/// 600,001). The C++ always writes `ignored = 0`, so a non-zero value would not
/// round-trip; none has been observed. A daemon template's parent coinbase is
/// the default-constructed value: version 0, no inputs, no outputs, extra = tag.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BaseTransaction {
    pub prefix: TransactionPrefix,
}

impl BaseTransaction {
    pub fn write(&self, w: &mut Writer) {
        self.prefix.write(w);
        if self.prefix.version >= 2 {
            w.varint(0);
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.write(&mut w);
        w.into_inner()
    }

    pub fn read(r: &mut Reader<'_>) -> Result<Self> {
        let prefix = TransactionPrefix::read_any_version(r)?;
        if prefix.version >= 2 {
            let _ignored = r.varint()?;
        }
        Ok(Self { prefix })
    }

    /// `getBaseTransactionHash` (`CryptoNoteTools.h:81`): the plain object hash
    /// for version < 2; for version >= 2 the Monero-style
    /// `keccak(prefix_hash ‖ keccak(0x00) ‖ 32 zero bytes)`.
    pub fn hash(&self) -> Hash {
        if self.prefix.version < 2 {
            return cn_fast_hash(&self.prefix.to_bytes());
        }
        let mut data = [0u8; 96];
        data[..32].copy_from_slice(&cn_fast_hash(&self.prefix.to_bytes()));
        data[32..64].copy_from_slice(&cn_fast_hash(&[0u8]));
        cn_fast_hash(&data)
    }
}

impl Transaction {
    pub fn write(&self, w: &mut Writer) -> Result<()> {
        self.prefix.write(w);
        // CryptoNoteSerialization.cpp:237 output rules
        if self.signatures.is_empty() {
            for input in &self.prefix.inputs {
                if let Input::Key { key_offsets, .. } = input {
                    if !key_offsets.is_empty() {
                        return Err(Error::Malformed("signatures missing for a key input"));
                    }
                }
            }
            return Ok(());
        }
        if self.signatures.len() != self.prefix.inputs.len() {
            return Err(Error::Malformed("signature vector count != input count"));
        }
        for (input, sigs) in self.prefix.inputs.iter().zip(&self.signatures) {
            let ring = match input {
                Input::Key { key_offsets, .. } => key_offsets.len(),
                Input::Base { .. } => 0,
            };
            if sigs.len() != ring {
                return Err(Error::Malformed("signature count != ring size"));
            }
            for s in sigs {
                w.raw(s);
            }
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        self.write(&mut w)?;
        Ok(w.into_inner())
    }

    /// Read a full transaction; the reader must be positioned at its start and
    /// the signatures run exactly to the end of the buffer.
    pub fn read(r: &mut Reader<'_>) -> Result<Self> {
        let prefix = TransactionPrefix::read(r)?;
        let mut signatures = reserve(prefix.inputs.len());
        for input in &prefix.inputs {
            let ring = match input {
                Input::Key { key_offsets, .. } => key_offsets.len(),
                Input::Base { .. } => 0,
            };
            let mut sigs = reserve(ring);
            for _ in 0..ring {
                sigs.push(r.raw(64)?.try_into().unwrap());
            }
            signatures.push(sigs);
        }
        // A coinbase has no signature bytes and an empty signature vector.
        if prefix.is_coinbase() {
            signatures.clear();
        }
        Ok(Self { prefix, signatures })
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let t = Self::read(&mut r)?;
        r.finish()?;
        Ok(t)
    }

    /// `getTransactionHash`: keccak of the full serialization, signatures included.
    pub fn hash(&self) -> Result<Hash> {
        Ok(cn_fast_hash(&self.to_bytes()?))
    }

    pub fn fee(&self) -> Option<u64> {
        self.prefix.sum_inputs()?.checked_sub(self.prefix.sum_outputs()?)
    }
}

/// Convert relative `key_offsets` to absolute global indexes
/// (`relativeOutputOffsetsToAbsolute`). Returns `None` on overflow.
pub fn relative_offsets_to_absolute(offsets: &[u64]) -> Option<Vec<u64>> {
    let mut out = reserve(offsets.len());
    let mut acc: u64 = 0;
    for (i, o) in offsets.iter().enumerate() {
        acc = if i == 0 { *o } else { acc.checked_add(*o)? };
        out.push(acc);
    }
    Some(out)
}

/// Absolute global indexes to relative offsets
/// (`absolute_output_offsets_to_relative`). The input must be sorted strictly
/// ascending, which is what the C++ guarantees by sorting before it subtracts;
/// an unsorted or duplicated sequence has no relative encoding, so this returns
/// `None` instead of wrapping (release) or panicking (debug).
pub fn absolute_to_relative_offsets(abs: &[u64]) -> Option<Vec<u64>> {
    let mut out = reserve(abs.len());
    for (i, a) in abs.iter().enumerate() {
        out.push(if i == 0 { *a } else { a.checked_sub(abs[i - 1]).filter(|d| *d != 0)? });
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// tx_extra
// ---------------------------------------------------------------------------

pub const TX_EXTRA_TAG_PADDING: u8 = 0x00;
pub const TX_EXTRA_TAG_PUBKEY: u8 = 0x01;
pub const TX_EXTRA_NONCE: u8 = 0x02;
pub const TX_EXTRA_MERGE_MINING_TAG: u8 = 0x03;
pub const TX_EXTRA_TRANSACTION_POW_NONCE: u8 = 0x04;
pub const TX_EXTRA_NONCE_PAYMENT_ID: u8 = 0x00;
pub const TX_EXTRA_NONCE_ENCRYPTED_SHORT_PAYMENT_ID: u8 = 0x03;
pub const TX_EXTRA_ARBITRARY_DATA: u8 = 0x7f;
pub const ENCRYPTED_PAYMENT_ID_TAIL: u8 = 0x8d;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeMiningTag {
    pub depth: u64,
    pub merkle_root: [u8; 32],
}

/// What the **consensus** parser `parseTransactionExtra` (`TransactionExtra.cpp:23`)
/// recovers. The daemon uses it for the merge-mining tag of a parent coinbase
/// and for the transaction public key it reports to wallets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedExtra {
    pub public_key: Option<[u8; 32]>,
    pub nonce: Option<Vec<u8>>,
    pub merge_mining_tag: Option<MergeMiningTag>,
    /// Padding length, if a padding field was seen.
    pub padding: Option<usize>,
    /// `false` when the C++ parser returned `false` (malformed tail); the
    /// fields collected before the failure are still returned, exactly as the
    /// C++ callers use them.
    pub well_formed: bool,
}

/// Mirror of `parseTransactionExtra`, byte for byte:
/// - `0x00` padding: zero bytes to the end (max 255); a non-zero byte or an
///   over-long run aborts; a second padding field stops parsing;
/// - `0x01` pubkey: 32 bytes; a second one stops parsing;
/// - `0x02` nonce: 1-byte length and bytes; a second one stops parsing;
/// - `0x03` merge-mining tag: varint-length string holding `varint depth ‖ 32-byte root`;
///   a **second** tag is skipped without consuming its body (the body bytes are
///   then walked as tags);
/// - any other byte (including the wallet's `0x04` PoW nonce tag) is skipped
///   one byte at a time: there is no default case in the C++ switch.
///
/// A truncated field aborts like the C++ exception path.
pub fn parse_extra(extra: &[u8]) -> ParsedExtra {
    let mut out = ParsedExtra { well_formed: true, ..Default::default() };
    let mut i = 0;
    let fail = |mut o: ParsedExtra| {
        o.well_formed = false;
        o
    };
    while i < extra.len() {
        let tag = extra[i];
        i += 1;
        match tag {
            TX_EXTRA_TAG_PADDING => {
                if out.padding.is_some() {
                    return out;
                }
                let mut size = 1usize;
                while i < extra.len() && size <= 255 {
                    if extra[i] != 0 {
                        return fail(out);
                    }
                    i += 1;
                    size += 1;
                }
                if size > 255 {
                    return fail(out);
                }
                out.padding = Some(size);
            }
            TX_EXTRA_TAG_PUBKEY => {
                if out.public_key.is_some() {
                    return out;
                }
                // `extra.len() - i` cannot underflow (`i <= extra.len()`),
                // while `i + 32` could overflow: keep every wire length on the
                // right of the comparison.
                if extra.len() - i < 32 {
                    return fail(out);
                }
                out.public_key = Some(extra[i..i + 32].try_into().unwrap());
                i += 32;
            }
            TX_EXTRA_NONCE => {
                if out.nonce.is_some() {
                    return out;
                }
                if i >= extra.len() {
                    return fail(out);
                }
                let n = extra[i] as usize;
                i += 1;
                if n > extra.len() - i {
                    return fail(out);
                }
                out.nonce = Some(extra[i..i + n].to_vec());
                i += n;
            }
            TX_EXTRA_MERGE_MINING_TAG => {
                if out.merge_mining_tag.is_some() {
                    continue; // `break` out of the switch: body not consumed
                }
                let Ok((len, ln)) = varint::read(&extra[i..]) else { return fail(out) };
                i += ln;
                // `len` is an unbounded wire varint: never add it to a cursor.
                // A u64::MAX length overflowed `i + len` here, which panicked in
                // debug and wrapped in release. The C++ equivalent is the throw
                // out of `binary(std::string)` when the length runs past the
                // stream, which is this `fail`.
                if len > (extra.len() - i) as u64 {
                    return fail(out);
                }
                let len = len as usize;
                let body = &extra[i..i + len];
                i += len;
                let Ok((depth, dn)) = varint::read(body) else { return fail(out) };
                // `doSerialize` (`CryptoNoteSerialization.cpp:512`) reads the
                // depth varint and 32 bytes out of the sub-stream and ignores
                // whatever follows it, so an over-long body is accepted and
                // only a short one throws.
                if body.len() - dn < 32 {
                    return fail(out);
                }
                out.merge_mining_tag =
                    Some(MergeMiningTag { depth, merkle_root: body[dn..dn + 32].try_into().unwrap() });
            }
            _ => {}
        }
    }
    out
}

/// Payment id recovered by the wallet-side parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentId {
    /// 32-byte plaintext id (sub-tag 0x00).
    Long([u8; 32]),
    /// 8-byte encrypted short id (sub-tag 0x03).
    EncryptedShort([u8; 8]),
}

/// What `Utilities::parseExtra` (`ParseExtra.cpp:46`) recovers: the wallet
/// and daemon-RPC view of `extra`, looser than the consensus parser.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalletParsedExtra {
    pub public_key: Option<[u8; 32]>,
    pub payment_id: Option<PaymentId>,
    pub arbitrary_data: Option<Vec<u8>>,
    pub merge_mining_tag: Option<MergeMiningTag>,
    pub pow_nonce: Option<[u8; 8]>,
}

/// Mirror of `ParseExtra.cpp` including its quirks: a field is recognised
/// wherever its tag byte appears with enough bytes left; the nonce length is
/// read as a **varint**; after a nonce only the recognised sub-fields advance
/// the outer cursor, so unknown nonce bytes are re-walked as top-level tags;
/// after a merge-mining tag the cursor advances by `depth varint + 32` but not
/// by the length varint. Legacy plaintext short ids (sub-tags 0x01/0x02) are
/// consumed and never reported.
pub fn parse_extra_wallet(extra: &[u8]) -> WalletParsedExtra {
    let mut out = WalletParsedExtra::default();
    let (mut seen_pk, mut seen_nonce, mut seen_data, mut seen_pid, mut seen_mm, mut seen_pow) =
        (false, false, false, false, false, false);
    let mut it = 0usize;
    while it < extra.len() {
        if seen_pk && seen_pid && seen_mm && seen_data && seen_pow {
            break;
        }
        let c = extra[it];
        let remaining = extra.len() - it;
        if c == TX_EXTRA_TAG_PUBKEY && remaining > 32 && !seen_pk {
            out.public_key = Some(extra[it + 1..it + 33].try_into().unwrap());
            it += 32;
            seen_pk = true;
            it += 1;
            continue;
        }
        if c == TX_EXTRA_NONCE && remaining > 1 && !seen_nonce {
            let (nonce_size, read_nonce_size) = match varint::read(&extra[it + 1..]) {
                Ok((v, n)) => (v as usize, n),
                Err(_) => (0, 0),
            };
            let mut advance = read_nonce_size;
            // `read_nonce_size < remaining` always (the varint was read out of
            // the `remaining - 1` bytes after the tag), so the subtraction is
            // safe. The C++ `elementsRemaining > readNonceSize + nonceSize`
            // wraps for a u64::MAX length and then runs `std::copy` off the end
            // of the buffer, so there is no defined behaviour to mirror.
            if remaining - read_nonce_size > nonce_size {
                let nonce = &extra[it + 1 + read_nonce_size..it + 1 + read_nonce_size + nonce_size];
                let mut is = 0usize;
                while is < nonce.len() {
                    let sb = nonce[is];
                    let nrem = nonce.len() - is;
                    if sb == TX_EXTRA_NONCE_PAYMENT_ID && nrem > 32 && !seen_pid {
                        out.payment_id = Some(PaymentId::Long(nonce[is + 1..is + 33].try_into().unwrap()));
                        seen_pid = true;
                        advance += 1 + 32;
                        is += 32;
                        is += 1;
                        continue;
                    }
                    if (sb == 0x01 || sb == 0x02) && nrem > 8 {
                        advance += 1 + 8;
                        is += 8;
                        is += 1;
                        continue;
                    }
                    if sb == TX_EXTRA_NONCE_ENCRYPTED_SHORT_PAYMENT_ID && nrem > 8 && !seen_pid {
                        out.payment_id = Some(PaymentId::EncryptedShort(nonce[is + 1..is + 9].try_into().unwrap()));
                        seen_pid = true;
                        advance += 1 + 8;
                        is += 8;
                        is += 1;
                        continue;
                    }
                    if sb == TX_EXTRA_ARBITRARY_DATA && nrem > 1 && !seen_data {
                        let (data_size, read_data_size) = match varint::read(&nonce[is + 1..]) {
                            Ok((v, n)) => (v as usize, n),
                            Err(_) => (0, 0),
                        };
                        // `read_data_size <= nrem - 1`, so this cannot underflow.
                        if nrem - 1 - read_data_size >= data_size {
                            out.arbitrary_data =
                                Some(nonce[is + 1 + read_data_size..is + 1 + read_data_size + data_size].to_vec());
                            seen_data = true;
                            advance += 1 + read_data_size + data_size;
                            is += read_data_size + data_size;
                            is += 1;
                            continue;
                        }
                    }
                    is += 1;
                }
            }
            it += advance;
            seen_nonce = true;
            it += 1;
            continue;
        }
        if c == TX_EXTRA_MERGE_MINING_TAG && remaining > 1 && !seen_mm {
            let (data_size, read_data_size) = match varint::read(&extra[it + 1..]) {
                Ok((v, n)) => (v as usize, n),
                Err(_) => (0, 0),
            };
            if remaining - read_data_size > data_size && data_size >= 33 {
                let (depth, read_depth_size) = match varint::read(&extra[it + 1 + read_data_size..]) {
                    Ok((v, n)) => (v, n),
                    Err(_) => (0, 0),
                };
                let begin = it + 1 + read_data_size + read_depth_size;
                if begin + 32 <= extra.len() {
                    out.merge_mining_tag =
                        Some(MergeMiningTag { depth, merkle_root: extra[begin..begin + 32].try_into().unwrap() });
                    it += read_depth_size + 32;
                    seen_mm = true;
                    it += 1;
                    continue;
                }
            }
        }
        if c == TX_EXTRA_TRANSACTION_POW_NONCE && remaining > 8 && !seen_pow {
            out.pow_nonce = Some(extra[it + 1..it + 9].try_into().unwrap());
            it += 8;
            seen_pow = true;
            it += 1;
            continue;
        }
        it += 1;
    }
    out
}

/// Build a wallet-style `extra`: `01‖R`, optional `02‖len‖nonce`, optional `04‖pow nonce`
/// (`Transfer.cpp:1487-1510`, `TransactionPoW.cpp:96`).
///
/// The nonce length is written as a single byte, because that is what the
/// consensus parser reads back, so a payload longer than 255 bytes cannot be
/// expressed. The length comes from caller data, so that is an
/// `Err(Error::Malformed)` and not a panic.
pub fn build_extra(tx_public_key: &[u8; 32], nonce: Option<&[u8]>, pow_nonce: Option<&[u8; 8]>) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(34 + 2 + nonce.map_or(0, |n| n.len()) + 9);
    out.push(TX_EXTRA_TAG_PUBKEY);
    out.extend_from_slice(tx_public_key);
    if let Some(n) = nonce {
        if n.len() > 255 {
            return Err(Error::Malformed("tx_extra nonce longer than 255 bytes"));
        }
        out.push(TX_EXTRA_NONCE);
        out.push(n.len() as u8);
        out.extend_from_slice(n);
    }
    if let Some(p) = pow_nonce {
        out.push(TX_EXTRA_TRANSACTION_POW_NONCE);
        out.extend_from_slice(p);
    }
    Ok(out)
}

/// Build the nonce payload: optional payment id sub-field then optional arbitrary data.
pub fn build_nonce(payment_id: Option<&PaymentId>, arbitrary_data: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    match payment_id {
        Some(PaymentId::Long(id)) => {
            out.push(TX_EXTRA_NONCE_PAYMENT_ID);
            out.extend_from_slice(id);
        }
        Some(PaymentId::EncryptedShort(id)) => {
            out.push(TX_EXTRA_NONCE_ENCRYPTED_SHORT_PAYMENT_ID);
            out.extend_from_slice(id);
        }
        None => {}
    }
    if let Some(d) = arbitrary_data {
        out.push(TX_EXTRA_ARBITRARY_DATA);
        varint::write(&mut out, d.len() as u64);
        out.extend_from_slice(d);
    }
    out
}

/// `appendMergeMiningTagToExtra`: `03 ‖ varint(len) ‖ varint(depth) ‖ root`.
pub fn append_merge_mining_tag(extra: &mut Vec<u8>, tag: &MergeMiningTag) {
    let mut body = varint::encode(tag.depth);
    body.extend_from_slice(&tag.merkle_root);
    extra.push(TX_EXTRA_MERGE_MINING_TAG);
    varint::write(extra, body.len() as u64);
    extra.extend_from_slice(&body);
}

/// `decomposeAmount` / `decompose_amount_into_digits`
/// (`CryptoNoteFormatUtils.h:74`), the split `Currency::constructMinerTx`
/// applies to a block reward.
///
/// Each non-zero decimal digit times its power of ten, least significant first.
/// A chunk is folded into the running dust total while `dust + chunk <=
/// dust_threshold` — note the **`<=`**, so a chunk that lands exactly on the
/// threshold is still dust. The accumulated dust is emitted **in place**,
/// immediately before the first chunk that is not dust, and only at the end if
/// every chunk was dust. `amount == 0` yields nothing.
///
/// The C++ documents the shape with 62,387,455,827 at a dust threshold of
/// 455,827: `455827 + 7000000 + 80000000 + 300000000 + 2000000000 +
/// 60000000000`, the dust first.
pub fn decompose_amount(amount: u64, dust_threshold: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut dust_handled = false;
    let mut dust: u64 = 0;
    let mut order: u64 = 1;
    let mut a = amount;
    while a > 0 {
        let chunk = (a % 10) * order;
        a /= 10;
        // The last multiplication of a 20-digit amount is never read again;
        // the C++ `order *= 10` wraps there too.
        order = order.wrapping_mul(10);
        if dust + chunk <= dust_threshold {
            dust += chunk;
        } else {
            if !dust_handled && dust != 0 {
                out.push(dust);
                dust_handled = true;
            }
            if chunk != 0 {
                out.push(chunk);
            }
        }
    }
    if !dust_handled && dust != 0 {
        out.push(dust);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_transaction_vector() {
        // spec/04-serialization.md, "synthetic transaction"
        let mut extra =
            build_extra(&[0x77; 32], Some(&build_nonce(Some(&PaymentId::Long([0x88; 32])), None)), None).unwrap();
        assert_eq!(extra.len(), 33 + 2 + 33);
        let tx = Transaction {
            prefix: TransactionPrefix {
                version: 1,
                unlock_time: 0,
                inputs: vec![Input::Key { amount: 500, key_offsets: vec![7, 3, 300], key_image: [0x66; 32] }],
                outputs: vec![Output { amount: 400, key: [0x71; 32] }, Output { amount: 90, key: [0x76; 32] }],
                extra: std::mem::take(&mut extra),
            },
            signatures: vec![vec![[0u8; 64]; 3]],
        };
        let prefix_hex = "01000102f403030703ac0266666666666666666666666666666666666666666666666666666666666666660290030271717171717171717171717171717171717171717171717171717171717171715a027676767676767676767676767676767676767676767676767676767676767676440177777777777777777777777777777777777777777777777777777777777777770221008888888888888888888888888888888888888888888888888888888888888888";
        assert_eq!(hex::encode(tx.prefix.to_bytes()), prefix_hex);
        let full = tx.to_bytes().unwrap();
        assert_eq!(full.len(), prefix_hex.len() / 2 + 192);
        assert_eq!(hex::encode(tx.prefix.hash()), "d45c12560672a1f7812a0a201396e55c3fc3b3913d1083540aca4230adb37bcc");
        assert_eq!(hex::encode(tx.hash().unwrap()), "3b7b2aa764e6631bfa203acdd6a1d6cc1000b7a9e2701ad92e3e55206b4e81a7");
        assert_eq!(tx.fee(), Some(10));
        let back = Transaction::from_bytes(&full).unwrap();
        assert_eq!(back, tx);
        let mut extended = full.clone();
        extended.push(0);
        assert_eq!(Transaction::from_bytes(&extended), Err(Error::TrailingBytes(1)));
        let parsed = parse_extra(&tx.prefix.extra);
        assert_eq!(parsed.public_key, Some([0x77; 32]));
        assert!(parsed.well_formed && parsed.nonce.as_ref().unwrap().len() == 33);
        let w = parse_extra_wallet(&tx.prefix.extra);
        assert_eq!(w.public_key, Some([0x77; 32]));
        assert_eq!(w.payment_id, Some(PaymentId::Long([0x88; 32])));
    }

    #[test]
    fn offsets_round_trip() {
        let abs = vec![10u64, 15, 100];
        let rel = absolute_to_relative_offsets(&abs).unwrap();
        assert_eq!(rel, vec![10, 5, 85]);
        assert_eq!(relative_offsets_to_absolute(&rel), Some(abs));
    }

    #[test]
    fn offsets_reject_unsorted_and_duplicate_input() {
        // The C++ subtracts neighbours in place after the caller has sorted;
        // an unsorted or repeated index has no relative encoding at all, and
        // used to underflow (panic in debug, wrap in release).
        assert_eq!(absolute_to_relative_offsets(&[5, 1]), None);
        assert_eq!(absolute_to_relative_offsets(&[7, 7]), None);
        assert_eq!(absolute_to_relative_offsets(&[1, 2, 2, 3]), None);
        assert_eq!(absolute_to_relative_offsets(&[0, u64::MAX]), Some(vec![0, u64::MAX]));
        assert_eq!(absolute_to_relative_offsets(&[]), Some(vec![]));
        assert_eq!(absolute_to_relative_offsets(&[42]), Some(vec![42]));
    }

    #[test]
    fn build_extra_rejects_an_over_long_nonce() {
        assert!(build_extra(&[1; 32], Some(&[0u8; 255]), None).is_ok());
        assert_eq!(
            build_extra(&[1; 32], Some(&[0u8; 256]), None),
            Err(Error::Malformed("tx_extra nonce longer than 255 bytes"))
        );
    }

    #[test]
    fn decompose_block_one_reward() {
        // 07: block 1 reward 11563301 -> 1, 300, 3000, 60000, 500000, 1000000, 10000000
        assert_eq!(decompose_amount(11563301, 0), vec![1, 300, 3000, 60000, 500000, 1000000, 10000000]);
        assert_eq!(decompose_amount(1000000, 0), vec![1000000]);
        assert_eq!(decompose_amount(0, 0), Vec::<u64>::new());
        assert_eq!(decompose_amount(0, 1000), Vec::<u64>::new());
    }

    #[test]
    fn decompose_with_a_dust_threshold() {
        // Hand-derived from `decompose_amount_into_digits`
        // (`CryptoNoteFormatUtils.h:74`) for the value in its own comment.
        // Chunks least significant first: 7, 20, 800, 5000, 50000, 400000,
        // 7000000, 80000000, 300000000, 2000000000, 60000000000.
        // dust: 7, 27, 827, 5827, 55827, 455827 — the sixth lands exactly on
        // the threshold and `dust + chunk <= dust_threshold` keeps it as dust.
        // 7000000 takes the else branch, which flushes the dust *first*.
        assert_eq!(
            decompose_amount(62_387_455_827, 455_827),
            vec![455_827, 7_000_000, 80_000_000, 300_000_000, 2_000_000_000, 60_000_000_000]
        );
        // With `<` instead of `<=` the 400000 chunk would not be dust and the
        // output would start `55827, 400000, ...`; pin that it does not.
        assert_ne!(decompose_amount(62_387_455_827, 455_827)[0], 55_827);
        // Every chunk is dust: emitted once, at the end.
        assert_eq!(decompose_amount(5, 10), vec![5]);
        assert_eq!(decompose_amount(999, 999), vec![999]);
        // A threshold one below the total leaves the top digit standing, and
        // the dust still comes first.
        assert_eq!(decompose_amount(999, 99), vec![99, 900]);
        // The dust total is exactly `<=`, so a chunk equal to the threshold is
        // dust even when it is the only one.
        assert_eq!(decompose_amount(300, 300), vec![300]);
        assert_eq!(decompose_amount(300, 299), vec![300]);
        // u64::MAX has 20 digits, two of which are 0 and are not emitted; the
        // final `order` multiplication wraps in the C++ and is never read.
        let all = decompose_amount(u64::MAX, 0);
        assert_eq!(all.len(), 18);
        assert_eq!(all.iter().sum::<u64>(), u64::MAX);
    }

    // ---- tx_extra quirks (spec/04 "tx_extra") ---------------------------------

    #[test]
    fn parse_extra_second_pubkey_stops_parsing() {
        // Two pubkey fields: the second `case` returns `true` immediately, so
        // the first key is kept and nothing after it is looked at.
        let mut extra = vec![TX_EXTRA_TAG_PUBKEY];
        extra.extend_from_slice(&[0xaa; 32]);
        extra.push(TX_EXTRA_TAG_PUBKEY);
        extra.extend_from_slice(&[0xbb; 32]);
        extra.push(TX_EXTRA_NONCE);
        extra.extend_from_slice(&[1, 0x77]);
        let p = parse_extra(&extra);
        assert_eq!(p.public_key, Some([0xaa; 32]));
        assert_eq!(p.nonce, None, "parsing stopped at the second pubkey");
        assert!(p.well_formed, "an early return is `true`, not a failure");
    }

    #[test]
    fn parse_extra_second_nonce_stops_parsing() {
        let mut extra = vec![TX_EXTRA_NONCE, 1, 0x11, TX_EXTRA_NONCE, 1, 0x22];
        extra.push(TX_EXTRA_TAG_PUBKEY);
        extra.extend_from_slice(&[0xcc; 32]);
        let p = parse_extra(&extra);
        assert_eq!(p.nonce, Some(vec![0x11]));
        assert_eq!(p.public_key, None, "the pubkey after the second nonce is never reached");
        assert!(p.well_formed);
    }

    #[test]
    fn parse_extra_second_merge_mining_tag_is_skipped_without_its_body() {
        // `break` out of the switch, not `return`: the cursor stays on the
        // length varint, so the tag body is then walked as top-level tags.
        let tag = MergeMiningTag { depth: 0, merkle_root: [0x33; 32] };
        let mut extra = Vec::new();
        append_merge_mining_tag(&mut extra, &tag);
        let first_len = extra.len();
        // The second body is `21 00 <32 bytes>`; byte 0x21 is an unknown tag
        // and skipped, 0x00 opens a padding field, and the 32 root bytes are
        // not zero, so the padding fails and `well_formed` goes false.
        append_merge_mining_tag(&mut extra, &MergeMiningTag { depth: 0, merkle_root: [0x44; 32] });
        assert_eq!(extra.len(), 2 * first_len);
        let p = parse_extra(&extra);
        assert_eq!(p.merge_mining_tag, Some(tag), "the first tag wins");
        assert!(!p.well_formed, "the second body is re-walked as tags and trips the padding rule");

        // With an all-zero second root the re-walked body is a valid padding
        // run instead, and the parse stays well formed.
        let mut extra = Vec::new();
        append_merge_mining_tag(&mut extra, &MergeMiningTag { depth: 0, merkle_root: [0x33; 32] });
        append_merge_mining_tag(&mut extra, &MergeMiningTag { depth: 0, merkle_root: [0; 32] });
        let p = parse_extra(&extra);
        assert_eq!(p.merge_mining_tag.unwrap().merkle_root, [0x33; 32]);
        assert_eq!(p.padding, Some(33), "0x00 root byte opened padding to the end");
        assert!(p.well_formed);
    }

    #[test]
    fn parse_extra_non_zero_padding_byte_keeps_the_fields_it_had() {
        let mut extra = vec![TX_EXTRA_TAG_PUBKEY];
        extra.extend_from_slice(&[0x55; 32]);
        extra.extend_from_slice(&[TX_EXTRA_TAG_PADDING, 0, 0, 0x01]);
        let p = parse_extra(&extra);
        assert!(!p.well_formed, "a non-zero byte inside padding returns false");
        assert_eq!(p.public_key, Some([0x55; 32]), "callers still use what was collected");
        assert_eq!(p.padding, None);
        // A padding run longer than 255 bytes fails the same way.
        let mut extra = vec![TX_EXTRA_TAG_PADDING];
        extra.extend_from_slice(&[0u8; 255]);
        let p = parse_extra(&extra);
        assert!(!p.well_formed);
        // Exactly 255 is fine.
        let mut extra = vec![TX_EXTRA_TAG_PADDING];
        extra.extend_from_slice(&[0u8; 254]);
        let p = parse_extra(&extra);
        assert!(p.well_formed);
        assert_eq!(p.padding, Some(255));
    }

    #[test]
    fn parse_extra_ignores_the_pow_nonce_tag_one_byte_at_a_time() {
        // The consensus switch has no default case, so 0x04 is not a field:
        // its 8 payload bytes are walked as tags. 0x00 would open padding, so
        // use a payload with no structural bytes and check the pubkey after it
        // is still found.
        let mut extra = vec![TX_EXTRA_TRANSACTION_POW_NONCE];
        extra.extend_from_slice(&[0x99; 8]);
        extra.push(TX_EXTRA_TAG_PUBKEY);
        extra.extend_from_slice(&[0x66; 32]);
        let p = parse_extra(&extra);
        assert_eq!(p.public_key, Some([0x66; 32]));
        assert!(p.well_formed);
        // The wallet parser, in contrast, does know the tag.
        let w = parse_extra_wallet(&extra);
        assert_eq!(w.pow_nonce, Some([0x99; 8]));
        assert_eq!(w.public_key, Some([0x66; 32]));
    }

    #[test]
    fn parse_extra_accepts_an_over_long_merge_mining_body() {
        // `doSerialize` reads depth + 32 bytes out of the sub-stream and drops
        // the rest, so a body longer than `varint(depth) + 32` still parses;
        // only a short one throws.
        let mut extra = vec![TX_EXTRA_MERGE_MINING_TAG, 36, 0];
        extra.extend_from_slice(&[0x77; 32]);
        extra.extend_from_slice(&[0xee; 3]);
        let p = parse_extra(&extra);
        assert_eq!(p.merge_mining_tag, Some(MergeMiningTag { depth: 0, merkle_root: [0x77; 32] }));
        let short = vec![TX_EXTRA_MERGE_MINING_TAG, 32, 0];
        let mut short_full = short;
        short_full.extend_from_slice(&[0x77; 31]);
        assert!(!parse_extra(&short_full).well_formed);
    }

    // ---- hostile input -----------------------------------------------------

    /// The 10-byte encoding of `u64::MAX`.
    fn max_varint() -> Vec<u8> {
        varint::encode(u64::MAX)
    }

    #[test]
    fn wire_lengths_of_u64_max_are_rejected_not_added_to_a_cursor() {
        // Each of these used to overflow a `usize` addition: a panic in debug
        // and a wrapped, out-of-bounds length in release.
        let mut mm = vec![TX_EXTRA_MERGE_MINING_TAG];
        mm.extend(max_varint());
        assert_eq!(mm.len(), 11);
        let p = parse_extra(&mm);
        assert!(!p.well_formed);
        assert_eq!(p.merge_mining_tag, None);

        let mut nonce = vec![TX_EXTRA_NONCE];
        nonce.extend(max_varint());
        assert_eq!(parse_extra_wallet(&nonce), WalletParsedExtra::default());

        let mut mmw = vec![TX_EXTRA_MERGE_MINING_TAG];
        mmw.extend(max_varint());
        assert_eq!(parse_extra_wallet(&mmw), WalletParsedExtra::default());

        // 0x7f arbitrary data with a u64::MAX length, inside a nonce.
        let mut inner = vec![TX_EXTRA_ARBITRARY_DATA];
        inner.extend(max_varint());
        let mut extra = vec![TX_EXTRA_NONCE, inner.len() as u8];
        extra.extend(&inner);
        extra.push(0);
        assert_eq!(parse_extra_wallet(&extra).arbitrary_data, None);

        // Every prefix of every one of these terminates without panicking.
        for full in [&mm, &nonce, &mmw, &extra] {
            for n in 0..=full.len() {
                let _ = parse_extra(&full[..n]);
                let _ = parse_extra_wallet(&full[..n]);
            }
        }
    }

    #[test]
    fn truncated_and_hostile_transactions_return_err() {
        let tx = Transaction {
            prefix: TransactionPrefix {
                version: 1,
                unlock_time: 0,
                inputs: vec![Input::Key { amount: 7, key_offsets: vec![1, 2], key_image: [9; 32] }],
                outputs: vec![Output { amount: 7, key: [3; 32] }],
                extra: vec![],
            },
            signatures: vec![vec![[0u8; 64]; 2]],
        };
        let full = tx.to_bytes().unwrap();
        for n in 0..full.len() {
            assert!(Transaction::from_bytes(&full[..n]).is_err(), "prefix of {n} bytes must not parse");
        }
        // A ring size of u64::MAX must not become a reservation.
        let mut hostile = vec![0x01, 0x00, 0x01, TAG_KEY_INPUT, 0x07];
        hostile.extend(max_varint());
        assert!(TransactionPrefix::from_bytes(&hostile).is_err());
        // An output count of u64::MAX, likewise.
        let mut hostile = vec![0x01, 0x00, 0x00];
        hostile.extend(max_varint());
        assert!(TransactionPrefix::from_bytes(&hostile).is_err());
        // An extra length of u64::MAX.
        let mut hostile = vec![0x01, 0x00, 0x00, 0x00];
        hostile.extend(max_varint());
        assert_eq!(TransactionPrefix::from_bytes(&hostile), Err(Error::Truncated));
    }
}
