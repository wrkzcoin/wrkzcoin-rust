#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# Builds Rust Pluton Wallet (apps/pluton) for every platform one Linux host can
# reach, into dist/pluton:
#
#   linux    rust-pluton-wallet-<version>-<commit>-linux-x86_64.tar.gz
#            rust-pluton-wallet-<version>-<commit>-linux-arm64.tar.gz
#   windows  rust-pluton-wallet-<version>-<commit>-windows-x86_64.zip
#   android  rust-pluton-wallet-<version>-<commit>-android-debug.apk
#            rust-pluton-wallet-<version>-<commit>-android-release.apk
#   web      rust-pluton-wallet-<version>-<commit>-web.tar.gz
#
#   scripts/pluton-build.sh                 # all of them
#   scripts/pluton-build.sh android web     # only these
#
# macOS is not here: a Mac app needs Apple's SDK and a Mac to sign on. Build it
# there with scripts/pluton-macos.sh.
#
# Everything this needs is in the image built from Dockerfile.pluton; see
# docs/PLUTON.md.
set -euo pipefail

# zig's lld opens every object file at once, and the wallet links more than a
# thousand of them; a container starts with 1024 descriptors, and the arm64
# link dies on that with ProcessFdQuotaExceeded. The hard limit is far higher
# and costs nothing to ask for. Should the host refuse to raise it, run the
# container with --ulimit nofile=65536:65536.
hard_nofile=$(ulimit -Hn 2>/dev/null || echo "")
[ -n "$hard_nofile" ] && ulimit -n "$hard_nofile" 2>/dev/null || true

# Where the Android NDK landed is recorded by scripts/cross-setup.sh, in the
# file scripts/cross.sh reads; neither image exports it, and the pinned NDK
# version belongs to that script rather than here.
[ -f "${WRKZ_TOOLS:-$HOME/.local/wrkz-cross}/env" ] && source "${WRKZ_TOOLS:-$HOME/.local/wrkz-cross}/env"

root=$(cd "$(dirname "$0")/.." && pwd)
app="$root/apps/pluton"
out="$root/dist/pluton"
cd "$app"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
commit=$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo unknown)
name="rust-pluton-wallet-$version-$commit"

# Reproducibility, as scripts/release.sh sets it.
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct 2>/dev/null || echo 0)}"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$root=. --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=~/.cargo"

want() {
    [ "$#" -eq 0 ] && return 1
    for part in "$@"; do
        case " $parts " in *" $part "*) return 0 ;; esac
    done
    return 1
}

parts="${*:-linux windows android web}"
mkdir -p "$out"

pack_dir() {
    # pack_dir <staging dir> <archive name> <zip|tar>
    local stage=$1 archive=$2 kind=$3
    cp "$root/LICENSE" "$stage/"
    find "$stage" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
    rm -f "$out/$archive"
    if [ "$kind" = zip ]; then
        (cd "$(dirname "$stage")" && TZ=UTC zip -Xrq "$out/$archive" "$(basename "$stage")")
    else
        tar --sort=name --owner=0 --group=0 --numeric-owner \
            --mtime="@$SOURCE_DATE_EPOCH" -czf "$out/$archive" \
            -C "$(dirname "$stage")" "$(basename "$stage")"
    fi
    rm -rf "$stage"
    echo "  -> dist/pluton/$archive"
}

# ---- desktop --------------------------------------------------------------
build_desktop() {
    # build_desktop <rust target> <platform name> <exe suffix> <zip|tar>
    local target=$1 platform=$2 suffix=$3 kind=$4
    echo "== $platform"
    case $target in
        x86_64-unknown-linux-gnu) cargo build --release --locked ;;
        aarch64-unknown-linux-gnu) cargo zigbuild --release --locked --target "$target" ;;
        *-windows-gnu) cargo build --release --locked --target "$target" ;;
    esac
    local built="target/release/rust-pluton-wallet$suffix"
    [ "$target" = x86_64-unknown-linux-gnu ] || built="target/$target/release/rust-pluton-wallet$suffix"
    local stage="$out/$name-$platform"
    rm -rf "$stage" && mkdir -p "$stage"
    cp "$built" "$stage/"
    pack_dir "$stage" "$name-$platform.${kind/tar/tar.gz}" "$kind"
}

if want linux; then
    build_desktop x86_64-unknown-linux-gnu linux-x86_64 "" tar
    build_desktop aarch64-unknown-linux-gnu linux-arm64 "" tar
fi

if want windows; then
    build_desktop x86_64-pc-windows-gnu windows-x86_64 ".exe" zip
fi

# ---- android --------------------------------------------------------------
# Both APKs, as asked for: the debug one is debuggable and installs beside the
# release one (its application id ends in .debug), the release one is what
# users get. Each carries the Rust library built at its own profile, so the
# debug APK is the one to attach a debugger to.
if want android; then
    echo "== android"
    : "${ANDROID_HOME:?set ANDROID_HOME (the image from Dockerfile.pluton does)}"
    : "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME; scripts/cross-setup.sh records it when WRKZ_CROSS includes android}"
    export ANDROID_NDK_ROOT="$ANDROID_NDK_HOME"

    jni="$app/android/app/src/main/jniLibs"
    for profile in debug release; do
        rm -rf "$jni"
        mkdir -p "$jni"
        echo "-- rust ($profile)"
        if [ "$profile" = release ]; then
            cargo ndk -t arm64-v8a -t x86_64 -o "$jni" build --release --locked
        else
            cargo ndk -t arm64-v8a -t x86_64 -o "$jni" build --locked
        fi
        echo "-- gradle (assemble${profile^})"
        (cd "$app/android" && gradle --no-daemon "assemble${profile^}")
        apk=$(find "$app/android/app/build/outputs/apk/$profile" -name "*.apk" | head -1)
        cp "$apk" "$out/$name-android-$profile.apk"
        echo "  -> dist/pluton/$name-android-$profile.apk"
    done
    rm -rf "$jni"
fi

# ---- web ------------------------------------------------------------------
if want web; then
    echo "== web"
    cargo build --release --locked --target wasm32-unknown-unknown
    rm -rf "$app/web/pkg"
    wasm-bindgen target/wasm32-unknown-unknown/release/rust_pluton_wallet.wasm \
        --out-dir "$app/web/pkg" --target web --no-typescript

    # The wallet is a big download for a phone: 16.6 MB as wasm-bindgen leaves
    # it, about 6 MB over the wire once the web server compresses it. wasm-opt
    # takes roughly a third off that. Never fatal: a module that it cannot
    # handle still works, it is just larger.
    module="$app/web/pkg/rust_pluton_wallet_bg.wasm"
    if command -v wasm-opt > /dev/null 2>&1; then
        echo "-- wasm-opt"
        before=$(wc -c < "$module")
        if wasm-opt -Oz "$module" -o "$module.opt" 2> /dev/null; then
            mv "$module.opt" "$module"
            echo "   $((before / 1024 / 1024)) MB -> $(( $(wc -c < "$module") / 1024 / 1024 )) MB"
        else
            rm -f "$module.opt"
            echo "   wasm-opt could not read this module; shipping it unoptimised"
        fi
    else
        echo "-- wasm-opt is not installed; shipping the module unoptimised (about a third larger)"
    fi

    stage="$out/$name-web"
    rm -rf "$stage" && mkdir -p "$stage"
    cp -r "$app/web/." "$stage/"
    pack_dir "$stage" "$name-web.tar.gz" tar
fi

echo
echo "Built into dist/pluton:"
ls -1 "$out"
