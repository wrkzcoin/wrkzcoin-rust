#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# One-time setup of the cross toolchains on an Ubuntu 22.04/24.04 (or Debian
# 12) host, so that scripts/cross.sh and scripts/release.sh can build the node
# and the wallets for Windows, macOS, Android and arm64 Linux from it. Run it
# after (or instead of) scripts/ubuntu-setup.sh. docs/CROSS-COMPILE.md explains
# the choices; in short:
#
#   windows      MinGW-w64 with posix threads, linked fully static
#   macos        zig as the C/C++ compiler and linker (cargo-zigbuild): no
#                Apple SDK and no Mac needed
#   linux-arm64  zig as well, against a glibc floor (default 2.28)
#   android      the Android NDK and cargo-ndk (x86_64 hosts only)
#
#   scripts/cross-setup.sh                              # all four
#   WRKZ_CROSS="windows macos" scripts/cross-setup.sh   # a subset
#
# Everything that is not an apt package goes under $WRKZ_TOOLS (default
# ~/.local/wrkz-cross), pinned and checksum-verified, and $WRKZ_TOOLS/env
# records where; scripts/cross.sh reads that file, so nothing has to be
# exported by hand. Safe to run again: what is already there is kept.
set -euo pipefail
# shellcheck disable=SC1091
[ -f "${CARGO_HOME:-$HOME/.cargo}/env" ] && source "${CARGO_HOME:-$HOME/.cargo}/env"

parts=${WRKZ_CROSS:-"windows macos linux-arm64 android"}
tools=${WRKZ_TOOLS:-$HOME/.local/wrkz-cross}

# Pinned. A version bump updates the checksum beside it, from
# https://ziglang.org/download/index.json and
# https://developer.android.com/ndk/downloads respectively.
ZIG_VERSION=0.15.2
ZIG_SHA256_x86_64=02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239
ZIG_SHA256_aarch64=958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f
NDK_VERSION=r30
NDK_SHA1=5107f898313790e449e87eee2183d9a20602dee9
CARGO_ZIGBUILD_VERSION=0.23.4
CARGO_NDK_VERSION=4.1.2

want() {
    case " $parts " in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
    esac
}
sudo=
[ "$(id -u)" -ne 0 ] && sudo=sudo
arch=$(uname -m)

# Download `url` to `out` and check it, or stop.
fetch() {
    local url=$1 out=$2 algo=$3 sum=$4
    curl --proto '=https' --tlsv1.2 -fL --retry 3 -o "$out" "$url"
    echo "$sum  $out" | "${algo}sum" -c -
}

packages="build-essential clang libclang-dev llvm cmake pkg-config git curl ca-certificates xz-utils unzip zip"
want windows && packages="$packages mingw-w64"
$sudo apt-get update
# shellcheck disable=SC2086
$sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $packages

if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    source "${CARGO_HOME:-$HOME/.cargo}/env"
fi

targets=""
want windows && targets="$targets x86_64-pc-windows-gnu"
want macos && targets="$targets x86_64-apple-darwin aarch64-apple-darwin"
want linux-arm64 && targets="$targets aarch64-unknown-linux-gnu"
want android && targets="$targets aarch64-linux-android x86_64-linux-android armv7-linux-androideabi"
# shellcheck disable=SC2086
[ -n "$targets" ] && rustup target add $targets

mkdir -p "$tools/bin"
env_file="$tools/env"
{
    echo "# Written by scripts/cross-setup.sh; read by scripts/cross.sh."
    echo "export PATH=\"$tools/bin:\$PATH\""
} > "$env_file.new"

if want macos || want linux-arm64; then
    zig_dir="$tools/zig-$arch-linux-$ZIG_VERSION"
    if [ ! -x "$zig_dir/zig" ]; then
        case $arch in
            x86_64) sum=$ZIG_SHA256_x86_64 ;;
            aarch64) sum=$ZIG_SHA256_aarch64 ;;
            *) echo "no zig pinned for a $arch host" >&2; exit 1 ;;
        esac
        tmp=$(mktemp -d)
        fetch "https://ziglang.org/download/$ZIG_VERSION/zig-$arch-linux-$ZIG_VERSION.tar.xz" "$tmp/zig.tar.xz" sha256 "$sum"
        tar -C "$tools" -xJf "$tmp/zig.tar.xz"
        rm -rf "$tmp"
    fi
    ln -sfn "$zig_dir/zig" "$tools/bin/zig"
    cargo install cargo-zigbuild --version "$CARGO_ZIGBUILD_VERSION" --locked
fi

if want android; then
    if [ "$arch" != x86_64 ]; then
        echo "the Android NDK is published for x86_64 Linux hosts only" >&2
        exit 1
    fi
    ndk_dir="$tools/android-ndk-$NDK_VERSION"
    if [ ! -d "$ndk_dir" ]; then
        tmp=$(mktemp -d)
        fetch "https://dl.google.com/android/repository/android-ndk-$NDK_VERSION-linux.zip" "$tmp/ndk.zip" sha1 "$NDK_SHA1"
        unzip -q "$tmp/ndk.zip" -d "$tools"
        rm -rf "$tmp"
    fi
    echo "export ANDROID_NDK_HOME=\"$ndk_dir\"" >> "$env_file.new"
    cargo install cargo-ndk --version "$CARGO_NDK_VERSION" --locked
fi

mv "$env_file.new" "$env_file"
echo "cross toolchains ready for: $parts"
echo "scripts/cross.sh reads $env_file; try: scripts/release.sh"
