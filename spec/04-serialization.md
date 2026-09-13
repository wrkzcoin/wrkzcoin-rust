# 04 - Serialization, hashing blobs, tx_extra, KV binary

Source files: `src/common/Varint.h`, `src/serialization/BinaryOutputStreamSerializer.cpp`,
`BinaryInputStreamSerializer.cpp`, `CryptoNoteSerialization.cpp` (field order
of every structure), `SerializationOverloads.h`, `SerializationTools.h`,
`KVBinaryCommon.h`, `KVBinaryOutputStreamSerializer.cpp`,
`KVBinaryInputStreamSerializer.cpp`, `src/common/CryptoNoteTools.{h,cpp}`,
`src/cryptonotecore/CachedBlock.cpp`, `CachedTransaction.cpp`,
`src/common/TransactionExtra.cpp`, `src/utilities/ParseExtra.cpp`,
`src/common/CryptoNoteJson.cpp`. Vectors: `vectors/blocks.txt`
(harness `vectors/blocks.cpp`) and the mainnet blobs
`vectors/mainnet_rawblocks_*.json`.

There are three encodings: the **binary** format (blocks and transactions,
what gets hashed), the **KV binary** "portable storage" format (P2P
payloads, the database, the peer state file), and **JSON** (RPC, wallet
files). Get the first one exactly right before anything else.

## varint

Unsigned LEB128 (`Varint.h:20`): emit the low 7 bits with the high bit set
while the remaining value is `>= 0x80`, then the last byte with the high
bit clear. Reading (`read_varint`) rejects overflow past the target width
and rejects a non-canonical zero continuation byte (`byte == 0 && shift != 0`).

    0 → 00        127 → 7f        128 → 80 01      300 → ac 02
    999730 → b2 82 3d      2^32 → 80 80 80 80 10     2^64−1 → ff×9 01

## Binary format (`BinaryOutputStreamSerializer`)

| C++ call | Bytes |
| --- | --- |
| any integer (`uint8..uint64`, signed cast to unsigned) | varint |
| `bool` | 1 byte, 0 or 1 |
| `std::string` and `binary(std::string)` | varint length, then the bytes |
| `binary(ptr, size)` (fixed size POD: hashes, keys, signatures, nonce) | the raw bytes, no length |
| `beginArray(n)` | varint `n`, then the elements |
| `beginObject` | nothing; fields follow in order |
| variant tag | 1 raw byte |
| `double` | not supported (throws) |

Deserialization MUST consume the whole buffer (`fromBinaryArray`,
`SerializationTools.h:211`); trailing bytes are an error.

### Transaction (`CryptoNoteSerialization.cpp:207-296`)

    TransactionPrefix
      varint version                 (input rejects version > 1)
      varint unlock_time
      varint vin_count
        per input:
          byte  tag                  0xff = BaseInput, 0x02 = KeyInput
          BaseInput:  varint height
          KeyInput:   varint amount
                      varint key_offsets_count, then varint × count
                      32 bytes k_image
      varint vout_count
        per output:
          varint amount
          byte  tag                  0x02 = KeyOutput
          32 bytes key
      varint extra_length, then extra bytes

    Transaction = TransactionPrefix followed by signatures:
      for each input, in order: key_offsets_count × 64-byte signatures, concatenated
      (no count prefixes at all)

Rules for the signatures section (`CryptoNoteSerialization.cpp:237`):

- a transaction whose only input is a `BaseInput` (a coinbase) has no
  signature bytes;
- on output, `signatures.size()` MUST equal `inputs.size()` unless
  `signatures` is empty, in which case every input MUST have zero ring
  members (only a coinbase qualifies);
- on input, the reader expects exactly `key_offsets_count` signatures per
  key input; the byte stream must end exactly there.

`BaseTransaction` (`CryptoNoteSerialization.cpp:222`) is the coinbase of a
merge-mining *parent* block. It differs from `Transaction`: any `version`
is accepted, there are no signature bytes, and when `version >= 2` one
extra `varint ignored` follows `extra` (the C++ always writes 0 on output).
Foreign chains' version-2 coinbases do appear on this chain (block 600,001),
so a port MUST parse this layout. `Transaction` input still rejects any
version above 1.

