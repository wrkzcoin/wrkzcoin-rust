# 02 - Hashing and proof of work

Source files: `src/crypto/keccak.c`, `hash.c`, `hash-ops.h`, `hash.h`,
`tree-hash.c`, `slow-hash-common.h`, `slow-hash-portable.c` (reference),
`slow-hash-x86.c` and `slow-hash-arm.c` (accelerated, must give identical
output), `aesb.c`, `oaes_lib.c`, `variant2_int_sqrt.h`, `hash-extra-*.c`,
`blake256.c`, `groestl.c`, `jh.c`, `skein.c`, `src/common/CheckDifficulty.cpp`.
Vectors: `vectors/primitives.txt`, sections "cn_fast_hash" through
"check_hash". The harness that produced them is `vectors/primitives.cpp`.

This is stage 1. Nothing else can be tested until these reproduce.

## Keccak as CryptoNote uses it

`keccak(in, inlen, md, mdlen)` in `keccak.c:76` is the original Keccak
(pre-SHA-3 padding), Keccak-f[1600] with 24 rounds:

- rate `rsiz` = 136 bytes when `mdlen == 200` (full state output) or
  `200 − 2·mdlen` otherwise; both callers here give 136;
- absorb full 136-byte blocks by XOR into the state as little-endian u64
  lanes, permuting after each;
- padding: copy the tail, append one byte `0x01`, zero-fill to 136, then
  OR `0x80` into the last byte of the block (byte 135), absorb, permute once;
- output the first `mdlen` bytes of the state.

Two entry points:

- `cn_fast_hash(data, len)` = first 32 bytes. This is "Keccak-256", **not**
  SHA3-256 (SHA-3 pads with `0x06`). Every hash in this protocol that is
  not a proof of work is this function.
- `keccak1600` / `hash_process` = the full 200-byte state, used as the
  starting state of CryptoNight.

**Vectors** (`cn_fast_hash`):

    ""                       c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470
    "abc"                    4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45
    44-byte ascii (I2)       af6fe96f8cb409bdd2a61fb837e346f1a28007b0f078a8d68bc1224b6fcfcc3c
    bytes 00..4b (I3)        73f7c43d64b877a25614f8b8dfbd31ed4f0f22b97e0236d2a2070774434ca3a1
    bytes 00..c7 (I4, 200 B) bfb0aa97863e797943cf7c33bb7e880bb4543f3d2703c0923c6901c2af57b890

I4 is longer than one rate block, so it exercises the absorb loop.

## Tree hash (Merkle root of transaction hashes)

`tree_hash(hashes, count)` in `tree-hash.c:18`. Let `H(x‖y)` be
`cn_fast_hash` over 64 bytes.

- `count == 1`: root = the hash itself.
- `count == 2`: root = `H(h0 ‖ h1)`.
- otherwise let `cnt` be the largest power of two strictly less than `count`.
  Build an array `ints` of `cnt` entries: the first `2·cnt − count` leaves are
  copied unchanged; the remaining leaves are hashed in adjacent pairs,
  `ints[j] = H(h[i] ‖ h[i+1])` for `j` from `2·cnt − count` up to `cnt`.
  Then while `cnt > 2`: halve `cnt` and set `ints[j] = H(ints[2j] ‖ ints[2j+1])`.
  Finally root = `H(ints[0] ‖ ints[1])`.

This is Monero's tree hash. Leaf 0 is always the coinbase transaction hash;
leaves 1..n are the block's transaction hashes in block order. `tree_branch`
and `tree_hash_from_branch` (`tree-hash.c:74`, `tree-hash.c:111`) are the
Merkle-branch equivalents used by the merge-mining header
(`04-serialization.md`); with `depth == 0` the branch function returns the
leaf unchanged, which is the only case that occurs on this chain.

