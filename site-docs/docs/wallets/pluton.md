# Rust Pluton Wallet

Rust Pluton Wallet is the WrkzCoin wallet with a window, on the Rust wallet
core: one codebase for Windows, macOS, Linux, Android and the browser. It is
[`apps/pluton`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/apps/pluton),
its own workspace, so the node and the command-line programs never pull a GUI
toolkit into their builds.

The name continues the C++ wallets' — PLUTON v2 on the desktop, PLUTON Mobile,
PLUTON Web — and its version numbers start again at 1.0 because the core
beneath it is new. The C++ apps are Flutter programs on the C++ wallet library,
described on the C++ site under
[wallet apps](https://docs.wrkz.work/guides/wallet-apps/); this one shares no
code with them, and runs on the same `wrkz-wallet` core the
[command-line wallet](wallet-cli.md) uses.

## What it is made of

| Piece | What it does |
| --- | --- |
| `ui/app.slint` | Every screen. A side rail on a desktop, a tab bar on a phone, the same pages behind both |
| `src/ui.rs` | Fills the screens and answers their buttons. Touches no wallet itself |
| `src/service.rs` | The wallet: owns the open file, answers commands, takes one sync step at a time |
| `src/protocol.rs` | The messages between the two, and WRKZ amount formatting |
| `src/native.rs` | Desktop and Android: wallet files in a folder, the wallet on a background thread |
| `src/web.rs` | The browser: the wallet in a Web Worker, files in IndexedDB |
| `web/` | The page, the worker, and the storage helpers |
| `android/` | The Gradle project that packages the APKs |

**The wallet never runs on the thread that draws.** On a desktop and on Android
it runs on a background thread; in a browser it runs inside a Web Worker, where
a blocking request is allowed. That is what lets the same wallet code run
everywhere, and why syncing or a proof-of-work search never freezes the window.
The C++ web wallet made its requests on the page's own thread and froze.

## Wallet files

On a desktop the wallets live in `RustPlutonWallet/wallets` under the
platform's data folder: `%APPDATA%\RustPlutonWallet\wallets` on Windows,
`~/Library/Application Support/RustPlutonWallet/wallets` on macOS and
`~/.local/share/RustPlutonWallet/wallets` on Linux. Android uses the app's own
directory. Each wallet is a `<name>.wallet` file in the same format
[`wrkz-wallet`](wallet-cli.md#compatibility-with-the-c-wallet) writes, so a
wallet moves between the two by copying the file. An open wallet is saved at
most every 30 seconds while it syncs, and always on close.

Until it is changed in Settings, the wallet uses the node
`http://node-fin.wrkz.work:17856`, or `https://node-fin.wrkz.work` in a browser.

## Building

To run it while developing, from the repository root:

```sh
cd apps/pluton && cargo run --release
```

Where OpenGL is missing or broken — an old virtual machine, a locked-down
desktop — `SLINT_BACKEND=winit-software` draws without the GPU:

```sh
SLINT_BACKEND=winit-software cargo run --release
```

It is not reached by `cargo --workspace` from the root: `scripts/ci.sh pluton`
lints and tests it separately, and the workflow's `deny` job audits its
dependency tree with the same `deny.toml`.

One Linux host builds every platform but macOS, into `dist/pluton`, with
[`scripts/pluton-build.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/pluton-build.sh):

```sh
scripts/pluton-build.sh                  # linux, windows, android, web
scripts/pluton-build.sh android web      # or only the ones named
```

That needs the Android SDK, a JDK, Gradle and wasm-bindgen on top of the
cross-toolchains of [Cross-compiling](../building/cross-compile.md); they come
as an image:

```sh
docker build -f Dockerfile.cross -t wrkz-cross .     # once, the base
docker build -f Dockerfile.pluton -t wrkz-pluton .
docker run --rm -v "$PWD":/src -v wrkz-cargo:/usr/local/cargo/registry \
    -u "$(id -u):$(id -g)" wrkz-pluton scripts/pluton-build.sh
```

| Platform | Result | Built with |
| --- | --- | --- |
| Linux x86_64 | `.tar.gz` | cargo |
| Linux arm64 | `.tar.gz` | zig |
| Windows x64 | `.zip` | MinGW-w64 |
| Android arm64 + x86_64 | **debug and release `.apk`** | cargo-ndk + Gradle |
| Browser | `.tar.gz` static site | wasm-bindgen |
| macOS | `Pluton.app` | `scripts/pluton-macos.sh`, **on a Mac** |

Pluton is not in the `wrkzcoin-cli-*` archives; its own are named
`rust-pluton-wallet-<version>-<commit>-<os>-<arch>`, for example
`rust-pluton-wallet-<version>-<commit>-linux-x86_64.tar.gz`,
`…-windows-x86_64.zip`, `…-android-release.apk` and `…-web.tar.gz`.

### Keep the chain out of the build context

`docker build .` hands the daemon a copy of the whole directory first. A
checkout that has been used to run a node holds the chain state as well —
`--data-dir ./wrkz-rust`, `./wrkz-sync0`, a copied C++ `data-*/DB` — and that
is tens of gigabytes of pointless copying before the first instruction runs.
`.dockerignore` excludes all of it, and every build directory too; keep it that
way if you move the data somewhere new.

`Dockerfile.pluton` copies nothing from the context at all, so it can be built
with none:

=== "bash"

    ```sh
    docker build -t wrkz-pluton - < Dockerfile.pluton
    ```

=== "PowerShell"

    ```powershell
    mkdir "$env:TEMP\empty-ctx"
    docker build -f Dockerfile.pluton -t wrkz-pluton "$env:TEMP\empty-ctx"
    ```

`Dockerfile.cross` copies one file, `scripts/cross-setup.sh`, so it takes the
two of them and nothing else:

```sh
tar -c Dockerfile.cross scripts/cross-setup.sh | docker build -f Dockerfile.cross -t wrkz-cross -
```

That pipe carries a tar stream, so run it in bash — PowerShell's pipeline is
text and corrupts it. On Windows use the empty-context form above, or
`cmd /c "..."`. More on the images is under [Docker](../building/docker.md).

### macOS needs a Mac

Every other target cross-compiles from one Linux host. A macOS application does
not: it needs Apple's SDK to build against and a Mac to sign on. Run
[`scripts/pluton-macos.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/pluton-macos.sh)
there:

```sh
scripts/pluton-macos.sh              # this Mac's architecture
scripts/pluton-macos.sh universal    # Intel and Apple silicon in one application
```

It produces `dist/pluton/Pluton.app` and a `.tar.gz` beside it, unsigned, so
macOS refuses to open it until you sign it with a Developer ID or open it once
with right-click → Open. The application needs macOS 11 or later.

### Signing the Android release

Without a key, `assembleRelease` signs with the debug key: the APK installs and
runs, but must not be published, and the build says so. With your own key:

```sh
PLUTON_KEYSTORE=/keys/pluton.jks PLUTON_KEYSTORE_PASSWORD=KEYSTORE_PASSWORD \
PLUTON_KEY_ALIAS=pluton PLUTON_KEY_PASSWORD=KEY_PASSWORD scripts/pluton-build.sh android
```

The debug APK's application id ends in `.debug`, so it installs beside the
release one for testing.

## The browser build

The bundle is a static site: unpack it at the document root of
`rust-wallet.wrkz.work`. Two things must be true or the wallet cannot reach a
node:

- **The node must be HTTPS.** A page served over HTTPS may not call `http://`.
- **The node must allow that origin** (`--enable-cors https://rust-wallet.wrkz.work`
  on the daemon, or the same header from the reverse proxy — not both, or the
  browser sees it twice and refuses). The daemon's options are under
  [Configuration](../node/configuration.md).

Wallet files live in that browser's IndexedDB, encrypted with the user's
password exactly as on a desktop. Clearing site data deletes them, so the seed
written down at creation is the only way back — the wallet says so when it
shows it.

!!! warning "Write the seed down"
    In a browser, clearing the site's data deletes the wallet file. On every
    platform, the 25-word seed shown when a wallet is created is the only way
    back to the funds if the file or its password is lost. While the wallet is
    open, Settings → *This wallet* shows the seed and keys again after the
    wallet password is entered.

### It is a big download — serve it compressed

Measured on 2026-09-12 from a release build:

| | Size |
| --- | --- |
| WebAssembly module, as `wasm-bindgen` leaves it | 16.6 MB |
| The same, gzipped — what a visitor actually fetches | 6.1 MB |

`scripts/pluton-build.sh` runs `wasm-opt -Oz` over it when binaryen is
installed (the image has it), which takes roughly a third off again.

**Turn compression on for `.wasm`**, or every visitor pulls the full 16.6 MB.
Most web servers do not compress that type by default:

```nginx
gzip on;
gzip_types application/wasm application/javascript text/html;
# Better still, if the module is pre-compressed beside the file:
# brotli_static on;  gzip_static on;
```

A phone on a slow connection will still wait, so the page shows "Loading the
wallet…" until the module is ready.

### The proof of work, in a browser

Every WrkzCoin transaction carries a small proof of work. On a desktop the
search takes seconds; in a browser, with one thread and no AES instructions, it
is impractical. So:

| Platform | Without a proof-of-work server | With one |
| --- | --- | --- |
| Desktop, Android | computed here | asked of the server, computed here if it fails |
| Browser | pays the 100 WRKZ fee that skips the proof of work | asked of the server, ordinary fee |

That is what the C++ web wallet does too. A server is configured in Settings,
under *Transaction proof-of-work server*: a URL, an optional API key, a *Test*
button and *Apply*. The server itself, which you can run yourself, is
[`wrkz-txpow-server`](txpow-server.md).

## Licensing

The app is GPL-3.0 or later, like the rest of this repository. It draws with
[Slint](https://slint.dev) under Slint's GPL-3.0 licence (Slint is
`GPL-3.0-only OR` its own royalty-free and commercial licences), so a Pluton
build is distributed under GPL-3.0. The **About** page credits Slint. See
[Licence](../licence.md).

## What is not there yet

- **A QR code** on the Receive page, and a camera scanner on Android.
- **Address book**, and payment-ID history per contact.
- **A smaller browser download**: the page and the worker load the same
  WebAssembly module today, so a browser fetches those 6.1 MB twice (the second
  time from its own cache, but it parses the module twice either way). The
  worker needs none of the drawing code and the page needs none of the wallet;
  building the crate twice behind a feature would roughly halve both.
- **Hardware wallet support**, which the core does not have either.