**Vector** (`vectors/blocks.txt`, "synthetic transaction"; one key input
amount 500 with offsets `[7, 3, 300]`, outputs 400 and 90, extra = pubkey +
nonce with a long payment id, three zero signatures):

    prefix blob  01 00 01 02 f403 03 07 03 ac02 66×32 02 9003 02 71×32 5a 02 76×32 44 01 77×32 02 21 00 88×32
    full blob    prefix ‖ 00×192
    prefix hash  d45c12560672a1f7812a0a201396e55c3fc3b3913d1083540aca4230adb37bcc
    tx hash      3b7b2aa764e6631bfa203acdd6a1d6cc1000b7a9e2701ad92e3e55206b4e81a7
    fee          10

### Transaction hashes (`CachedTransaction.cpp`, `CryptoNoteTools.h`)

- **transaction hash** = `cn_fast_hash(full serialized Transaction)`,
  signatures included (`getTransactionHash`, line 35). No length prefix.
- **prefix hash** = `cn_fast_hash(serialized TransactionPrefix)`; this is
  what ring signatures sign and what transaction proof of work hashes.
- coinbase hash = transaction hash of the coinbase (its serialization has
  no signature bytes).

`getObjectHash(T)` for a structure `T` is `cn_fast_hash(toBinaryArray(T))`.
**But** `getObjectHash` applied to a byte array (`BinaryArray`) hashes the
array *as a string*, i.e. with a varint length prefix
(`SerializationTools.h:194`, the `toBinaryArray<std::vector<uint8_t>>`
specialization). Block ids depend on this; see below.

### Block header and block template (`CryptoNoteSerialization.cpp:455-497`)

    BlockHeader (major_version == 1)
      varint major_version (= 1)
      varint minor_version
      varint timestamp
      32 bytes prev_id
      4 bytes nonce (raw little-endian uint32)

    BlockHeader (major_version >= 2)
      varint major_version
      varint minor_version
      32 bytes prev_id
      (timestamp and nonce are serialized inside the parent block instead)

    BlockTemplate
      BlockHeader
      if major_version >= 2: ParentBlock (hashing = false, headerOnly = false)
      Transaction miner_tx
      varint tx_hashes_count, then 32 bytes × count

`major_version > 7` is rejected on input (`serializeBlockHeader`, line 458).

### Parent block (`ParentBlockSerializer`, `CryptoNoteSerialization.cpp:361`)

Two flags select what is written: `hashing` and `headerOnly`.

    varint  parent.major_version
    varint  parent.minor_version
    varint  block.timestamp                 ← the block's own timestamp
    32 bytes parent.prev_id
    4 bytes  block.nonce                    ← the block's own nonce, raw LE
    if hashing:
        32 bytes merkle_root = tree_hash_from_branch(parent.baseTransactionBranch, depth = branch size,
                                                     leaf = hash of parent.baseTransaction, path = null)
    varint  numberOfTransactions (= parent.transactionCount, MUST be >= 1)
    if headerOnly: stop
    tree_depth(numberOfTransactions) × 32 bytes  baseTransactionBranch      (0 entries when count is 1)
    Transaction parent.minerTx (as a BaseTransaction: prefix only, no signatures)
    (the merge-mining tag MUST be present in minerTx.extra; depth <= 256)
    depth × 32 bytes  blockchainBranch                                        (0 entries when depth is 0)

`merkle_root` is `tree_hash_from_branch(baseTransactionBranch, depth,
getBaseTransactionHash(minerTx))` where `getBaseTransactionHash`
(`CryptoNoteTools.h:81`) is the plain object hash for a coinbase of
version < 2 and, for version >= 2,
`keccak(prefix_hash ‖ keccak(0x00) ‖ 32 zero bytes)`. `tree_depth(n)` is
floor(log2 n), so `numberOfTransactions = 5` gives a 2-entry branch.

Three parent-block shapes exist on chain and all MUST be accepted:

- **daemon template** (`getblocktemplate`, `Core.cpp:2364-2366`): parent
  major **0**, minor 0 (a typo assigns `BLOCK_MINOR_VERSION_0` to
  `majorVersion`), `transactionCount = 1`, empty branch, and a
  default-constructed coinbase: version 0, unlock 0, **no inputs, no
  outputs**, extra = merge-mining tag only, i.e. `00 00 00 00 23 03 21 00
  <root>`. Blocks 2, 3, 4, 5, 4,213,000 and 4,213,648 have this shape;