**Vectors** (leaf[i] = `cn_fast_hash` of the single byte `i`):

    leaf[0]            bc36789e7a1e281436464229828f817d6612f7b477d66591ff96a9e064bcc98a
    tree_hash(count=1) bc36789e7a1e281436464229828f817d6612f7b477d66591ff96a9e064bcc98a
    tree_hash(count=2) 57d772147cdf27f5f67d679f0f3a513f8b87622ce598a3cf0b048ab178ddfc6e
    tree_hash(count=3) 31ea648480acca9d46c5cfd2fd5ecf576ce7a797bdd582869c38deeacf6d17d4
    tree_hash(count=4) dd5115b5dcca3db0bffa31064a0d21f21362cd02e1263e47d69e38bbeec1d359
    tree_hash(count=5) 3b85b9b4e7171846e3dd41d242f99cdc136467ff276a272d5d8f960b2c447d67
    tree_hash(count=7) 6db3924fa166ddef0003d700474beb10c7cd9cc90b882af3b1bbb98aeb557a5f
    tree_hash(count=8) 791521f02a712f28265f5200914f9772b133bc2692260f8c8f426e176b1713ed
    tree_hash(count=9) 6a31a9bc64f694b411012bf9293fbf312a418c49565fcee0b0125c5c768c77be

All nine leaves are listed in `primitives.txt`.

## The CryptoNight family

One C function serves four of the five proofs of work
(`hash-ops.h:64`):

    void cn_slow_hash(const void *data, size_t length, char *hash,
                      int light, int variant, int prehashed,
                      uint32_t page_size, uint32_t scratchpad,
                      uint32_t iterations, uint64_t mask);

`prehashed` is always 0 on this chain. The wrappers in `hash.h` fix the
other parameters:

| Wrapper (`hash.h`) | Used for | light | variant | scratchpad bytes | iterations | mask |
| --- | --- | --- | --- | --- | --- | --- |
| `cn_slow_hash_v0` (line 97) | block major 1–3, legacy wallet key | 0 | 0 | 2,097,152 | 1,048,576 | `0x1FFFF0` |
| `cn_lite_slow_hash_v1` (line 158) | block major 4 | 1 | 1 | 1,048,576 | 524,288 | `0xFFFF0` |
| `cn_turtle_lite_slow_hash_v2` (line 357) | block major 5 | 1 | 2 | 262,144 | 131,072 | `0x1FFF0` |
| `cn_upx` (line 372) | block major 7, transaction PoW | 2 | 2 | 131,072 | 32,768 | `0x1FFF0` |

`page_size` only governs allocation and equals the scratchpad size except
for the lite variant (page 2 MiB, scratchpad 1 MiB); it never changes the
result. The mask is always `scratchpad − 16`, i.e. it selects a 16-byte
aligned offset inside the scratchpad.

`light` has exactly one effect on the result: when `light == 2` (UPX) and
`variant == 2`, the variant-2 shuffle writes its three chunks in a different
order (below). `light == 1` changes nothing beyond what the scratchpad and
iteration parameters already change. External miners know `cn_upx` as
`cn/upx2` and `cn_turtle_lite_slow_hash_v2` as `cn-pico/trtl`.

### Algorithm (`slow-hash-portable.c:110`)

