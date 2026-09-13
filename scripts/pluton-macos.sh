#!/usr/bin/env bash
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

# Rust Pluton Wallet for macOS. Run this on a Mac: a Mac application needs
# Apple's own SDK to build against and a Mac to sign on, which is why it is not
# in scripts/pluton-build.sh with the others.
#
#   scripts/pluton-macos.sh                 # this Mac's architecture
#   scripts/pluton-macos.sh universal       # both, in one application
#
# The result is dist/pluton/Pluton.app and a .tar.gz beside it. It is not
# signed or notarized: macOS will refuse to open it until you either sign it
# with your own Developer ID or right-click -> Open once.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
app="$root/apps/pluton"
out="$root/dist/pluton"
cd "$app"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
commit=$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo unknown)
bundle="$out/Pluton.app"

case "${1:-native}" in
    universal)
        rustup target add x86_64-apple-darwin aarch64-apple-darwin
        cargo build --release --locked --target x86_64-apple-darwin
        cargo build --release --locked --target aarch64-apple-darwin
        binary="$app/target/pluton-universal"
        lipo -create -output "$binary" \
            target/x86_64-apple-darwin/release/rust-pluton-wallet \
            target/aarch64-apple-darwin/release/rust-pluton-wallet
        ;;
    *)
        cargo build --release --locked
        binary="$app/target/release/rust-pluton-wallet"
        ;;
esac

rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$binary" "$bundle/Contents/MacOS/Pluton"
chmod +x "$bundle/Contents/MacOS/Pluton"
cp "$root/LICENSE" "$bundle/Contents/Resources/"

cat > "$bundle/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Pluton</string>
    <key>CFBundleDisplayName</key><string>Rust Pluton Wallet</string>
    <key>CFBundleIdentifier</key><string>work.wrkz.rustpluton</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleExecutable</key><string>Pluton</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

archive="rust-pluton-wallet-$version-$commit-macos.tar.gz"
tar -czf "$out/$archive" -C "$out" "Pluton.app"
echo "Built $bundle and dist/pluton/$archive"
echo "Unsigned: sign it with your Developer ID, or open it once with right-click -> Open."