- **pool software**: parent major 1, minor 0, `transactionCount = 1`, a
  version-1 coinbase `01 00 01 ff 00 00 23 03 21 00 <root>` (unlock 0, one
  `BaseInput{0}`, no outputs). Blocks 302,401, 1,000,001, 4,213,649;
- **genuinely merge-mined**: block 600,001 carries parent major 12, minor
  14, `numberOfTransactions = 5` (branch depth 2), a version-2 foreign
  coinbase with the `ignored` varint after `extra`, *two* merge-mining tags
  in that extra (the first parsed one wins, depth 1) and a one-entry
  `blockchainBranch`.

A port's template builder should copy the daemon-template bytes so its
blocks are indistinguishable from the C++ daemon's.

### Hashing blobs and block identity (`CachedBlock.cpp`)

    headerHashingBlob  = serialize(BlockHeader)              (v1: with timestamp+nonce; v2+: versions + prev_id only)
                       ‖ tree_hash([coinbase hash] + tx_hashes)
                       ‖ varint(tx_hashes_count + 1)

    block id:
      v1:  keccak( varint(len(headerHashingBlob)) ‖ headerHashingBlob )
      v2+: blob = headerHashingBlob ‖ ParentBlock(hashing = true, headerOnly = false)
           keccak( varint(len(blob)) ‖ blob )

    auxiliary header hash (v2+, the value the merge-mining tag commits to):
           keccak( varint(len(headerHashingBlob)) ‖ headerHashingBlob )

    proof-of-work input (no length prefix):
      v1:  headerHashingBlob
      v2+: ParentBlock(hashing = true, headerOnly = true)
           = parent versions ‖ timestamp ‖ parent prev_id ‖ nonce ‖ merkle_root ‖ varint(numberOfTransactions)

    pow hash = HASHING_ALGORITHMS_BY_BLOCK_VERSION[major](powInput)

The length prefix on the block id (and only there and on the auxiliary
hash) is the single most common porting mistake on CryptoNote chains.

**Vectors** (`vectors/blocks.txt`):

Genesis (v1):

    header hashing blob  0100 00 00×32 46000000 c1a9de04…3e95 01            (72 bytes)
    block id             877e55b4e902b9bf4c9e0a7c16440f449339d56679c49d62261ae5c92596a6ce   = keccak(48 ‖ blob)
    pow hash             3cb9405522d3c32293ce88d1c7700f2f27b86fbb3a11663be7f244199dbb5423   = cn_slow_hash_v0(blob)

Synthetic v2 block on top of genesis (timestamp 1529831318, nonce
0x11223344, one fake tx hash `55×32`, coinbase with one output; full blobs
in the file):

    header hashing blob      0200 877e…a6ce 9aa463ab…28b9 02
    aux header hash          934cd35732e548e7387ebcc3613c361f9e8e4d98bf88214753aaab687b9cbbcb
    parent hashing blob (header only, the PoW input)
                             0100 96bfbdd905 00×32 44332211 399dcf2f…d07d 01
    block id                 0db3816e368074ab6709e8e44aec34829e1f690aab1b677d2121a12b7b9d716f
    pow hash                 81148ed0d71d95cfddd8e4e1c416d20b119635a064e4babc869edcbe2ad7e806  (cn_slow_hash_v0)

The same synthetic block is given for every major version 1–7 in the
file; the only differences are the version byte, the merge-mining root
(because the aux hash includes the version) and the proof-of-work function.
Use them to test the version switch.

Mainnet: `vectors/mainnet_rawblocks_0_to_5.json` holds blocks 0–5 (v1, v1,
v2, v3, v4, v4); the other `mainnet_rawblocks_*.json` files hold one v5,
one v6 and two v7 blocks with their transactions. Their ids and difficulties
are in `09-rpc-and-wallet-sync.md`; each proof of work MUST satisfy its
difficulty.

Reading block 2 from that file as a worked example:

    02 00                         major 2, minor 0
    93bb…7b51                     prev_id (block 1)
    00 00                         parent major 0, minor 0 (daemon template shape)
    96bfbdd905                    timestamp 1529831318
    00×32                         parent prev_id
    4bd3902f                      nonce 798020427
    01                            numberOfTransactions
                                  (branch: 0 entries)
    00 00 00 00 23 03 21 00 addfa9…26b6   parent coinbase: version 0, unlock 0,
                                  no inputs, no outputs, 35-byte extra = mm tag
                                  (blockchain branch: 0 entries)
    01 2a 01 ff 02 08 …           miner_tx: version 1, unlock 42, BaseInput height 2, 8 outputs …
    00                            tx_hashes_count

