#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# Build a release on the Linux host: the node and the import tool with the
# RocksDB engine, and the wallets, stripped, one archive per target, with a
# SHA256SUMS file and — when a key is configured — its detached signature.
#
#   scripts/release.sh                               # every target below
#   scripts/release.sh windows macos                 # by OS: linux windows macos android host
#   WRKZ_TARGETS="x86_64-pc-windows-gnu aarch64-apple-darwin" scripts/release.sh
#   WRKZ_SIGN_KEY=<gpg key id> scripts/release.sh    # plus SHA256SUMS.asc
#
# Every target is x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu (linux),
# x86_64-pc-windows-gnu (windows), x86_64-apple-darwin, aarch64-apple-darwin
# (macos), aarch64-linux-android and x86_64-linux-android (android); `host` is
# the build machine's own. OS names on the command line win over WRKZ_TARGETS.
# Every target but the host needs scripts/cross-setup.sh once (or the
# Dockerfile.cross image); scripts/cross.sh builds each one and says with what
# (docs/CROSS-COMPILE.md).
#
# Each archive is wrkzcoin-cli-<version>-<commit>-<os>-<arch>, e.g.
# wrkzcoin-cli-1.0.0-97f3ab1-windows-x86_64.zip: a .zip for Windows, a .tar.gz
# for the rest.
#
# Output goes to dist/. It keeps the archives of the same commit from earlier
# runs — `release.sh windows` then `release.sh linux` leaves both, and
# SHA256SUMS covers every archive there — and clears anything else, such as
# an older commit's archives. The build is reproducible for a given commit and
# toolchain: source paths are remapped out of the binaries, the commit is
# stamped in (`wrkz-node --version` prints it), and each archive is written
# with fixed owners, a fixed order and the commit's own timestamp, and
# compressed without a name or time in its header. Two hosts building the same
# commit with the same `rustc` and the same pinned cross tools produce the same
# SHA256SUMS — and "the same rustc" is checked rather than hoped for, below.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/cross.sh
source scripts/cross.sh

# The compiler a release is cut with. Reproducibility is a claim about the
# bytes two hosts produce, and a different rustc produces different bytes, so
# this is checked rather than documented. Bump it here and in the workspace's
# `rust-version` together; `WRKZ_RUSTC=` (empty) skips the check for someone
# deliberately building with another compiler, whose archives will not match.
WRKZ_RUSTC=${WRKZ_RUSTC-1.98.1}
if [ -n "$WRKZ_RUSTC" ]; then
    have=$(rustc --version | awk '{print $2}')
    if [ "$have" != "$WRKZ_RUSTC" ]; then
        echo "release.sh: this release is built with rustc $WRKZ_RUSTC, but rustc $have is on PATH."
        echo "            rustup toolchain install $WRKZ_RUSTC && rustup override set $WRKZ_RUSTC"
        echo "            or WRKZ_RUSTC= scripts/release.sh to build anyway (the archives will differ)."
        exit 1
    fi
fi

if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "the working tree has uncommitted changes; a release is built from a commit"
    exit 1
fi

all="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-pc-windows-gnu \
x86_64-apple-darwin aarch64-apple-darwin aarch64-linux-android x86_64-linux-android"

# The targets an OS name stands for.
os_targets() {
    case $1 in
        linux) echo "x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu" ;;
        windows) echo "x86_64-pc-windows-gnu" ;;
        macos) echo "x86_64-apple-darwin aarch64-apple-darwin" ;;
        android) echo "aarch64-linux-android x86_64-linux-android" ;;
        host) cross_host ;;
        all) echo "$all" ;;
        *)
            echo "release.sh: no OS called '$1'; pick from linux windows macos android host all" >&2
            return 1
            ;;
    esac
}