Notation: `S` is the scratchpad, `S[j]` the 16-byte block at byte offset
`j`; `u64(x)` reads 8 bytes little-endian; `AES_expand(k)` is the standard
AES-256 key schedule of a 32-byte key (`oaes_key_import_data`), of which the
first 10 round keys are used; `pseudo_round(x, K)` applies ten AES *forward
rounds* (SubBytes, ShiftRows, MixColumns, AddRoundKey with round key `i`)
for `i = 0..9` — no initial key addition and MixColumns in every round
(`aesb.c:166`); `single_round(x, k)` is one such round with the 16-byte key
`k` (`aesb.c:155`).

    1. state = keccak1600(data)                       200 bytes
       text  = state[64..192)                          128 bytes (INIT_SIZE_BYTE)
       K1    = AES_expand(state[0..32))
       variant 1: require length >= 43 (abort otherwise);
                  tweak1_2 = u64(state[192..200)) XOR u64(data[35..43))
       variant 2: b[16..32) = state[64..80) XOR state[80..96)   (lanes w8^w10, w9^w11)
                  division_result = u64(state[96..104))          (w12)
                  sqrt_result     = u64(state[104..112))         (w13)

    2. scratchpad fill: for i in 0 .. scratchpad/128:
           for j in 0..8: text[16j..16j+16) = pseudo_round(text[16j..], K1)
           S[128·i .. 128·i+128) = text

    3. a = state[0..16)  XOR state[32..48)
       b = state[16..32) XOR state[48..64)

    4. main loop, iterations/2 times:
           j  = u32(a[0..4)) & mask
           c1 = single_round(S[j], a);   S[j] = c1
           variant 2: shuffle_add(S, j)                          (see below)
           S[j] = S[j] XOR b[0..16)
           variant 1: t = S[j][11]; S[j][11] = t XOR ((0x75310 >> ((((t>>3)&6)|(t&1))<<1)) & 0x30)

           j  = u32(c1[0..4)) & mask
           c  = S[j]
           variant 2: integer_math(c, c1)                        (see below)
           d  = mul128(u64(c1[0..8)), u64(c[0..8)))  stored as  d[0..8) = hi, d[8..16) = lo
           variant 2: S[j^0x10] ^= d;  d ^= S[j^0x20]            (in that order, 16 bytes each)
           variant 2: shuffle_add(S, j)
           a  = a + d  (two independent u64 lane additions, wrapping)
           S[j] = a;   variant 1: S[j][8..16) ^= tweak1_2   (applied to the value written)
           a  = a XOR c
           variant >= 2: b[16..32) = b[0..16)
           b[0..16) = c1

    5. text = state[64..192);  K2 = AES_expand(state[32..64))
       for i in 0 .. scratchpad/128:
           for j in 0..8: text[16j..] = pseudo_round(text[16j..] XOR S[128·i + 16·j], K2)
       state[64..192) = text
       keccakf(state, 24)
       result = finalizer[state[0] & 3](state, 200)  →  32 bytes

The "a = c ^ (a+d)" step above is what the portable code writes as
`sum_half_blocks(a, d); swap_blocks(a, c); xor_blocks(a, c); copy_block(p, c)`
(`slow-hash-portable.c:202-206`): the value stored back into `S[j]` is the
new `a + d` (with the variant-1 tweak on its upper half), and the register
`a` becomes `c_old XOR (a + d)`.

**Finalizers** (`extra_hashes`, index `state[0] & 3`): 0 → BLAKE-256
(`blake256.c`, 14 rounds), 1 → Grøstl-256 (`groestl.c`), 2 → JH-256 (`jh.c`),
3 → Skein-512 with a 256-bit output (`skein.c` NIST reference,
`skein_hash(256, ...)`; `SKEIN_256_NIST_MAX_HASHBITS` is 0 there, so the
512-bit state is selected). Each hashes the full 200-byte state. The five vectors per function below exercise more
than one finalizer; a port should additionally test each finalizer directly
against the reference C.

**variant 2 shuffle_add(S, j)** (`VARIANT2_PORTABLE_SHUFFLE_ADD`,
`slow-hash-common.h:145`): with `chunk1 = S[j^0x10]`, `chunk2 = S[j^0x20]`,
`chunk3 = S[j^0x30]`, `b0 = b[0..16)`, `b1 = b[16..32)`, `a0 = a`, and every
`+` a pair of independent u64 lane additions:

    standard (light != 2):        UPX (light == 2):
      new chunk1 = chunk3 + b1      new chunk1 = chunk1 + b1
      new chunk2 = chunk1 + b0      new chunk2 = chunk3 + b0
      new chunk3 = chunk2 + a0      new chunk3 = chunk2 + a0

using the *old* values of the chunks on the right-hand sides.