### Other structures

- `AccountPublicAddress`: 32 bytes spend public ‖ 32 bytes view public
  (`CryptoNoteSerialization.cpp:499`). This is the payload inside a base58
  address.
- `KeyPair`: secret ‖ public (`line 540`).
- `RawBlock` in the binary format (`line 547`, used by the database and the
  blockchain dump): `varint block_size ‖ block bytes ‖ varint tx_count ‖
  per tx: varint tx_size ‖ tx bytes`.
- `TransactionExtraMergeMiningTag` on its own: `varint depth ‖ 32 bytes
  merkle_root`, and inside extra it is wrapped as a length-prefixed string
  (`line 520`).

## tx_extra

`extra` is a byte string inside the prefix. The consensus parser is
`parseTransactionExtra` (`TransactionExtra.cpp:23`); the wallet-side parser
`Utilities::parseExtra` (`ParseExtra.cpp:46`) reads the same fields more
loosely. Fields, in the order producers write them:

| Tag | Layout | Notes |
| --- | --- | --- |
| `0x01` pubkey | 32 bytes | at most one; a second one stops parsing |
| `0x02` nonce | 1 byte length `n` (0–255), `n` bytes | at most one; the bytes are sub-tagged below |
| `0x03` merge-mining | varint length, then `varint depth ‖ 32 bytes root` | only in parent coinbases |
| `0x04` tx PoW nonce | 8 bytes | wallet-created transactions; always the last field |
| `0x00` padding | zero bytes to the end, at most 255 | never written by current code |

Exact semantics of the consensus parser `parseTransactionExtra`, which a
port MUST mirror because they decide which merge-mining tag a parent
coinbase commits to:

- padding (`0x00`): zero bytes to the end, at most 255; a non-zero byte or
  a longer run makes the function return `false`, but every caller keeps
  the fields collected before the failure; a second padding field stops
  parsing;
- a second pubkey or a second nonce **stops** parsing (fields so far kept);
- a second merge-mining tag is skipped **without consuming its body**, so
  its bytes are walked as tags;
- there is no default case: any other byte, including the wallet's `0x04`
  PoW nonce tag, is skipped one byte at a time;
- a truncated field is an exception, i.e. `false` with the fields so far.

The wallet-side `Utilities::parseExtra` (`ParseExtra.cpp:46`) is looser
and has its own quirks: a field is recognised wherever its tag appears with
enough bytes left; the nonce length is read as a **varint** (identical to
the 1-byte length below 128); after a nonce the outer cursor advances only
by the sub-fields it recognised; after a merge-mining tag it advances by
`depth varint + 32` but not by the length varint. Both parsers are
implemented in `crates/wrkz-primitives/src/tx.rs`. Consensus only needs
the merge-mining tag from the parent coinbase; nothing else in extra is
validated except its total size (`06-transactions.md`).

Nonce sub-fields (`ParseExtra.cpp:112`), inside the `0x02` payload:

| Sub-tag | Layout |
| --- | --- |
| `0x00` | 32-byte plaintext payment id |
| `0x01`, `0x02` | 8-byte plaintext short payment id (legacy; consumed, never reported) |
| `0x03` | 8-byte encrypted short payment id |
| `0x7f` | varint length, arbitrary data bytes |

A wallet writes extra as: `01 ‖ txpub ‖ [02 ‖ len ‖ nonce payload] ‖ [04 ‖ 8-byte pow nonce]`
(`Transfer.cpp:1487-1510`, `TransactionPoW.cpp:96`). The nonce payload is
the payment id sub-field, if any, followed by the arbitrary data sub-field,
if any. The daemon's coinbase writes `01 ‖ txpub ‖ [02 ‖ len ‖ extra_nonce]`
where the reserved bytes given to a mining pool are the raw `extra_nonce`
without any sub-tag.

## KV binary ("portable storage")

