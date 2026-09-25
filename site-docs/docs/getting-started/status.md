# Project status

WrkzCoin (Rust) is not ready for production; this page lists what has been checked against real data so far, what has not, and which C++ programs are not ported.

The port follows the protocol specification in [`spec/`](../spec/index.md), which was taken from the C++ reference, [wrkzcoin/wrkzcoin](https://github.com/wrkzcoin/wrkzcoin) at commit `8d89d7bf`, and it is tested against that code.

## Not ready for production

Nothing here should serve a public wallet or accept mining until the block-by-block dual run against the live C++ daemon (`scripts/dual-run.py`) has passed. That run is the last step of the daemon stage of the [roadmap](../spec/12-roadmap.md#stage-3-daemon): keep a port node and a C++ node side by side for several weeks and compare them at every block. [Testing](../contributing/testing.md#dual-run) describes the tool.

## What is proven so far

The tests replay every vector in `spec/vectors/`, and these results came from real data rather than fixtures:

- block templates match the live C++ daemon's `getblocktemplate` field for field, including the whole blob with the random keys masked;
- the node synced thousands of blocks from real mainnet peers over Levin, and serves a C++-shaped client that dials in;
- wallet files written here open in the C++ `wrkz-wallet` CLI and re-export byte-identically, and files it writes open here;
- transactions built by the wallet pass our port of the C++ validator with the rings resolved against real chain outputs and every signature checked;
- the storage layer opened a real 4.2 million block C++ database and verified its headers, proofs of work and ring signatures;
- the 4,300,000 fork (rings of up to eight) is tested at the block level: blocks up to and including 4,300,000 refuse a ring of three, 4,300,001 takes it, and the pool agrees with the next block at every height.

## Not yet

- the dual run against the C++ daemon;
- a block mined by a stock xmrig through the stratum port;
- a cross-compiled build run against the chain.

## Not ported

The C++ documentation's [Other tools](https://docs.wrkz.work/guides/other-tools/) describes the C++ side of each.

| C++ program | Status |
| --- | --- |
| `miner` | The node's stratum port is for xmrig instead ([Mining](../node/mining.md)) |
| `wrkz-netmon` | Not ported |
| `wallet-upgrader` | Not ported. `wrkz-service` opens the modern wallet file, not WalletGreen containers; a WalletGreen container needs the C++ `wrkz-walletupgrader` first ([Wallet service](../wallets/service.md)) |
| `cryptotest` | Not ported |
| `wallet_capi` | The 57-function C library is not written yet rather than dropped: it is stage 2, step 5 of the [roadmap](../spec/12-roadmap.md#stage-2-wallet-library-and-c-api) |

The command-line programs this port adds that the C++ does not have (`wrkz-replay`, `wrkz-verify-state`, `wrkz-db-inspect`, `wrkz-p2p-probe`, `wrkz-rpc-diff`, `wrkz-wallet-sync` and `wrkz-wallet-send`) are listed on [Installing a release](install.md#what-each-archive-holds), and the wallet with a window has its own page, [Rust Pluton Wallet](../wallets/pluton.md).
