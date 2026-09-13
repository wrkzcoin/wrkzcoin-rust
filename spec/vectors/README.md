# Conformance vectors

Everything in this folder was produced by the C++ code at commit
`8d89d7bf` or fetched from the live network on 2026-09-09. A second
implementation must reproduce all of it byte for byte.

| File | Produced by | Used by |
| --- | --- | --- |
| `primitives.txt` | `primitives.cpp` linked against the C++ static libraries | `02-hashing.md`, `03-crypto-primitives.md`, `05-addresses-keys-mnemonics.md` |
| `blocks.txt` | `blocks.cpp`, same | `04-serialization.md`, `07-blocks-consensus.md` |
| `mainnet_rawblocks_0_to_5.json` | `POST /getrawblocks` on `node-fin.wrkz.work:17856`, blocks 0–5 (major versions 1, 1, 2, 3, 4, 4) | 02, 04, 07 |
| `mainnet_rawblocks_302401_v5.json` | same, block 302,401 (v5, with one real transaction) | 02, 04, 06 |
| `mainnet_rawblocks_600001_v6.json` | same, block 600,001 (v6) | 02, 04 |
| `mainnet_rawblocks_1000001_v7.json` | same, block 1,000,001 (v7) | 02, 04 |
| `mainnet_rawblocks_4213000_v7.json` | same, block 4,213,000 (v7, recent) | 02, 04 |
| `mainnet_getwalletsyncdata_4213000.json` | `POST /getwalletsyncdata` for 4,213,000–4,213,001 | 09, 10 |
| `mainnet_getrandom_outs.json` | `POST /getrandom_outs` amounts 10000 and 50000, 3 each | 09, 10 |
| `mainnet_get_global_indexes_for_range.json` | `POST /get_global_indexes_for_range` 4,213,000–4,213,001 | 09, 10 |
| `mainnet_rawblocks_4213648_to_4213650_v7.json` | `POST /getrawblocks`, blocks 4,213,648–4,213,650 (live tip on 2026-09-09; the last one carries a real wallet transaction: fee 70, ring size 2, PoW nonce) | 02, 04, 06 |
| `mainnet_headers_4213588_to_4213650.json` | `getblockheaderbyheight` for 63 consecutive blocks ending at the tip; pins LWMA-2 (`nextDifficultyV5`) against the live chain | 07 |

Block headers (hash, difficulty, nonce, timestamp, reward) for the same
blocks are tabulated in `../09-rpc-and-wallet-sync.md`.

## Regenerating the harness output

Both harness programs compile against the static libraries a normal build
of the C++ repository leaves under `build/`. On Windows with Visual Studio
2022, from the repository root after a Release build:

    cl /nologo /O2 /EHsc /std:c++17 /DA2_VISCTL ^
       /I include /I src /I src\platform\msc ^
       /I external\argon2\include /I external\nlohmann-json ^
       /Fe:primitives.exe spec\vectors\primitives.cpp ^
       /link build\src\Release\Crypto.lib build\src\Release\Common.lib ^
             build\src\Release\Mnemonics.lib build\src\Release\Errors.lib ^
             build\external\argon2\Release\argon2.lib advapi32.lib

    cl /nologo /O2 /EHsc /std:c++20 /DA2_VISCTL ^
       /I include /I src /I src\platform\msc /I src\platform\windows ^
       /I external\argon2\include /I external\nlohmann-json ^
       /I external\cpp-httplib /I external\rocksdb\include ^
       /Fe:blocks.exe spec\vectors\blocks.cpp ^
       /link build\src\Release\CryptoNoteCore.lib build\src\Release\Serialization.lib ^
             build\src\Release\Logging.lib build\src\Release\Logger.lib ^
             build\src\Release\Utilities.lib build\src\Release\Errors.lib ^
             build\src\Release\SubWallets.lib build\src\Release\Config.lib ^
             build\src\Release\System.lib build\src\Release\Common.lib ^
             build\src\Release\Crypto.lib build\src\Release\Mnemonics.lib ^
             build\external\argon2\Release\argon2.lib ^
             build\external\rocksdb\Release\rocksdb.lib ^
             build\external\zstd\build\cmake\lib\Release\zstd_static.lib ^
             advapi32.lib ws2_32.lib shlwapi.lib rpcrt4.lib

`A2_VISCTL` makes the argon2 header declare its functions for static
linking. On Linux or macOS use the corresponding `.a` files under `build/`
with `g++ -std=c++20` and the same include paths, replacing
`src/platform/windows` with `src/platform/linux` or `src/platform/osx`.

The same technique (a small program linked against the C++ libraries) is
how a port should build its own conformance harness: feed random inputs to
both implementations and compare.

## Fetching more mainnet data

    curl -s -H "Content-Type: application/json" \
      -d '{"startHeight":H,"startTimestamp":0,"blockHashCheckpoints":[],"blockCount":N,"skipCoinbaseTransactions":false}' \
      http://node-fin.wrkz.work:17856/getrawblocks

    curl -s -H "Content-Type: application/json" \
      -d '{"jsonrpc":"2.0","id":"1","method":"getblockheaderbyheight","params":{"height":H}}' \
      http://node-fin.wrkz.work:17856/json_rpc

Any synced node answers the same; the seed node is public and rate limited
to 240 requests per minute per address.