`KVBinaryOutputStreamSerializer.cpp`, `KVBinaryInputStreamSerializer.cpp`,
`KVBinaryCommon.h`. Used for every P2P payload, every database record and
the peer state file. This is Monero's epee portable storage format; the
constants match.

    header:  01 11 01 01   (SIGNATURE_A 0x01011101 LE)
             01 01 02 01   (SIGNATURE_B 0x01020101 LE)
             01            (format version)
    body:    section

    section: kv-varint entry_count, then entries
    entry:   1 byte name length, name bytes, 1 byte type, value
    value by type:
      1 int64, 2 int32, 3 int16, 4 int8, 5 uint64, 6 uint32, 7 uint16, 8 uint8:  raw little-endian
      9 double: 8 bytes            11 bool: 1 byte
      10 string: kv-varint length, bytes
      12 object: section (kv-varint entry count, entries)
      13 array: (not written by this code)
      type | 0x80: array of that type: kv-varint count, then count values with no names

    kv-varint: low 2 bits of the first byte give the size:
      00 → 1 byte, value = byte >> 2                (<= 63)
      01 → 2 bytes LE, value = word >> 2            (<= 16383)
      10 → 4 bytes LE, value = dword >> 2           (<= 2^30 − 1)
      11 → 8 bytes LE, value = qword >> 2

Rules that matter for interoperability (`KVBinaryOutputStreamSerializer.cpp`):

- an **empty array is not written at all** (`checkArrayPreamble` only
  emits the name when the first element arrives, and `endArray` only
  counts the entry when something was written); readers MUST treat a
  missing array field as empty (`readSequence`, `SerializationOverloads.h:315`);
- an **empty binary blob is not written** (`binary(void*, size)` with
  `size == 0` writes nothing); readers get an absent field;
- the C++ types map to KV types by their C++ width: `uint8_t` fields are
  type 8, `uint32_t` type 6, `uint64_t` type 5, `std::string` and every
  fixed POD (hash, key, 16-byte network id, blobs from `serializeAsBinary`)
  type 10, `bool` type 11, nested structures type 12;
- `serializeAsBinary(vector<POD>)` writes the vector's raw memory as one
  string (used for hash lists and peer lists);
- readers are lenient about unknown names (they parse into a JSON-like
  tree first) and about order; writers emit fields in declaration order.
  Values are read by name, so a port may emit in any order but SHOULD keep
  declaration order;
- integers are read by the declared type of the receiving field; a value
  stored with a different width is converted through a 64-bit integer.

**Example**: `COMMAND_PING` response `{ status: "OK", peer_id: 1 }`:

    01 11 01 01 01 01 02 01 01        header
    08                                 2 entries
    06 73 74 61 74 75 73  0a  08 4f 4b     "status" string "OK"
    07 70 65 65 72 5f 69 64  05  01 00 00 00 00 00 00 00   "peer_id" uint64 1

## JSON

`nlohmann::json`. Hashes, keys, key images and derivations are lowercase
hex strings; on input the daemon and wallet also accept base64 of the same
bytes when the string length matches (`CryptoNoteJson.cpp:36`), which is how
the `base64` sync feature works. `KeyInput` is
`{"amount", "key_offsets": [..], "k_image"}` with `key_offsets` optional on
input (`CryptoNoteJson.cpp:118`). `RawBlock` is
`{"block": hex, "transactions": [hex]}`. Amounts and heights are JSON
numbers; `uint64` values above 2^53 are emitted as numbers anyway (the
wallet tolerates `unlockTime` as a string for third-party caches,
`WalletTypes.h:619`).

## Acceptance for this document

1. Round-trip every blob in `vectors/mainnet_rawblocks_*.json` and
   `vectors/blocks.txt`: parse, re-serialize, byte-identical; reject with
   trailing bytes appended.
2. Reproduce the transaction vector's prefix hash and tx hash, the genesis
   block id, and the block ids and pow hashes of every synthetic block in
   `blocks.txt` and every mainnet block (ids from the headers in
   `09-rpc-and-wallet-sync.md`).
3. Parse the extra of every mainnet transaction in the samples and recover
   the tx public key; produce the wallet extra layout for a pubkey, an
   encrypted short id, arbitrary data and a pow nonce and confirm the C++
   `parseExtra` reads all four back (link it in a test).
4. Encode the ping example and the handshake structures in KV binary and
   decode a real handshake captured from a seed node (stage 3), byte for
   byte.
