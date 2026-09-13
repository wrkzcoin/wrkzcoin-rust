// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Rust Pluton Wallet on the desktop.

// No console window behind the wallet on Windows.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(any(target_os = "android", target_family = "wasm")))]
fn main() -> Result<(), slint::PlatformError> {
    rust_pluton_wallet::run()
}

/// Android loads the library through its activity and a browser loads it as a
/// module; neither starts a program here, but cargo still builds this file for
/// them.
#[cfg(any(target_os = "android", target_family = "wasm"))]
fn main() {}
