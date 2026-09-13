# 03 - Cryptographic primitives

Source files: `src/crypto/crypto.cpp`, `src/crypto/crypto.h`,
`src/crypto/crypto-ops.c` (curve arithmetic), `src/crypto/random.h`,
`src/utilities/PaymentIdEncryption.cpp`, `src/crypto/WalletCrypto.h`,
`src/crypto/chacha8.h`. Vectors: `vectors/primitives.txt`, sections
"keys" onward.

Everything here is on ed25519 (curve25519 in twisted Edwards form, base point
`G`, group order `l = 2^252 + 27742317777372353535851937790883648493`).
Points are 32-byte compressed little-endian encodings; scalars are 32-byte
little-endian integers. `keccak(x)` below means `cn_fast_hash` from
`02-hashing.md`.

## Curve operations required

The C code is the ref10 arithmetic plus CryptoNote additions. A port needs the
following operations with exactly these semantics.

| Name in C (`crypto-ops.h`) | Meaning |
| --- | --- |
| `sc_reduce32(s)` | `s mod l`, input and output 32 bytes |
| `sc_reduce(s)` | `s mod l` for a 64-byte input |
| `sc_check(s)` | 0 if `s < l` (canonical scalar), non-zero otherwise |
| `sc_add`, `sc_sub`, `sc_mul`, `sc_mulsub(r,a,b,c)` = `c - a*b mod l`, `sc_muladd`, `sc_0`, `sc_isnonzero` | scalar arithmetic mod `l` |
| `ge_scalarmult_base(P, s)` | `P = s·G` |
| `ge_scalarmult(P, s, A)` | `P = s·A` |
| `ge_double_scalarmult_base_vartime(P, a, A, b)` | `P = a·A + b·G` |
| `ge_double_scalarmult_precomp_vartime(P, a, A, b, Bprecomp)` | `P = a·A + b·B` with `B` precomputed |
| `ge_frombytes_vartime(A, bytes)` | decompress; returns non-zero if the encoding is not a valid point |
| `ge_tobytes`, `ge_p3_tobytes` | compress |
| `ge_mul8(P, A)` | `P = 8·A` (cofactor clearing) |
| `ge_fromfe_frombytes_vartime(P, bytes)` | the CryptoNote "hash to point" map: interprets 32 bytes as a field element and maps it to a curve point with the Elligator-like formulas in `crypto-ops.c` (constants `fe_ma2`, `fe_ma`, `fe_fffb1..4`, `fe_sqrtm1`). This is Monero's `ge_fromfe_frombytes_vartime`, unchanged. A port must reproduce it exactly; the `hash_to_ec` vectors test it |
| `ge_check_subgroup_precomp_vartime(Bprecomp)` | 0 if `l·B = identity` (point is in the prime-order subgroup) |

The Monero and TurtleCoin implementations of all of these are interchangeable
with this code; a port may take them from any faithful port of Monero's
`crypto-ops.c`, but must still pass the vectors.

## Key generation

`crypto_ops::generate_keys` (`crypto.cpp:60`): `sec = random 64 bytes reduced
with sc_reduce`; `pub = sec·G`.

`generate_deterministic_keys(pub, sec, seed)` (`crypto.cpp:68`):
`sec = sc_reduce32(seed)`; `pub = sec·G`. Used by view-from-spend and by every
vector below, so a port can be tested without randomness.

`secret_key_to_public_key` (`crypto.cpp:103`) fails if `sc_check(sec) != 0`.
A stored private key MUST already be reduced.

`check_key(pub)` (`crypto.cpp:97`) is `ge_frombytes_vartime` succeeding. It
does not check the subgroup; output keys on chain are accepted as long as they
decompress.

**Vector** (`primitives.txt`, "keys"):

    seed          1111…11 (32 × 0x11)
    spend_secret  243d1bb4f6adfeb83a74196e321732fc10111111111111111111111111111101
    spend_public  857eed804ff087b97f87848f6493e87257a8c5203cb9f422f6e7a7d8a4d299f3

## View key from spend key

`crypto_ops::generateViewFromSpend` (`crypto.cpp:719`):

    view_seed   = keccak(spend_secret)          (32 bytes, cn_fast_hash of the 32-byte scalar)
    view_secret = sc_reduce32(view_seed)
    view_public = view_secret · G

Every wallet in this ecosystem derives its view key this way, which is why a
25-word mnemonic (the spend key alone) restores a whole wallet.