# The <os>-<arch> a target's archive is named with.
platform() {
    local os arch
    case $1 in
        *-windows-*) os=windows ;;
        *-apple-darwin) os=macos ;;
        *-android*) os=android ;;
        *-linux-musl*) os=linux-musl ;;
        *-linux-*) os=linux ;;
        *) os=${1#*-} ;;
    esac
    case $1 in
        aarch64-*) arch=arm64 ;;
        *) arch=${1%%-*} ;;
    esac
    echo "$os-$arch"
}

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
commit=$(git rev-parse --short HEAD)
if [ $# -gt 0 ]; then
    targets=""
    for os in "$@"; do
        targets="$targets $(os_targets "$os")"
    done
else
    targets=${WRKZ_TARGETS:-all}
    if [ "$targets" = all ]; then
        targets=$all
    fi
fi
# `wrkz-p2p-probe` ships as the operator's connectivity diagnostic: it needs no
# data directory and no state, dials any peer, and exits non-zero when the
# handshake or the block download fails. It is the only thing here that can
# answer "can this box reach the network at all" before a node exists.
bins="wrkz-node wrkz-replay wrkz-p2p-probe wrkz-service \
wrkz-wallet wrkz-wallet-api wrkz-wallet-sync wrkz-wallet-send wrkz-txpow-server"

export WRKZ_GIT_COMMIT="$commit"
export SOURCE_DATE_EPOCH
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$PWD=. --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=~/.cargo"

mkdir -p dist
find dist -mindepth 1 -maxdepth 1 ! -name "wrkzcoin-cli-$version-$commit-*" -exec rm -rf {} +
for target in $targets; do
    echo "== $target =="
    cross_build "$target" --release --locked --features rocksdb -p wrkz-node --bin wrkz-node
    cross_build "$target" --release --locked --features rocksdb -p wrkz-chain --bin wrkz-replay
    cross_build "$target" --release --locked -p wrkz-p2p --bin wrkz-p2p-probe
    cross_build "$target" --release --locked -p wrkz-wallet --bins
    cross_build "$target" --release --locked -p wrkz-service --bin wrkz-service
    cross_build "$target" --release --locked -p wrkz-txpow-server --bin wrkz-txpow-server

    name="wrkzcoin-cli-$version-$commit-$(platform "$target")"
    stage="dist/$name"
    exe=$(cross_exe_suffix "$target")
    # zip adds to an existing archive rather than replacing it.
    rm -rf "$stage" "dist/$name.zip" "dist/$name.tar.gz"
    mkdir -p "$stage"
    for b in $bins; do
        cp "target/$target/release/$b$exe" "$stage/"
    done
    cp LICENSE "$stage/"
    find "$stage" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
    case $target in
        *-windows-*)
            # Each .exe must start on a bare Windows: no MinGW runtime DLL
            # imported, or Windows loads whatever copy it finds on PATH.
            for b in $bins; do
                if x86_64-w64-mingw32-objdump -p "$stage/$b.exe" | grep -iE 'DLL Name: (libstdc\+\+|libgcc|libwinpthread)'; then
                    echo "release.sh: $b.exe imports a MinGW runtime DLL (above); it would not start on a bare Windows" >&2
                    exit 1
                fi
            done
            # -X: no uid/gid or extra timestamps; sorted input: a fixed order;
            # TZ=UTC: zip stores local time.
            (cd dist && find "$name" -print | LC_ALL=C sort | TZ=UTC zip -X -9 -q -@ "$name.zip")
            ;;
        *)
            tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$SOURCE_DATE_EPOCH" \
                -C dist -cf - "$name" | gzip -n -9 > "dist/$name.tar.gz"
            ;;
    esac
    rm -rf "$stage"
done

(
    cd dist
    shopt -s nullglob
    archives=(*.tar.gz *.zip)
    sha256sum -- "${archives[@]}" > SHA256SUMS
)
if [ -n "${WRKZ_SIGN_KEY:-}" ]; then
    gpg --batch --yes --local-user "$WRKZ_SIGN_KEY" --armor --detach-sign --output dist/SHA256SUMS.asc dist/SHA256SUMS
fi
echo
cat dist/SHA256SUMS
echo "release in dist/"
