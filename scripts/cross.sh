#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# Build for another platform from a Linux host, with the toolchain chosen by
# the target. Run it, or source it for its functions (scripts/release.sh does).
#
#   scripts/cross.sh <target> [cargo build arguments]
#   scripts/cross.sh x86_64-pc-windows-gnu --release --features rocksdb -p wrkz-node --bin wrkz-node
#   scripts/cross.sh aarch64-apple-darwin --release -p wrkz-wallet --bins
#
# Targets, and what builds them (scripts/cross-setup.sh installs each):
#
#   the host                        cargo (needs the build machine's glibc or
#                                   newer; WRKZ_HOST_ZIG=1 builds it like
#                                   the other Linux targets instead)
#   x86_64-pc-windows-gnu           cargo + MinGW-w64 (posix threads), static
#   x86_64-apple-darwin,
#   aarch64-apple-darwin            cargo zigbuild: zig compiles the C/C++
#                                   and links, so no Apple SDK is needed
#   *-unknown-linux-gnu             cargo zigbuild, against glibc $WRKZ_GLIBC
#   *-linux-musl*                   cargo zigbuild
#   *-linux-android*                cargo ndk, API level $WRKZ_ANDROID_API
#
# Knobs, all optional:
#   WRKZ_GLIBC=2.28          oldest glibc a zig-built Linux binary runs on
#   WRKZ_ANDROID_API=24      Android API level (24 = Android 7.0)
#   BINDGEN_EXTRA_CLANG_ARGS_<target_with_underscores>
#                            extra clang flags for the RocksDB bindings, if a
#                            target's headers are ever not found
#
# A --release build is stripped by the linker (CARGO_PROFILE_RELEASE_STRIP),
# except on macOS: zig ad-hoc signs an arm64 Mach-O as it links it, which
# Apple silicon insists on, and stripping afterwards would break that
# signature. Both macOS targets need macOS 13 or newer (zig 0.15's floor).
# Output lands where cargo puts it: target/<target>/<profile>/.

# shellcheck disable=SC1091
[ -f "${CARGO_HOME:-$HOME/.cargo}/env" ] && source "${CARGO_HOME:-$HOME/.cargo}/env"
# shellcheck disable=SC1091
[ -f "${WRKZ_TOOLS:-$HOME/.local/wrkz-cross}/env" ] && source "${WRKZ_TOOLS:-$HOME/.local/wrkz-cross}/env"

cross_host() {
    rustc -vV | sed -n 's/^host: //p'
}

# What a binary's file name ends with on `target`.
cross_exe_suffix() {
    case $1 in
        *-windows-*) echo ".exe" ;;
        *) echo "" ;;
    esac
}

# Is the tool on PATH? If not, say which setup part provides it.
cross_need() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "cross.sh: $1 not found; run: WRKZ_CROSS=\"$2\" scripts/cross-setup.sh" >&2
        return 1
    fi
}