**Vector**:

    keccak(spend_secret) 5146398cf960e8705f27bd61413037857d6eb51457125bb9c90339b752f23720
    view_secret          779e4dd2c49ac3c0b2edcd1b843c795b7d6eb51457125bb9c90339b752f23700
    view_public          0489cb98c7108372eaff2cdeddc5e76166b017a847537bf8499d61465395e942

## Output derivation (one-time addresses)

Sender has a random transaction key pair `(r, R = r·G)`; receiver has spend
keys `(b, B)` and view keys `(a, A)`.

`generate_key_derivation(A, r)` (`crypto.cpp:115`):
`D = 8·(r·A)`, compressed. The receiver computes the same `D = 8·(a·R)`.
Returns false if `A` does not decompress. The cofactor multiplication is part
of the value; `D` is never the plain Diffie-Hellman point.

`derivation_to_scalar(D, i)` (`crypto.cpp:133`):
`Hs(D ‖ varint(i))` = `sc_reduce32(keccak(D ‖ varint(i)))`, where `i` is the
output's index within the transaction (0-based, counting every output) and
`varint` is the LEB128 encoding from `04-serialization.md`.

`derive_public_key(D, i, B)` (`crypto.cpp:169`): `P = Hs(D‖varint(i))·G + B`.
Returns false if `B` does not decompress.

`derive_secret_key(D, i, b)` (`crypto.cpp:254`): `x = Hs(D‖varint(i)) + b mod l`.

`underive_public_key(D, i, P)` (`crypto.cpp:294`): `B = P − Hs(D‖varint(i))·G`.
This is what the wallet uses to scan: it recovers the spend public key that
an output was sent to and compares it against the keys it owns. There is no
per-output "is mine" hash; ownership is `underive == one of our spend public
keys`.

The three-argument overloads that take a `suffix` are declared but not used
on this chain.

**Vector** (tx key seed 32 × 0x22):

    tx_secret     487a3668ed5bfd7175e832dc642e64f821222222222222222222222222222202
    tx_public     512e1a2060d978a11a9ce65bbb6b98dcf3300b762c520b13fe2e658b583593bf
    derivation    5d450058695ab466965468876364cb788bc4656123bc40479246d23d24b2c9dd   (both sides)
    index 0: scalar cc75e88978b27e2d1f864cd797ed65e20e2e44f10a99018c6f2e3c676973b40c
             P      22c90af32cfded17237a447122de92e736b59313f635bf43f69bacfad3756723
             x      f0b2033e6f607de659fa6545ca0498de1f3f55021caa129d803f4d787a84c50d
    index 1: P      7bff8bd6445e8ebb0772d0e19e5c6a533b45a2594a58c12797d5a20e607f55c4
    index 300 (varint ac02): P 0feba64cf7934aed7a41dbb702f9c87b3f9ad723e37d93b419659f270eec091e

## Hash to point and key images

`hash_to_ec(P)` (`crypto.cpp:418`): `Hp(P) = 8 · fromfe(keccak(P))`, where
`fromfe` is `ge_fromfe_frombytes_vartime`. `hash_data_to_ec` is the same over
arbitrary bytes.

`generate_key_image(P, x)` (`crypto.cpp:453`): `I = x · Hp(P)`. `P` is the
one-time public key of the output being spent and `x` its one-time secret.
The key image is what the chain uses to detect double spends: it is
deterministic for an output and reveals nothing about which ring member is
real.

**Vectors**:

    hash_to_scalar('abc')  9ab38d0681b95fef6d619d1cace05a14c0d1e6e33a64a036ec44f58fa12d6c05
    hash_to_ec('abc')      5697a435347c8d6f988ba157c69e7825c1ede8abf00ceb74c0c45bea8d1d85ba
    Hp(spend_public)       17578ae6fcb167f76bcdbeece316b383e13a164e2daca2ed2397c2aebd22b007
    key image, index 0     4b8483b5c810b23cb58ec80547f3fa43fd587e5c5f53f29e61ee5e949df7e7f4
    key image, index 1     5ce813e86628317e0638bbd3513a1db4e5c826f4b23da46d0395445fa8a97d54
    key image, index 300   2091d3c30087b73a7f77e65813111f8b52777eb93949c411f6fd6656b735ba2b

## Key image domain check (consensus)