**variant 2 integer_math(c, c1)** (`VARIANT2_INTEGER_MATH_DIVISION_STEP`,
`slow-hash-common.h:195`, then `variant2_int_sqrt.h`):

    c[0..8)  ^= division_result ^ (sqrt_result << 32)
    dividend  = u64(c1[8..16))
    divisor   = (u32)( u64(c1[0..8)) + (u32)(sqrt_result << 1) ) | 0x80000001
    division_result = (u32)(dividend / divisor) + ((dividend % divisor) << 32)
    sqrt_input = u64(c1[0..8)) + division_result          (wrapping)
    sqrt_result = floor( sqrt(2^64 + sqrt_input) · 2 − 2^33 )    as a 32-bit value

The square root MUST be exact. The reference integer implementation is
`integer_square_root_v2` (`variant2_int_sqrt.h:55`); the floating point
shortcut is only valid with the `VARIANT2_INTEGER_MATH_SQRT_FIXUP` correction
(`variant2_int_sqrt.h:166`). Sample inputs and outputs are in the header
comment (`2^50 → 262140`, `2^64−1 → 3558067407`). A port should use the
integer version.

`mul128` is the full 64×64→128 unsigned product of the first 8 bytes of
each operand, written high half first (`slow-hash-portable.c:50`).

### Chukwa (block major 6)

`chukwa_slow_hash` (`hash.h:468`):

    salt = data[0..16)
    hash = argon2id(pwd = data, pwdlen = len, salt, saltlen = 16,
                    t_cost = 4, m_cost = 256 KiB, parallelism = 1,
                    taglen = 32, version = 0x13)

It is plain Argon2id 1.3 with the input's own first 16 bytes as salt. The
input MUST be at least 16 bytes (block hashing blobs always are). Any
conformant Argon2 library reproduces it.

### Vectors

Inputs: I0 empty, I1 `616263`, I2 the 44-byte ASCII string
`This is a test This is a test This is a test`, I3 bytes `00..4b`, I4 bytes
`00..c7`. Full hex in `primitives.txt`.

    cn_slow_hash_v0
      I0  eb14e8a833fac6fe9a43b57b336789c46ffe93f2868452240720607b14387e11
      I1  22b72dd4751523b2fa3a46a90dc146dae83d033f0e369ab71a44563f2f18e209
      I2  74d15836e33d14e164c2494648996eb5ed71a3ec2c72c2be225eda1b8a857aba
      I3  f6cb9c11f00543bab31ad730687d5df828118e8e5ed678ff73ae483c23785386
      I4  1adf947fecb24cb051de5e21d20becd0a00d16943b92312ed5f11a0bcc2bcee5
    cn_lite_slow_hash_v1  (inputs shorter than 43 bytes are invalid)
      I2  fca17d4437709b4a3bd71ef3ed21b417ca93dc8679ce81dfd3cbdd0a22d758ba
      I3  f3887e3c44d015abf2e3a431991ba09eb2941e5671ef34a731b6abff59465dba
      I4  fee0656b2bc1a08ec20dd594946f4103ff229f74201e8fd44f45c00774cf1592
    cn_turtle_lite_slow_hash_v2
      I0  16cba4f89786b8aa785a4085f529f757296402aca4edbaefc1470bc691071ed9
      I1  9cbd76fa436fd9a1082e270002d0d10c3d4c229c5cde987e2f71e38d4a9cdead
      I2  305f66febbf3600edabb60f7f1c9b90a3ae85a31d476ca381d5618a6c62760d7
      I3  645a537295c69620af9f203279efd972fbccef508b048e5877039d0f18a6e4ff
      I4  1d6c9b9668266f4694b9b49a69776ca321da2078bab9ac09dade8c5078df8e86
    chukwa_slow_hash  (inputs shorter than 16 bytes are invalid)
      I2  2a37ad80dc974ff1fab8a670baed8bbd56a662919b8c80f21b90595150394bb8
      I3  90f35c9d1e694fde56ee9e2e95eda30000a6d65bde9fa1d304f22602e44fdd27
      I4  9890115d629b57265a78a97b298bb3091f87eec1abe7e8c2da5ad897bee875c2
    cn_upx
      I0  ba2a6fec59d3316c1c33d04189996d4aa56bd5c5eefe7a9cae742a244b906d2d
      I1  f8291063f631e1f0b2b282bb03e61f29624cc316dbf6977389eed4d818d6c67c
      I2  9553131ca973561f010302b89ff59926502378c1c7ee420df5fc60def9c38729
      I3  ed1a7234dfa8a4b614193c8ceedbec80202c6921b83693cc7b4f6bc11d11cc43
      I4  f9bfe6929b7b7a8edeabc7a8f44135c86ed18289685ea58f47259fba42f13438

