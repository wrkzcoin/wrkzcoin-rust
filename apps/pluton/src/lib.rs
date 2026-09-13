// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Rust Pluton Wallet: one wallet for Windows, macOS, Linux, Android and the
//! browser, on the Rust wallet core (`wrkz-wallet`).
//!
//! The wallet never runs on the thread that draws: on desktop and Android it
//! runs on a background thread, in the browser inside a Web Worker, where a
//! blocking request is allowed. The user interface talks to it in messages, so
//! syncing and a proof-of-work search never freeze the window.

pub mod protocol;
pub mod service;

/// Desktop and Android: files in a folder, and the wallet on a thread.
#[cfg(not(target_family = "wasm"))]
pub mod native;
pub mod ui;
/// The browser: the wallet in a Web Worker, files in the browser's storage.
#[cfg(target_family = "wasm")]
pub mod web;

slint::include_modules!();

/// Open the window and run until it closes, with the wallets in this user's
/// own data folder.
#[cfg(not(any(target_os = "android", target_family = "wasm")))]
pub fn run() -> Result<(), slint::PlatformError> {
    let dir = native::FileStorage::default_dir();
    ui::run(move |sink| {
        std::rc::Rc::new(native::WalletThread::start(dir, sink)) as std::rc::Rc<dyn protocol::WalletHandle>
    })
}

/// The entry point the page calls once the module is loaded.
#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    ui::run(web::WalletWorker::start).expect("the window runs");
}

/// The entry point Android's activity calls. The wallets live in the app's
/// private storage, which nothing else on the phone can read.
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::AndroidApp) {
    let dir = app.internal_data_path().unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp")).join("wallets");
    slint::android::init(app).expect("the Android backend starts");
    ui::run(move |sink| {
        std::rc::Rc::new(native::WalletThread::start(dir, sink)) as std::rc::Rc<dyn protocol::WalletHandle>
    })
    .expect("the window runs");
}