`ValidateTransaction::validateTransactionInputs` (`ValidateTransaction.cpp:260`)
requires for every key input that `scalarmultKey(I, L) == I_identity`, i.e.
`l·I` equals the encoding `0100…00` (the identity point). `L` is the group
order as a 32-byte scalar and `scalarmultKey` (`crypto.cpp:429`) is a plain
`ge_scalarmult` with no canonicality check. A key image outside the prime
order subgroup is rejected. `checkRingSignature` repeats the check with
`ge_check_subgroup_precomp_vartime`.

## Ring signatures (MLSAG-free, original CryptoNote scheme)

`crypto_ops::generateRingSignatures` / `checkRingSignature` (`crypto.cpp:606`,
`crypto.cpp:631`). Inputs: `prefix_hash` (the transaction prefix hash from
`04-serialization.md`), key image `I`, the ring `P_0 … P_{n-1}` of one-time
public keys (the real one at index `s`), and the real one-time secret `x`.

Signature is `n` pairs `(c_i, r_i)` of 32-byte scalars, stored as 64-byte
`Signature` values in ring order.

The challenge is the hash-to-scalar of a buffer laid out exactly as

    struct rs_comm { Hash h; struct { EllipticCurvePoint a, b; } ab[n]; }

that is `prefix_hash ‖ a_0 ‖ b_0 ‖ a_1 ‖ b_1 ‖ … ‖ a_{n-1} ‖ b_{n-1}`, with
`sizeof(rs_comm)` bytes hashed (32 + 64·n, no padding).

Signing (`prepareRingSignatures` then `completeRingSignatures`):

    k = random scalar
    for i in 0..n:
        if i == s:
            a_i = k·G
            b_i = k·Hp(P_s)
        else:
            c_i, r_i = random scalars
            a_i = c_i·P_i + r_i·G          (ge_double_scalarmult_base_vartime(c_i, P_i, r_i))
            b_i = r_i·Hp(P_i) + c_i·I      (ge_double_scalarmult_precomp_vartime(r_i, Hp(P_i), c_i, precomp(I)))
            sum += c_i
    h = Hs(rs_comm)
    c_s = h − sum
    r_s = k − c_s·x                        (sc_mulsub(r_s, c_s, x, k))

Note the argument order of `ge_double_scalarmult_base_vartime(P, a, A, b)`
is `a·A + b·G` and of the precomp variant is `a·A + b·B`; `b_i` is
`r_i·Hp(P_i) + c_i·I`, not the other way round.

Verification (`checkRingSignature`):

    reject if I does not decompress or l·I ≠ identity
    for i in 0..n:
        reject if sc_check(c_i) or sc_check(r_i) fails, or P_i does not decompress
        a_i = c_i·P_i + r_i·G
        b_i = r_i·Hp(P_i) + c_i·I
        sum += c_i
    accept iff Hs(rs_comm) − sum == 0

The two-step split (`prepareRingSignatures` returns `k`,
`completeRingSignatures` consumes it) exists so a wallet without the spend
key can prepare and a signer can finish; the on-wire result is identical.

There are no fixed vectors for signing because `k` and the decoy scalars are
random. Test verification against real mainnet transactions instead: every
transaction returned by `/getrawblocks` (`09-rpc-and-wallet-sync.md`) has ring
signatures that MUST verify against the ring members resolved through the
global output indexes in `11-storage.md`. Sign-then-verify round trips test
the signer.

## Plain signatures

`generate_signature` / `check_signature` (`crypto.cpp:354`, `crypto.cpp:386`)
implement a Schnorr-style signature over

    struct s_comm { Hash h; EllipticCurvePoint key; EllipticCurvePoint comm; }

`c = Hs(h ‖ pub ‖ k·G)`, `r = k − c·sec`. Verification recomputes
`comm = c·pub + r·G`. These are not used by transactions on this chain
(ring signatures are), but the wallet API's proof features and older tooling
use them.

## Deterministic subwallets

`generate_deterministic_subwallet_key(baseSpend, index)` (`crypto.cpp:731`):

    tmp = baseSpend (32 bytes) ‖ 8 zero bytes            (40-byte buffer)
    repeat index times:
        tmp[32..40) = index as uint64 little-endian
        tmp[0..32)  = keccak(tmp[0..40))
    subSpend = sc_reduce32(tmp[0..32))

Index 0 is the primary wallet itself and is never derived. The loop runs
`index` times, so subwallet 5 costs five hashes. The public key is
`subSpend·G`. All subwallets share the primary view key.

