# Docker

The repository has three Dockerfiles: `Dockerfile` builds an image that runs the node, `Dockerfile.cross` is a build host for every release platform, and `Dockerfile.pluton` adds what Rust Pluton Wallet is built with; this page covers each.

For the C++ node's images and packaging scripts, see [Other tools](https://docs.wrkz.work/guides/other-tools/) in the C++ documentation.

## The node image

[`Dockerfile`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/Dockerfile) builds the node, the import tool and the wallets with the RocksDB engine (the build stage has the libclang that needs), with thin LTO as a release is built, and copies them onto `debian:bookworm-slim`:

- programs in `/usr/local/bin`: `wrkz-node`, `wrkz-replay`, `wrkz-wallet`, `wrkz-wallet-api`, `wrkz-wallet-sync`, `wrkz-wallet-send` and `wrkz-simnet`;
- they run as the unprivileged user `wrkz` (uid 10001), whose home is `/data`;
- the chain state lives in the volume `/data`;
- ports 17855 (P2P) and 17856 (RPC) are exposed;
- the entry point is `wrkz-node --data-dir /data --rpc-bind-ip 0.0.0.0`.

Build and run it:

```sh
docker build -t wrkz-rust .
docker run -d --name wrkz -v wrkz-data:/data \
    -p 17855:17855 -p 127.0.0.1:17856:17856 wrkz-rust
```

To stamp the commit into `wrkz-node --version`, pass it at build time: `docker build --build-arg WRKZ_GIT_COMMIT=$(git rev-parse --short HEAD) -t wrkz-rust .`

### The RPC port

Inside the container the RPC listens on `0.0.0.0` so that a port mapping can reach it at all. Publish it on `127.0.0.1` as above, or give it a token. Anything after the image name is appended to the daemon's command line:

```sh
docker run -d --name wrkz -v wrkz-data:/data -p 17855:17855 -p 17856:17856 \
    wrkz-rust --rpc-access-token "$(openssl rand -hex 16)"
```

The same way, any other `wrkz-node` option can be given. A `Wrkzd` configuration file works too: put it in the volume and pass `-c /data/wrkz.json`. [Configuration](../node/configuration.md) lists the options.

The ZMQ publisher binds `tcp://127.0.0.1:17857` by default, which inside a container nothing outside can reach; to use it, give `--zmq-pub` an address on all interfaces and publish that port too ([ZMQ and notify hooks](../node/zmq-and-hooks.md)).

### Stopping

`SIGTERM` is the clean shutdown: the engine flushes the chain state and writes the peer file. Give it time:

```sh
docker stop -t 120 wrkz
```

### Importing a C++ database

To bring the state up from a C++ database instead of syncing from peers, run the import tool in the image, against the same volume:

```sh
docker run --rm -v wrkz-data:/data -v /path/to/copy/of/DB:/cpp:ro \
    --entrypoint wrkz-replay wrkz-rust --store-raw --db /cpp --state /data/state
```

then start the node as above. [Replay](../tools/replay.md) explains `--store-raw` and the rest.

The other programs in the image run the same way, with `--entrypoint`.

### A simnet

[`compose.simnet.yml`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/compose.simnet.yml) runs a private test network from the same image: three `--simnet` nodes in a line, each with its RPC and `/ws` on loopback, and `wrkz-simnet mine` making a block every ten seconds.

```sh
docker compose -f compose.simnet.yml up -d
```

[Simnet](../tools/simnet.md) explains what a simnet is and the other ways to run one.

## The cross-build image

[`Dockerfile.cross`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/Dockerfile.cross) is a Linux build host for every release target: the node, the import tool and the wallets for Linux (x86_64, arm64), Windows, macOS and Android, built by `scripts/release.sh`. The image holds only toolchains (it runs `scripts/cross-setup.sh` on Ubuntu 24.04) and the source is mounted, so one image builds every commit.

```sh
docker build -f Dockerfile.cross -t wrkz-cross .
docker run --rm -v "$PWD":/src -v wrkz-cargo:/usr/local/cargo/registry \
    -u "$(id -u):$(id -g)" wrkz-cross scripts/release.sh
```

That is every platform; name some to build only those (`linux`, `windows`, `macos`, `android`):

```sh
docker run --rm -v "$PWD":/src -v wrkz-cargo:/usr/local/cargo/registry \
    -u "$(id -u):$(id -g)" wrkz-cross scripts/release.sh windows macos
```

The archives and `SHA256SUMS` land in `./dist`, owned by you. The `wrkz-cargo` volume keeps the downloaded crates between runs.

From PowerShell with Docker Desktop: no `-u`, and the build tree in a volume, since compiling onto a Windows drive through the mount is far slower:

```powershell
docker build -f Dockerfile.cross -t wrkz-cross .
docker run --rm -v "${PWD}:/src" -v wrkz-cargo:/usr/local/cargo/registry `
    -v wrkz-target:/src/target wrkz-cross bash scripts/release.sh
```

The checkout needs the repository's `.gitattributes` in effect (LF scripts). A Windows checkout made before it existed has CRLF scripts, which bash in the container rejects; on a clean tree, delete them and let git write them again: `Remove-Item scripts\*.sh, Dockerfile*; git checkout -- .`

A subset of the toolchains: `docker build -f Dockerfile.cross --build-arg WRKZ_CROSS="windows macos" -t wrkz-cross .` The Linux x86_64 build needs zig, which comes with `macos` or `linux-arm64`: the image sets `WRKZ_HOST_ZIG=1`, so Linux x86_64 is linked by zig against glibc 2.28 like arm64, not against the image's own 2.39, and also runs on older distributions.

[Cross-compiling](cross-compile.md) explains the toolchains, the targets and the knobs.

## The Rust Pluton Wallet image

[`Dockerfile.pluton`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/Dockerfile.pluton) is everything Rust Pluton Wallet is built with, in one image: it starts `FROM wrkz-cross`, so it shares the desktop cross-toolchains with the node, and adds the Android SDK (platform 35, build tools 35.0.0), JDK 17 and Gradle 8.9 for the APKs, wasm-bindgen 0.2.128 and binaryen's `wasm-opt` for the browser build, and fontconfig for both Linux desktop targets.

```sh
docker build -f Dockerfile.cross -t wrkz-cross .        # once, the base
docker build -f Dockerfile.pluton -t wrkz-pluton .
docker run --rm -v "$PWD":/src -v wrkz-cargo:/usr/local/cargo/registry \
    -u "$(id -u):$(id -g)" wrkz-pluton scripts/pluton-build.sh
```

Name platforms to build fewer: `... scripts/pluton-build.sh android web`. The archives and both APKs land in `dist/pluton`, owned by you.

macOS is not here: a Mac app needs Apple's SDK and a Mac to sign on, so build it there with `scripts/pluton-macos.sh`. See [Rust Pluton Wallet](../wallets/pluton.md).
