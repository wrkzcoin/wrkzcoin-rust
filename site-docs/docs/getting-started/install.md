# Installing a release

A release is one archive per platform, each holding every command-line program ready to run, and a `SHA256SUMS` file that covers them all; this page says which archive to take, how to check it and what is in it.

The archives are built on one Linux host by `scripts/release.sh` (see [Cross-compiling](../building/cross-compile.md)). Rust Pluton Wallet, the wallet with a window, is not in them: its archives are named `rust-pluton-wallet-<version>-<commit>-<os>-<arch>` (see [Rust Pluton Wallet](../wallets/pluton.md)).

## Picking an archive

Each archive is named `wrkzcoin-cli-<version>-<commit>-<os>-<arch>`: a `.zip` for Windows, a `.tar.gz` for everything else, for example `wrkzcoin-cli-1.0.0-97f3ab1-linux-arm64.tar.gz`. The commit is the short hash the release was built from, and `wrkz-node --version` prints it too.

| Archive ends in | Runs on |
| --- | --- |
| `linux-x86_64.tar.gz` | Linux x86_64 |
| `linux-arm64.tar.gz` | Linux arm64 (Raspberry Pi 4/5 with a 64-bit OS, Graviton, Ampere), glibc ≥ 2.28 |
| `windows-x86_64.zip` | Windows 10/11 x64, no DLLs needed |
| `macos-x86_64.tar.gz` | macOS 13+ on Intel |
| `macos-arm64.tar.gz` | macOS 13+ on Apple silicon |
| `android-arm64.tar.gz` | Android 7.0+ (Termux, `adb`) |
| `android-x86_64.tar.gz` | Android emulator, x86 Chromebooks |

32-bit targets are not released: nothing in this port has been tested on one, and consensus code that is right on 64 bits is not automatically right on 32.

## Checking the download

Download the archive for your platform and the `SHA256SUMS` file beside it into one directory, then check the archive against it. `SHA256SUMS` lists every archive of the release, so ask the checker to skip the ones you did not download:

=== "Linux"

    ```sh
    sha256sum --check --ignore-missing SHA256SUMS
    ```

=== "macOS"

    ```sh
    shasum -a 256 --check --ignore-missing SHA256SUMS
    ```

=== "Windows"

    ```powershell
    (Get-FileHash .\wrkzcoin-cli-<version>-<commit>-windows-x86_64.zip -Algorithm SHA256).Hash.ToLower()
    Select-String windows-x86_64.zip .\SHA256SUMS
    ```

    The two hashes must be the same.

When the release was signed, `SHA256SUMS.asc` is a detached GPG signature of `SHA256SUMS` (`scripts/release.sh` writes it when `WRKZ_SIGN_KEY` names a key). Check it before trusting the sums:

```sh
gpg --verify SHA256SUMS.asc SHA256SUMS
```

The build is reproducible: two hosts building the same commit with the same `rustc` (1.98.1, which `scripts/release.sh` checks) and the same pinned cross tools produce the same `SHA256SUMS`. Anyone can rebuild a release from its commit and compare; see [Cross-compiling](../building/cross-compile.md).

## What each archive holds

Unpacking an archive gives one directory with the archive's name, holding the programs and the repository's `LICENSE`:

| Program | What it is |
| --- | --- |
| `wrkz-node` | The daemon, with the RocksDB engine |
| `wrkz-wallet` | The command-line wallet |
| `wrkz-wallet-api` | The wallet's HTTP API |
| `wrkz-service` | The JSON-RPC wallet service |
| `wrkz-txpow-server` | Computes the transaction proof of work for phones and browsers |
| `wrkz-replay` | Imports a C++ node's database, validating every block ([Replay](../tools/replay.md)) |
| `wrkz-verify-state` | Checks a chain state someone else built ([Verifying a chain state](../tools/verify-state.md)) |
| `wrkz-db-inspect` | Checks a C++ database's headers, proofs of work and ring signatures |
| `wrkz-p2p-probe` | Can this machine reach the network at all |
| `wrkz-rpc-diff` | Compares two daemons' RPC answers |
| `wrkz-wallet-sync`, `wrkz-wallet-send` | Sync an address, or build one transaction by hand |

The last four are described in [Diagnostics](../tools/diagnostics.md). On Windows each program ends in `.exe`, and `scripts/release.sh` refuses to package one that imports a MinGW runtime DLL, so each starts on a bare Windows.

## Platform notes

**macOS.** The binaries are ad-hoc signed, which Apple silicon requires before it runs anything, but they are not signed with a Developer ID, so a downloaded copy is quarantined by Gatekeeper. Clear the flag once, in the unpacked directory:

```sh
xattr -d com.apple.quarantine wrkz-*
```

Macs on macOS 11 or 12 need a build made on the Mac itself ([Building from source](build.md)).

**Android.** The programs are command-line programs: run them from Termux, or push them with `adb push` to `/data/local/tmp` and run them from `adb shell`.

**Linux.** The arm64 binaries are linked against glibc 2.28, so they run on Debian 10, Ubuntu 18.10, RHEL 8 and anything newer.

## Before trusting a new architecture

Before you trust a release on an architecture it has not been proven on (a Mac, a Raspberry Pi, Termux on a phone), run the consensus vectors on that machine once, from a checkout of the same commit:

```sh
cargo test --release -p wrkz-pow
```

It runs the CryptoNight, Chukwa and curve vectors against the C++ reference outputs, and the Rust hashing against the vendored C built for that machine. AES is picked at run time (AES-NI, the ARMv8 crypto extensions, or table AES where the CPU has neither, as on the Raspberry Pi 4), and all three paths are tested against each other.

## Next

- [Running a node](../node/index.md)
- [Wallet CLI](../wallets/wallet-cli.md)
- [Project status](status.md): what has been proven so far, and what has not
