# Cross-compiling

One Ubuntu host (22.04 or 24.04, x86_64) builds every release platform, Windows, macOS, Android and arm64 Linux included, with no Windows machine, no Mac and no Apple SDK; this page covers the three scripts that do it, the targets, the toolchain choices and the knobs.

| Script | What it does |
| --- | --- |
| [`scripts/cross-setup.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/cross-setup.sh) | Once: installs the toolchains, pinned and checksum-verified |
| [`scripts/cross.sh <target> [cargo args]`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/cross.sh) | Builds one target with the right toolchain |
| [`scripts/release.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/release.sh) | Builds and packages any set of targets, with `SHA256SUMS` |

The C++ node's build and packaging are in the C++ documentation's [Building](https://docs.wrkz.work/guides/building/) guide and [Other tools](https://docs.wrkz.work/guides/other-tools/). `Dockerfile.cross` is the same host as an image, for building on anything that runs Docker ([Docker](docker.md#the-cross-build-image)).

## Quick start

```sh
scripts/ubuntu-setup.sh          # once, if not done already
scripts/cross-setup.sh           # once: Windows, macOS, arm64 Linux, Android
scripts/release.sh               # every target below
ls dist/                         # one archive per target + SHA256SUMS
```

`dist/` keeps the archives of the same commit between runs, so platforms can be built one at a time; `SHA256SUMS` always covers all of them. Anything else there, such as an older commit's archives, is cleared. A release is built from a commit: `release.sh` refuses a working tree with uncommitted changes.

A subset of the toolchains: `WRKZ_CROSS="windows macos" scripts/cross-setup.sh`. The parts are `windows`, `macos`, `linux-arm64` and `android`.

A subset of the targets, by OS: `scripts/release.sh windows macos`, from `linux`, `windows`, `macos`, `android`, or `host` for the build machine's own. By Rust target: `WRKZ_TARGETS="x86_64-pc-windows-gnu aarch64-apple-darwin" scripts/release.sh`. OS names on the command line win over `WRKZ_TARGETS`.

One binary during development, without packaging:

```sh
scripts/cross.sh x86_64-pc-windows-gnu --release -p wrkz-wallet --bins
scripts/cross.sh aarch64-apple-darwin --release --features rocksdb -p wrkz-node --bin wrkz-node
# → target/<target>/release/
```

## Targets

| Target | Runs on | Toolchain | Archive ends in |
| --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Linux x86_64 | cargo (host); zig, glibc ≥ 2.28 in Docker | `linux-x86_64.tar.gz` |
| `aarch64-unknown-linux-gnu` | Linux arm64: Raspberry Pi 4/5 (64-bit OS), Graviton, Ampere | zig, glibc ≥ 2.28 | `linux-arm64.tar.gz` |
| `x86_64-pc-windows-gnu` | Windows 10/11 x64 | MinGW-w64 (posix threads), static | `windows-x86_64.zip` |
| `x86_64-apple-darwin` | macOS 13+ on Intel | zig | `macos-x86_64.tar.gz` |
| `aarch64-apple-darwin` | macOS 13+ on Apple silicon | zig | `macos-arm64.tar.gz` |
| `aarch64-linux-android` | Android 7.0+ phones and tablets | NDK + cargo-ndk | `android-arm64.tar.gz` |
| `x86_64-linux-android` | Android emulator, x86 Chromebooks | NDK + cargo-ndk | `android-x86_64.tar.gz` |

`scripts/release.sh` with no arguments builds exactly this list. `armv7-linux-androideabi` (32-bit Android) is installed by the setup and `scripts/cross.sh` will build it, but it is **not** in the list and not a supported release: nothing in this port has been tested on a 32-bit target, and consensus code that is right on 64 bits is not automatically right on 32.

Each archive is named `wrkzcoin-cli-<version>-<commit>-<os>-<arch>`, e.g. `wrkzcoin-cli-1.0.0-97f3ab1-windows-x86_64.zip`, and holds the same twelve programs and the `LICENSE`: `wrkz-node`, `wrkz-replay`, `wrkz-verify-state` and `wrkz-db-inspect` (with the RocksDB engine), `wrkz-p2p-probe`, `wrkz-rpc-diff`, `wrkz-service`, `wrkz-wallet`, `wrkz-wallet-api`, `wrkz-wallet-sync`, `wrkz-wallet-send` and `wrkz-txpow-server`. [Installing a release](../getting-started/install.md) is the user's side of it.

## Reproducible releases

The build is reproducible for a given commit and toolchain. Source paths are remapped out of the binaries, the commit is stamped in (`wrkz-node --version` prints it), and each archive is written with fixed owners, a fixed order and the commit's own timestamp, and compressed without a name or time in its header. Two hosts building the same commit with the same `rustc` and the same pinned cross tools produce the same `SHA256SUMS`.

"The same `rustc`" is checked rather than hoped for: `release.sh` refuses to run under any compiler but the one a release is cut with, 1.98.1, the version CI pins and the workspace's `rust-version` names. `WRKZ_RUSTC=` (empty) skips the check for someone deliberately building with another compiler, whose archives will not match.

The shipped binaries are built with thin LTO (`CARGO_PROFILE_RELEASE_LTO=thin`, set by `release.sh` and the Dockerfile rather than in the release profile, so `cargo test --release` does not pay for it). Thin LTO is deterministic, so the sums still reproduce.

With `WRKZ_SIGN_KEY=<gpg key id>`, `release.sh` also writes `SHA256SUMS.asc`, a detached signature.

## With Docker

Without a configured host, the same toolchains come as an image:

```sh
docker build -f Dockerfile.cross -t wrkz-cross .
docker run --rm -v "$PWD":/src -v wrkz-cargo:/usr/local/cargo/registry \
    -u "$(id -u):$(id -g)" wrkz-cross scripts/release.sh
# fewer: ... wrkz-cross scripts/release.sh windows macos
```

From PowerShell with Docker Desktop on Windows (no `-u`; the build tree goes in a volume, because compiling onto a Windows drive through the mount is far slower):

```powershell
docker build -f Dockerfile.cross -t wrkz-cross .
docker run --rm -v "${PWD}:/src" -v wrkz-cargo:/usr/local/cargo/registry `
    -v wrkz-target:/src/target wrkz-cross bash scripts/release.sh
```

A Windows checkout made before `.gitattributes` existed has CRLF scripts, which bash in the container rejects. On a clean tree, delete them and let git write them again with LF: `Remove-Item scripts\*.sh, Dockerfile*; git checkout -- .`

The image builds Linux x86_64 through zig too (`WRKZ_HOST_ZIG=1`), against glibc 2.28 like arm64, rather than against its own Ubuntu 24.04 glibc (2.39), so the one Linux binary runs on Ubuntu 20.04, 22.04 and 24.04 alike. More on the image in [Docker](docker.md).

## Why these toolchains

**Windows: MinGW-w64.** It is the toolchain the port is already developed with on Windows (`x86_64-pc-windows-gnu`), so a Linux-built `.exe` is the same kind of binary. The *posix-threads* variant (`x86_64-w64-mingw32-g++-posix`) is required: RocksDB uses `std::thread` and `std::mutex`, which the win32-threads variant does not provide. It is linked with `-static`, and the C++ runtime that RocksDB needs is linked as `libstdc++.a` (`CXXSTDLIB_x86_64_pc_windows_gnu=static:-bundle=stdc++`; `-static` alone does not reach it), so the `.exe` runs on a bare Windows with no MinGW DLLs (`libstdc++-6.dll`, `libgcc_s_seh-1.dll`, `libwinpthread-1.dll`) beside it. `release.sh` refuses to package an `.exe` that imports any of them: Windows would load whichever copy it finds on `PATH` (Git's, say), and fail with "The procedure entry point … could not be located".

**macOS: zig.** `cargo zigbuild` uses zig as the C and C++ compiler and as the linker. zig carries the macOS headers and a libc++, so neither Xcode nor an Apple SDK is needed, which is also what keeps this legal to run on Linux. zig ad-hoc signs an arm64 Mach-O as it links it, which Apple silicon requires before it will run a binary at all; for that reason the macOS binaries are **not** stripped (stripping would invalidate the signature). They are unsigned in the Developer ID sense, so a downloaded copy is quarantined by Gatekeeper; run `xattr -d com.apple.quarantine wrkz-*` once, or sign and notarize them on a Mac for a public release.

The oldest macOS they run on is **13 (Ventura)**, on both Intel and Apple silicon. That is zig 0.15's floor: it raises any lower `MACOSX_DEPLOYMENT_TARGET` to 13.0, so there is no setting for it. Macs stuck on macOS 11 or 12 need a build made on a Mac (`cargo build --release` there works as on Linux).

**arm64 Linux: zig**, for the same reason as macOS, plus one more: zig links against a chosen glibc version (`WRKZ_GLIBC`, default 2.28, which is Debian 10 / Ubuntu 18.10 / RHEL 8 and newer), so one binary runs across distros. The host's own Linux build uses plain cargo and therefore needs the build machine's glibc or newer; `WRKZ_HOST_ZIG=1` builds it through zig too.

**Android: the NDK.** Nothing else ships Android's libc (bionic). `cargo ndk` points cargo at the NDK's clang for the chosen API level (`WRKZ_ANDROID_API`, default 24 = Android 7.0). The C++ runtime is linked in statically (`libc++_static` + `libc++abi`), so there is no `libc++_shared.so` to ship. The binaries are command-line programs: run them from Termux, or push them with `adb push` to `/data/local/tmp`. The Android NDK is published for x86_64 Linux hosts only, so the `android` part of the setup needs one.

## Knobs

| Variable | Default | Meaning |
| --- | --- | --- |
| `WRKZ_CROSS` | all four parts | Which toolchains `cross-setup.sh` installs |
| `WRKZ_TOOLS` | `~/.local/wrkz-cross` | Where zig and the NDK go; `cross.sh` reads `$WRKZ_TOOLS/env` |
| `WRKZ_TARGETS` | `all` | What `release.sh` builds when given no OS names; `all` is the table above |
| `WRKZ_RUSTC` | `1.98.1` | The `rustc` `release.sh` insists on; empty skips the check |
| `WRKZ_GLIBC` | `2.28` | Oldest glibc a zig-built Linux binary runs on |
| `WRKZ_HOST_ZIG` | `0` (`1` in the image) | `1`: build the host Linux target with zig as well |
| `WRKZ_ANDROID_API` | `24` | Android API level |
| `WRKZ_SIGN_KEY` | none | GPG key that signs `SHA256SUMS` |
| `BINDGEN_EXTRA_CLANG_ARGS_<target>` | none | Extra clang flags for the RocksDB bindings, e.g. a `--sysroot` |

## Pinned tools

`cross-setup.sh` pins zig 0.15.2 (SHA-256 from ziglang.org), Android NDK r30 LTS (SHA-1 from developer.android.com), cargo-zigbuild 0.23.4 and cargo-ndk 4.1.2. A download whose checksum does not match stops the setup. To move a version, change it and its checksum together at the top of the script. The setup is safe to run again: what is already there is kept.

Together with a fixed `rustc`, the pins are what makes `SHA256SUMS` reproducible across build hosts.

## Notes for porters

- **The proof of work is Rust on every target.** The CryptoNight scratchpad is a per-thread heap buffer (`crates/wrkz-pow/src/cryptonight`), so no target puts it on a thread's stack. The only C left in a shipped binary is the curve code in `crates/wrkz-pow-ref`, which has no per-target flags.
- **AES is picked at run time.** AES-NI on x86_64 and the ARMv8 crypto extensions on aarch64 when the CPU has them; otherwise (the Raspberry Pi 4's Cortex-A72, for one) the table AES ported from `aesb.c`. All three give the same hash and are tested against each other; the table path is only slower.
- **Consensus vectors should be run on each new architecture** before a release is trusted there: `cargo test --release -p wrkz-pow` on an arm64 machine (a Raspberry Pi, a Mac, or Termux on a phone) runs the CryptoNight, Chukwa and curve vectors against the C++ reference outputs, and the Rust hashing against the vendored C built for that machine.
- **RocksDB's bindings** are generated by bindgen with the host's libclang for the target triple. They need only the C headers clang itself ships (`stdint.h`, `stddef.h`, `stdbool.h`), so no target sysroot is required. If a target ever fails with a missing header, pass its sysroot through `BINDGEN_EXTRA_CLANG_ARGS_<target_with_underscores>`. Android is the exception: cargo-ndk points bindgen at the NDK sysroot, whose headers reject a target triple without an API level, so `cross.sh` passes bindgen the sysroot and `--target=<triple><api>` itself, in the hyphenated `BINDGEN_EXTRA_CLANG_ARGS_<target>` that bindgen reads first. On Android that one replaces any value you set in the underscored form.

CI builds the macOS, arm64 Linux and Windows targets on every push with `-D warnings` (the `cross` job, [Testing](../contributing/testing.md#ci-jobs)), without RocksDB or Android.