**Vectors**:

    index 1  secret 2c7d88e6b43bb83f7215ecc744e73589d8d1a841e7ab8f26672c5490c1aa2b0a  public 2c1c4f98aed340fd311ab7d1fe51a1c2e879fde0eb74695e3d10b33d62cc5086
    index 2  secret b5f66b6627238ac68d776a1319f785243feb071a3a11cf611c6bc69e0b40a20e  public 560f3e3b47ffd155f6a42b9764464b751f63f2269dc22e775dc67c543097c456
    index 5  secret a4ddafa869de6372de50b571f2d8aa6f99de80d2da2ef7bf0b84fcc878026c08  public 4d4dffbb254aba5cbeed60ac4bab40ae295cc806517f5047ed0c8bb6110879bd

## Encrypted short payment ids

`Utilities::encryptPaymentId` (`PaymentIdEncryption.cpp:16`). An 8-byte
payment id is XORed with the first 8 bytes of

    keystream = keccak(D ‖ 0x8d)

where `D` is the key derivation between the transaction key and the
receiver's view key (sender: `8·r·A`; receiver: `8·a·R`), and `0x8d` is the
`ENCRYPTED_PAYMENT_ID_TAIL` domain byte. Encryption and decryption are the
same operation. This is Monero's scheme with the same tail byte.

**Vector** (derivation from the output derivation section):

    keystream                       520bf37ff39f9f08c845ebe51eef6d95d022a0a4af92a01b90942cf0007148d6
    encrypt(0102030405060708)       5309f07bf6999800

Rules the wallet applies (`10-wallet.md`): a short payment id may only be
sent to a single destination, a wallet never decrypts a short id on a
transaction it sent itself, and long (32-byte) ids are plaintext.

## Wallet file encryption

`WalletCrypto` (`src/crypto/WalletCrypto.h`, implemented with the bundled
`sha256.c` and `aes_cbc.c`):

- key derivation: PBKDF2-HMAC-SHA256, 16-byte random salt, 500,000
  iterations, 16-byte key;
- cipher: AES-128-CBC with PKCS#7 padding;
- IV: the salt itself (this is a fixed property of every wallet on disk);
- the wallet API password hash uses the same derivation with 10,000 iterations.

A decryption failure of any kind MUST be reported as "wrong password", never
as a padding error (padding oracle). The file layout is in `10-wallet.md`.

**Vectors**:

    deriveKey('password', salt 00..0f, 500000, 16)  c21c37b6728f17a31765f45fecd1e583
    deriveKey('password', salt 00..0f, 10000, 16)   eb6c81535592203c092b158f8d390967
    encrypt('hello wallet', key, iv=salt)           7f97edf047d16b130baebd5d15e88ac3

## ChaCha8 (legacy only)

`Crypto::chacha8` (`chacha8.h`) with a 32-byte key and 8-byte IV encrypts
the pre-2019 `WalletGreen` container. Its key is
`cn_slow_hash_v0(password)[0..32)` (`generate_chacha8_key`). Needed only to
open and convert old files.

**Vectors**:

    chacha8(key 00..1f, iv 00..07, 'hello wallet')  2884c68673a44ccb44ddebc3
    generate_chacha8_key('password')                 d525a9cdd04547c1bd8d9ee13c2d53a9e059ca8a6f0fdc94745c3a9776827d14

## Randomness

`Random::randomBytes` (`src/crypto/random.h`) wraps the platform CSPRNG. It
is used for key generation, ring signature nonces, decoy scalars, the wallet
salt and the P2P peer id. A port MUST use a cryptographic generator for all
of these; a deterministic nonce in `generateRingSignatures` leaks the spend
key exactly as a repeated ECDSA nonce would.

## Acceptance for this document

1. All "keys", "deterministic subwallets", "output derivation",
   "hash_to_scalar / hash_to_ec", "encrypted short payment id", "chacha8" and
   "wallet file crypto" vectors in `vectors/primitives.txt` reproduce.
2. `x·G == P` and `underive(P) == spend_public` hold for indexes 0, 1, 300.
3. Ring signature verification passes on every transaction in
   `vectors/mainnet_rawblocks_302401_v5.json` once the ring members are
   resolved (this requires stage 3 storage or a daemon query; for stage 1,
   a sign-then-verify round trip with random rings of sizes 1, 2, 4 and 8 is
   sufficient).
4. A signature produced by the port verifies with the C++ `checkRingSignature`
   and vice versa (link the C++ library in a test, as `vectors/primitives.cpp`
   does).
