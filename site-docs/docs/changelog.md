# Changelog

The notable changes to WrkzCoin (Rust), newest first, grouped by version; the full history is the repository's [commit log](https://github.com/wrkzcoin/wrkzcoin-rust/commits/development).

The C++ WrkzCoin's own changes are in its [changelog](https://docs.wrkz.work/changelog/daemon/).

## 1.0.1

The workspace version since 2026-09-25; not tagged yet. Every program's `--version` reports it.

### Wallets

- **Sync keeps pace with its downloads.** A sync step no longer downloads another batch while a downloaded one is still waiting to be applied, so a first sync stops growing its store by 500 blocks a round and a batch costs one request.
- **No more standing still at the tip.** The command-line wallet, `wrkz-wallet-api` and `wrkz-service` refreshed the daemon's `/info` every forty rounds rather than on the clock, so a wallet could sit for minutes behind a block the daemon already had. `/info` is now refreshed every ten seconds, and a block the daemon hands over raises the height the wallet holds for it.
- **Global indexes in one request a chunk**, with adjacent windows merged, instead of one round trip for every block paying the wallet: a pool's wallet made hundreds a chunk. A 429 or a dropped connection there no longer spends the retries and leaves an input unspendable; the chunk waits and is tried again.
- **No holes in a sync.** A block that could not be applied dropped the rest of its chunk and sync carried on after it. The downloaded blocks are now discarded and fetched again from the last block applied.
- **`--sync-windows`** on `wrkz-wallet`, `wrkz-wallet-api`, `wrkz-wallet-sync` and `wrkz-service`: far below the tip, ask the daemon for four height windows a round instead of one batch. Off by default, because a public node may rate limit it. It works only with coinbase scanning off, and now says so rather than stopping after the first window.
- **`--sync-max-blocks`** on the same four programs raises the batch ceiling up to 10000, for a daemon started with a higher `--rpc-max-block-count`.
- `wrkz-wallet-api` no longer copies the whole wallet after every sync step: while it catches up it publishes at most once a second, and at once for a transaction, a fork or reaching the tip.
- **Tab completes a command name** at every prompt: the daemon console, `wrkz-node attach` and each of the wallet's prompts, which offer exactly the commands they accept.

### Rust Pluton Wallet

- The sync line shows the scanning rate and the time left.
- Settings gained **Faster first sync**, which skips coinbase transactions so the daemon can skip whole empty blocks. Off by default, because a wallet that is mined to would never see a reward arrive; the screen says so.
- Settings shows the node and the proof-of-work server actually in use, rather than the platform's defaults.
- Every secret the wallet shows (the seed at creation, and the seed and both keys behind the password) has a button that selects the whole of it.
- The Android package's version code is 2, so 1.0.1 installs over 1.0.0.

### Node

- **Seed names are looked up again.** They were resolved once at start-up, so a long-running node dialled addresses that were gone, and a node started before its resolver was up never had a seed at all. They are now re-resolved hourly while the node has few outbound connections, and every five minutes while it holds no seed address.
- **One progress line**, not two that disagreed, with the percentage and the number of peers the chain is being pulled from. It is printed with `--no-console` too, so a node under systemd shows that it is syncing.

### Releases

- The archives now carry **`wrkz-verify-state`, `wrkz-db-inspect` and `wrkz-rpc-diff`** as well: twelve programs instead of nine.
- Release binaries and the Docker image are built with **thin LTO**. It is deterministic, so `SHA256SUMS` still reproduce.

### Testing

- CI now builds every push to `development`; it had been building only pull requests. The dependency audit covers Rust Pluton Wallet's own dependency tree too.
- New fuzz targets for the daemon's HTTP request and JSON body parsers, the wallet file's JSON, the database records `wrkz-replay` and `wrkz-db-inspect` read, the peer state file and the lite snapshot ([Fuzzing](contributing/fuzzing.md)).

## 1.0.0

Tagged 2026-09-14. The first release of the port: the node (`wrkz-node`), the wallet programs (`wrkz-wallet`, `wrkz-wallet-api`, `wrkz-wallet-sync`, `wrkz-wallet-send`), the wallet service (`wrkz-service`), the transaction proof-of-work server (`wrkz-txpow-server`), the import and inspection tools, Rust Pluton Wallet, the protocol specification, fuzz targets, build scripts and CI.

Changes made before the tag:

- **Mixin rules from the daemon's own top block.** The wallets chose the ring size from the height peers claim, so one peer announcing a height past 4,300,000 before the fork would have put every send on the new tier, and the daemon's pool would have refused them all until the fork arrived. The default mixin, the check before a send and every rule of a sweep now use the daemon's top block, as the C++ does since its commit `0b58b035`.
- **Wallets save on Ctrl-C, SIGTERM and SIGHUP.** `wrkz-wallet`, `wrkz-wallet-api` and `wrkz-service` used to end on a signal without saving, so `systemctl stop` or `docker stop` lost whatever had been synced or sent since the last save.
- **Line editing at the consoles.** The node console, `wrkz-node attach` and the wallet's command prompt recall the last hundred commands with Up and Down and edit the line in place. Only the wallet's command prompts use the editor, so no address, amount or seed is kept for Up to recall.
- **Licensed as the C++ WrkzCoin is**: GPL-3.0-or-later, with the C++ repository's own `LICENSE`. Rust Pluton Wallet uses Slint under its GPL-3.0 licence ([Licence](licence.md)).
- CI pinned to one runner image and one toolchain, with a weekly dependency audit and Dependabot.