`cn_slow_hash_v0` of the string `password` is also pinned:
`d525a9cdd04547c1bd8d9ee13c2d53a9e059ca8a6f0fdc94745c3a9776827d14`
(it is the legacy wallet cipher key, `03-crypto-primitives.md`).

Real-chain vectors: every raw block in `vectors/mainnet_rawblocks_*.json`
has a proof of work that satisfies its recorded difficulty. Which blob is
hashed depends on the block version and is defined in `04-serialization.md`;
the difficulties are in the headers in `09-rpc-and-wallet-sync.md`.

## Which function a block uses

`HASHING_ALGORITHMS_BY_BLOCK_VERSION` (`CryptoNoteConfig.h:462`) keyed by the
block's major version; the table is in `01-constants.md`. A node MUST refuse
a block whose major version has no entry (`CachedBlock::getBlockLongHash`
throws for it).

## Difficulty check

`check_hash(hash, difficulty)` (`CheckDifficulty.cpp:44`): interpret the
32-byte hash as a 256-bit little-endian integer `h`; the block is valid iff
`h · difficulty < 2^256`. The C code computes the product word by word and
returns false on any carry out of the top word; the first check is the
top 64-bit word alone, which rejects almost every candidate immediately.

**Vectors**:

    check_hash(keccak("abc"), 1)        true
    check_hash(keccak("abc"), 1000)     false
    check_hash(0100..00, 2^63)          true
    check_hash(ff..ff, 1)               true
    check_hash(ff..ff, 2)               false

## Keeping the C code

Recommended layout for a port, whatever the language:

- vendor `keccak.c`, `hash.c`, `tree-hash.c`, `slow-hash-portable.c`,
  `slow-hash-common.h`, `variant2_int_sqrt.h`, `aesb.c`, `oaes_lib.c`,
  `hash-extra-*.c`, `blake256.c`, `groestl.c`, `jh.c`, `skein.c`,
  `common/int-util.h` and, optionally, `slow-hash-x86.c`/`slow-hash-arm.c`
  for speed, plus the argon2 library;
- expose `cn_fast_hash`, `cn_slow_hash` with the ten parameters above,
  `tree_hash`, `tree_branch`, `tree_hash_from_branch`, and the argon2 call;
- build the wrappers of `hash.h` in the host language from the parameter
  table so the C surface stays minimal;
- note the reference C allocates and frees the scratchpad on every call
  (`slow-hash-x86.c:541`); a port that hashes in a loop (mining, transaction
  proof of work) should keep a per-thread scratchpad. Output is unaffected.

`slow-hash-portable.c` is compiled only when neither the x86-64 nor the ARM
accelerated file applies (`slow-hash-portable.c:12`). All three MUST agree;
the vectors here were produced by the x86-64 file on Windows.

## Acceptance for this document

1. All `cn_fast_hash`, `tree_hash`, `cn_slow_hash_v0`, `cn_lite_slow_hash_v1`,
   `cn_turtle_lite_slow_hash_v2`, `chukwa_slow_hash`, `cn_upx` and
   `check_hash` vectors reproduce.
2. The proof of work of blocks 1, 2, 3, 4, 5, 302,401, 600,001, 1,000,001
   and 4,213,000 in `vectors/` satisfies the difficulty from the headers,
   using the hashing blob rules of `04-serialization.md`. That test spans all
   five functions.
3. A fuzz comparison against the C `cn_slow_hash` (linked in a test) on
   10,000 random inputs of lengths 43..512 for each of the four CryptoNight
   wrappers, if the port reimplements them natively.
