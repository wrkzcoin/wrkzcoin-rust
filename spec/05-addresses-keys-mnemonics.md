# 05 - Addresses, integrated addresses, mnemonics, subwallets

Source files: `src/common/Base58.cpp`, `src/utilities/Addresses.cpp`,
`src/mnemonics/Mnemonics.cpp`, `src/mnemonics/CRC32.h`,
`src/mnemonics/WordList.h`, `src/config/WalletConfig.h`,
`src/subwallets/SubWallets.cpp`. Vectors: `vectors/primitives.txt` sections
"keys", "mnemonic", "base58", "varint".

## Base58 (CryptoNote block variant)

`src/common/Base58.cpp`. This is not Bitcoin base58. The input is processed in
8-byte blocks, each encoded independently as a fixed-width big-endian number.

- Alphabet: `123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz` (58 symbols, no `0 O I l`).
- Full block: 8 input bytes → exactly 11 characters.
- Last partial block of `n` bytes (1..7) → `encoded_block_sizes[n]` characters: `[0, 2, 3, 5, 6, 7, 9, 10, 11]` indexed by `n`.
- Each block is read as a big-endian unsigned integer, converted to base 58, and left-padded with `1` to the fixed width.
- Decoding: the encoded length must decompose as `11·k + r` with `r` a valid entry of the table; each block is checked for overflow (`decode_block`, `Base58.cpp:148`).

**Vectors**:

    encode(00000000)            111111
    encode(61)                  2g
    encode(68656c6c6f)          Cn8eVZg
    encode(68656c6c6f20776f726c6421)  JTmsyNwG6XQ3vdzkp
    encode(00000001)            111112

## Address encoding

`Tools::Base58::encode_addr(tag, data)` (`Base58.cpp:259`):

    buf      = varint(tag) ‖ data
    checksum = keccak(buf)[0..4)
    address  = base58(buf ‖ checksum)

`decode_addr` (`Base58.cpp:269`) reverses it: base58-decode, split off the
last 4 bytes, recompute `keccak` over the rest and compare, then read the
varint tag and return the remaining bytes.

For a standard address (`Utilities::getAccountAddressAsStr`,
`Addresses.cpp:168`):

    tag  = 999730  (varint b2823d)
    data = spend_public (32) ‖ view_public (32)     (AccountPublicAddress serialization, 04-serialization.md)

giving 3 + 64 + 4 = 71 bytes → 8 full blocks (88 chars) + 7 bytes (10 chars)
= **98 characters**, always starting with `Wrkz`.

Parsing (`parseAccountAddressString`, `Addresses.cpp:175`) MUST check that the
prefix is 999730 and that both keys decompress (`check_key`).

**Vector**:

    spend_public  857eed804ff087b97f87848f6493e87257a8c5203cb9f422f6e7a7d8a4d299f3
    view_public   0489cb98c7108372eaff2cdeddc5e76166b017a847537bf8499d61465395e942
    checksum      f8e92e65
    address       WrkzaiGnz9chESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVkpDFBue

## Integrated addresses

`Utilities::createIntegratedAddress` (`Addresses.cpp:129`):

    data = payment_id_hex_string ‖ spend_public ‖ view_public
    address = encode_addr(999730, data)

The payment id travels as its **ASCII hex string** (16 or 64 characters),
not as raw bytes, placed **before** the keys. Two sizes exist:

| Payment id | Packed bytes | Address length | Constant |
| --- | --- | --- | --- |
| short | 16 (hex chars) | 120 (`3 + 16 + 64 + 4 = 87` bytes → 10 blocks + 7) | `integratedAddressLength` |
| long | 64 (hex chars) | 186 (`3 + 64 + 64 + 4 = 135` bytes → 16 blocks + 7) | `integratedAddressLengthLong` |

`isIntegratedAddress` (`Addresses.cpp:18`) decides by length alone, and
`extractIntegratedAddressData` (`Addresses.cpp:64`) uses the length to know
how many payment id bytes to strip. A port must produce exactly these lengths.

**Vector** (short payment id `0102030405060708` with the keys above):

    WrkzKmKSCDz21UVh4DEcHUhESkaqwevEPRjtnU46TRBRY6z3EZ7fXymcE6dTbHDSWJ3mCgQ2Vdzxca6v5FEwodPCExuJuAmtVj86gVknoDapN   (109 chars)

