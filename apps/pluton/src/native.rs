// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Desktop and Android: wallet files in a directory, and the wallet itself on
//! a background thread. The window sends [`Command`]s down a channel and is
//! handed [`Event`]s back through a callback, which the user interface turns
//! into a hop onto the drawing thread.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use wrkz_wallet::http::UreqTransport;

use crate::protocol::{Command, Event, WalletHandle};
use crate::service::{Service, Storage};

/// The folder holding the wallet files and `settings.json`.
///
/// A wallet called `main` is `main.wallet`, the name the command-line wallets
/// use, so the same file opens in either.
pub struct FileStorage {
    dir: PathBuf,
}

impl FileStorage {
    /// Wallets under `dir`, which is created if it is not there yet.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);
        FileStorage { dir }
    }

    /// Where a desktop keeps them: `%APPDATA%\RustPlutonWallet\wallets` on
    /// Windows, `~/Library/Application Support/…` on macOS,
    /// `~/.local/share/…` on Linux. Android passes its own directory instead.
    #[cfg(not(any(target_os = "android", target_family = "wasm")))]
    pub fn default_dir() -> PathBuf {
        dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("RustPlutonWallet").join("wallets")
    }

    /// `settings.json` keeps its name; a wallet gets the `.wallet` suffix.
    fn path(&self, name: &str) -> PathBuf {
        if name.ends_with(".json") {
            self.dir.join(name)
        } else {
            self.dir.join(format!("{name}.wallet"))
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Storage for FileStorage {
    fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "wallet"))
            .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names
    }

    fn load(&self, name: &str) -> Result<Vec<u8>, String> {
        std::fs::read(self.path(name)).map_err(|e| e.to_string())
    }

    /// Through `<file>.tmp` and a rename, as the wallet's own `save` does: an
    /// interrupted write would otherwise leave a file nothing can open.
    fn save(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.path(name);
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.to_string());
        }
        Ok(())
    }

    fn exists(&self, name: &str) -> bool {
        self.path(name).exists()
    }
}

/// The running wallet thread. Dropping it asks the wallet to stop and waits
/// for it, so a wallet is always saved before the process ends.
pub struct WalletThread {
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl WalletThread {
    /// Start the wallet on its own thread. Every [`Event`] it produces is
    /// handed to `on_event`, on that thread.
    pub fn start<F>(dir: PathBuf, on_event: F) -> Self
    where
        F: Fn(Event) + Send + 'static,
    {
        let (commands, inbox) = channel::<Command>();
        let thread = std::thread::Builder::new()
            .name("wallet".into())
            .spawn(move || {
                // The daemon transport allows a 32 MiB sync response; the
                // proof-of-work server's is small and follows no redirects.
                let mut service = Service::new(
                    UreqTransport::for_daemon(),
                    UreqTransport::for_pow_server(),
                    FileStorage::new(dir),
                    false,
                );
                let mut wait = Duration::from_millis(250);
                loop {
                    match inbox.recv_timeout(wait) {
                        Ok(command) => {
                            let stopping = matches!(command, Command::Shutdown);
                            for event in service.handle(command) {
                                on_event(event);
                            }
                            if stopping {
                                return;
                            }
                            // Answer whatever else is waiting before syncing again.
                            wait = Duration::from_millis(1);
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            let (events, next) = service.tick();
                            for event in events {
                                on_event(event);
                            }
                            // Never spin: even a zero wait yields, so a command
                            // that arrives meanwhile is answered first.
                            wait = next.max(Duration::from_millis(10));
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("spawn the wallet thread");
        WalletThread { commands, thread: Some(thread) }
    }

    /// Ask the wallet to do something. Fails only once it has stopped.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl WalletHandle for WalletThread {
    fn send(&self, command: Command) {
        WalletThread::send(self, command);
    }
}

impl Drop for WalletThread {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pluton-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn wallets_are_files_in_the_folder_and_settings_keeps_its_name() {
        let dir = temp_dir("storage");
        let mut storage = FileStorage::new(&dir);
        assert!(storage.list().is_empty());

        storage.save("main", b"wallet bytes").unwrap();
        storage.save("settings.json", b"{}").unwrap();
        assert!(dir.join("main.wallet").exists(), "the command-line wallets' own name");
        assert!(dir.join("settings.json").exists());

        assert_eq!(storage.list(), ["main"], "settings are not a wallet");
        assert_eq!(storage.load("main").unwrap(), b"wallet bytes");
        assert!(storage.exists("main") && !storage.exists("other"));
        assert!(storage.load("other").is_err());

        // A second save replaces the file rather than appending to it.
        storage.save("main", b"new bytes").unwrap();
        assert_eq!(storage.load("main").unwrap(), b"new bytes");
        assert!(!dir.join("main.wallet.tmp").exists(), "the temporary file is renamed away");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