# cross_build <target> [cargo build arguments]
cross_build() {
    local target=$1
    shift
    local host under
    host=$(cross_host)
    under=${target//-/_}
    (
        set -euo pipefail
        if [ "$target" = "$host" ] && [ "${WRKZ_HOST_ZIG:-0}" != 1 ]; then
            export CARGO_PROFILE_RELEASE_STRIP=symbols
            set -x
            cargo build --target "$target" "$@"
            exit 0
        fi
        case $target in
            x86_64-pc-windows-gnu)
                cross_need x86_64-w64-mingw32-gcc-posix windows
                # The posix-threads MinGW: RocksDB needs std::thread and
                # std::mutex, which the win32-threads variant lacks.
                export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc-posix
                export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++-posix
                export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
                export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc-posix
                # RocksDB 8.10's options/offpeak_time_info.h uses int64_t
                # without <cstdint>, which MinGW's libstdc++ (GCC 13 and on)
                # no longer brings in through <string>.
                export CXXFLAGS_x86_64_pc_windows_gnu="${CXXFLAGS_x86_64_pc_windows_gnu:-} -include cstdint"
                # An .exe that runs on a bare Windows without MinGW DLLs
                # beside it. -static covers libgcc and winpthread, but not
                # the C++ runtime: the `cc` crate asks for stdc++ as a dylib,
                # rustc links dylibs with -Bdynamic, and libstdc++.dll.a wins
                # (wrkz-node.exe then imported libstdc++-6.dll and failed on
                # whichever copy Windows found). static:-bundle takes
                # libstdc++.a at the final link, without copying it into the
                # rlib. scripts/release.sh checks the result.
                export CXXSTDLIB_x86_64_pc_windows_gnu="static:-bundle=stdc++"
                export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-static"
                export CARGO_PROFILE_RELEASE_STRIP=symbols
                set -x
                cargo build --target "$target" "$@"
                ;;
            *-apple-darwin)
                cross_need cargo-zigbuild macos
                cross_need zig macos
                # The oldest macOS is zig's to decide: zig 0.15 links for
                # macOS 13.0 and raises any lower MACOSX_DEPLOYMENT_TARGET to
                # it, so no knob is offered that would only appear to work.
                set -x
                cargo zigbuild --target "$target" "$@"
                ;;
            *-linux-android*)
                cross_need cargo-ndk android
                if [ -z "${ANDROID_NDK_HOME:-}" ]; then
                    echo "cross.sh: ANDROID_NDK_HOME is not set; run: WRKZ_CROSS=android scripts/cross-setup.sh" >&2
                    exit 1
                fi
                # The C++ runtime linked in, not libc++_shared.so shipped
                # beside every binary (the `cc` crate's Android default).
                # libc++_static carries no ABI layer, hence -lc++abi.
                export "CXXSTDLIB_${under}=c++_static"
                export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-lc++abi"
                export CARGO_PROFILE_RELEASE_STRIP=symbols
                # The RocksDB bindings: the NDK's headers refuse clang's
                # unversioned triple ("Unversioned target triples are not
                # supported!"), and cargo-ndk gives bindgen only a sysroot.
                # bindgen reads BINDGEN_EXTRA_CLANG_ARGS_<target> with hyphens
                # before the underscored one cargo-ndk sets, so this one is
                # used instead: cargo-ndk's sysroot and include, plus the API
                # level in the target, as cargo-ndk passes it to the C compiler.
                local api=${WRKZ_ANDROID_API:-24}
                local sysroot="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/sysroot"
                local clang_triple=$target sysroot_triple=$target
                if [ "$target" = armv7-linux-androideabi ]; then
                    clang_triple=armv7a-linux-androideabi
                    sysroot_triple=arm-linux-androideabi
                fi
                set -x
                env "BINDGEN_EXTRA_CLANG_ARGS_$target=--target=$clang_triple$api --sysroot=$sysroot -I$sysroot/usr/include/$sysroot_triple" \
                    cargo ndk --platform "$api" --target "$target" build "$@"
                ;;
            *-linux-gnu* | *-linux-musl*)
                cross_need cargo-zigbuild linux-arm64
                cross_need zig linux-arm64
                # zig ships libc++, not libstdc++; the `cc` crate would ask
                # for the latter on a -gnu target.
                export "CXXSTDLIB_${under}=c++"
                export CARGO_PROFILE_RELEASE_STRIP=symbols
                local zig_target=$target
                case $target in
                    *-linux-gnu | *-linux-gnueabihf) zig_target="$target.${WRKZ_GLIBC:-2.28}" ;;
                esac
                set -x
                cargo zigbuild --target "$zig_target" "$@"
                ;;
            *)
                echo "cross.sh: no recipe for $target" >&2
                exit 2
                ;;
        esac
    )
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    set -euo pipefail
    if [ $# -lt 1 ]; then
        sed -n '2,/^$/s/^# \{0,1\}//p' "$0"
        exit 2
    fi
    cd "$(dirname "$0")/.."
    cross_build "$@"
fi