Note the vector is 109 characters, not 120: the harness packed the raw 8
bytes, which `createIntegratedAddress` never does (it packs the 16-character
hex string after validating it). The value only confirms the packing order.
`extractIntegratedAddressData` strips 16 or 64 characters by address length
and returns them as the payment id string.

When a wallet sends to an integrated address it extracts the standard address
and the payment id and sends to the standard address with that payment id
(`10-wallet.md`). A short id extracted this way is encrypted on the wire
(`03-crypto-primitives.md`); a long id is plaintext.

## Mnemonic seed (25 words)

`src/mnemonics/Mnemonics.cpp`. This is the Monero/Electrum-style scheme with
the same 1626-word English list (`WordList.h`, copy it verbatim; it is the
word list, not the algorithm, that makes seeds portable).

Encoding a 32-byte spend private key (`PrivateKeyToMnemonic`, line 117):

    for each 4-byte chunk, read as uint32 little-endian `val`:
        w1 = val % 1626
        w2 = (val / 1626 + w1) % 1626
        w3 = (val / 1626 / 1626 + w2) % 1626
        emit words[w1], words[w2], words[w3]
    → 24 words
    checksum word = words[ crc32(concat of first 3 chars of each of the 24 words) % 24 ]
    → 25 words, space separated

`crc32` is the standard reflected CRC-32 (polynomial `0xEDB88320`, init and
xor-out `0xFFFFFFFF`), table in `CRC32.h`. The checksum word is one of the 24
words, chosen by index; it is not a 25th distinct word.

Decoding (`MnemonicToPrivateKey`, line 31): lowercase the words, require
exactly 25, every word in the list, valid checksum, then for each triple

    val = w1 + 1626·((1626 − w1 + w2) % 1626) + 1626²·((1626 − w2 + w3) % 1626)
    require val % 1626 == w1
    append val as 4 bytes little-endian

The result is used directly as the spend private key. It is not reduced
here; a key produced by `PrivateKeyToMnemonic` was already reduced, and the
wallet validates imported keys with `sc_check` before use.

**Vector** (spend_secret `243d1bb4…1101` from the keys section):

    eluded ceiling theatrics orange mixture epoxy viewpoint oatmeal aggravate tell different dating intended richly slower inundate ridges slug inundate ridges slug were rotate rudely viewpoint

Round trip returns the same 32 bytes.

## Wallet identity and subwallets

A wallet container (`SubWallets`) holds one private view key and one or more
subwallets, each with its own spend key pair and address
(`SubWallets.cpp:27`). The first subwallet is the primary address.

- New wallet: random spend key; view key by `generateViewFromSpend`.
- Restore from seed: mnemonic → spend key → view key derived.
- Restore from keys: spend and view keys given; both must pass `sc_check`.
- View-only wallet: view private key plus the public address; the spend
  private key is `NULL_SECRET_KEY` (all zeros) and the container is flagged
  `isViewWallet`. View wallets can scan incoming outputs but cannot compute
  key images, so they cannot see their own spends or send.
- `addSubWallet` (`SubWallets.cpp:91`): increments `subWalletIndexCounter`
  and derives the new spend key with the deterministic subwallet function
  from `03-crypto-primitives.md`; the address uses the shared view public
  key. `importSubWallet(index)` re-derives a given index and bumps the
  counter if needed; `importSubWallet(privateSpendKey)` imports an arbitrary
  key (non-deterministic subwallet, index 0 in the file).

All subwallets share `view_public` in their addresses, so a scanner can tell
that two addresses belong to the same container. That is the existing
design.

## Amount formatting

`Currency::formatAmount` (`Currency.cpp:473`): decimal with exactly 2 fraction
digits, no thousands separators, leading zero when below 1.00.
`parseAmount` (`Currency.cpp:496`) accepts up to 2 fraction digits and rejects
more (after trimming trailing zeros).

## Acceptance for this document

1. `varint`, `base58` vectors reproduce.
2. The address vector reproduces from the two public keys, and decodes back
   to prefix 999730 and the same 64-byte payload.
3. The integrated-address packing vector reproduces; an integrated address
   built through the wallet path for a 16-hex-char id is 120 characters and
   for a 64-hex-char id is 186 characters.
4. The mnemonic vector reproduces in both directions, and a modified checksum
   word is rejected.
5. Subwallet indexes 1, 2 and 5 reproduce (`03-crypto-primitives.md`).
