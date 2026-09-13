// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! What the window and the wallet say to each other.
//!
//! The wallet never runs on the drawing thread: on desktop and Android it is a
//! background thread, in the browser a Web Worker. Both carry these messages —
//! as values through a channel natively, as JSON through `postMessage` in the
//! browser — so the two platforms run the same wallet code and the same
//! window code.
//!
//! Amounts are always atomic units (WRKZ has two decimal places); only
//! [`format_amount`] turns one into something to read.

use serde::{Deserialize, Serialize};

/// The number of decimal places WRKZ is divided into
/// (`CRYPTONOTE_DISPLAY_DECIMAL_POINT`).
pub const DECIMAL_PLACES: u32 = wrkz_primitives::constants::CRYPTONOTE_DISPLAY_DECIMAL_POINT;

/// Atomic units in one WRKZ.
pub const ONE_WRKZ: u64 = 10u64.pow(DECIMAL_PLACES);

/// `12480.37`, with thousands separators and both decimals, as the wallets
/// print amounts.
pub fn format_amount(atomic: u64) -> String {
    let (whole, fraction) = (atomic / ONE_WRKZ, atomic % ONE_WRKZ);
    let digits = whole.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 4);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{grouped}.{fraction:0width$}", width = DECIMAL_PLACES as usize)
}

/// `12480.37` back to atomic units, for what someone types into Send.
pub fn parse_amount(text: &str) -> Option<u64> {
    let text = text.trim().replace([',', ' '], "");
    if text.is_empty() {
        return None;
    }
    let (whole, fraction) = match text.split_once('.') {
        Some((w, f)) => (w, f),
        None => (text.as_str(), ""),
    };
    // Digits only, at most two of them after the point, and at least one
    // somewhere: "1.2.3", "abc", "-1", "1.234" and "." are all not amounts.
    if fraction.len() > DECIMAL_PLACES as usize
        || (whole.is_empty() && fraction.is_empty())
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !fraction.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let units: u64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let mut hundredths: u64 = if fraction.is_empty() { 0 } else { fraction.parse().ok()? };
    // "0.5" is fifty hundredths, "0.05" is five: the *typed* digits set the
    // scale, not the value they parse to.
    for _ in fraction.len()..DECIMAL_PLACES as usize {
        hundredths *= 10;
    }
    units.checked_mul(ONE_WRKZ)?.checked_add(hundredths)
}

/// Whatever carries commands to the wallet: a background thread on a desktop
/// or a phone, a Web Worker in a browser. The window only ever holds one of
/// these, so it does not know which platform it is on.
pub trait WalletHandle {
    fn send(&self, command: Command);
}

/// Where the wallet sends what it has to say. It is called on whichever thread
/// the wallet runs on, so it hops to the drawing thread itself.
pub type EventSink = Box<dyn Fn(Event) + Send>;

/// How a wallet is created or restored.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum NewWallet {
    /// A fresh spend key; the seed is shown once, afterwards.
    Create,
    /// The 25-word mnemonic.
    Seed { seed: String, scan_height: u64 },
    /// The private spend and view keys.
    Keys { spend_key: String, view_key: String, scan_height: u64 },
    /// An address and its private view key: balances and history, no spending.
    ViewOnly { address: String, view_key: String, scan_height: u64 },
}

/// What the wallet is asked to do.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "camelCase")]
pub enum Command {
    /// The wallet files this device holds.
    List,
    /// Create or restore `name`, then open it.
    Create { name: String, password: String, wallet: NewWallet },
    /// Open an existing wallet. `bytes` is how the browser supplies the file
    /// it holds; natively the file is read from the wallet directory.
    Open {
        name: String,
        password: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<Vec<u8>>,
    },
    /// Save and close.
    Close,
    /// Write the file now.
    Save,
    /// Point the wallet at another daemon.
    SetNode { url: String },
    /// Use a proof-of-work server, or nobody (an empty `url`).
    SetPowServer { url: String, api_key: String },
    /// Try a proof-of-work server without saving it.
    TestPowServer { url: String, api_key: String },
    /// Build a transaction and report its fee, without relaying it.
    PrepareSend { address: String, amount: String, payment_id: String, send_all: bool },
    /// Relay what [`Command::PrepareSend`] built.
    ConfirmSend,
    /// Throw away what was prepared.
    CancelSend,
    /// Combine small inputs (a fusion transaction).
    Optimize,
    /// The seed and keys, after the password is given again.
    RevealSecrets { password: String },
    /// Re-encrypt the file under a new password.
    ChangePassword { old: String, new: String },
    /// Scan again from `height`.
    Rescan { height: u64 },
    /// Stop the wallet; the last message before the window closes.
    Shutdown,
}

/// Where syncing has got to.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProgress {
    pub wallet_height: u64,
    pub local_height: u64,
    pub network_height: u64,
    pub synced: bool,
}

impl SyncProgress {
    /// 0 to 100, for the progress bar.
    pub fn percent(&self) -> f32 {
        if self.synced || self.network_height == 0 {
            return 100.0;
        }
        (self.wallet_height as f64 / self.network_height as f64 * 100.0).clamp(0.0, 100.0) as f32
    }
}

/// One row of the history.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionRow {
    pub hash: String,
    /// Signed: what the wallet gained or lost, fee included.
    pub amount: i64,
    pub fee: u64,
    /// `0` while the transaction is still in the pool.
    pub block_height: u64,
    pub timestamp: u64,
    pub payment_id: String,
    pub is_coinbase: bool,
    pub is_fusion: bool,
}

/// What one wallet file looks like from outside.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletSummary {
    pub name: String,
    pub address: String,
    pub is_view_only: bool,
    pub has_seed: bool,
}

/// A transaction built but not yet relayed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedSend {
    pub address: String,
    pub amount: u64,
    pub fee: u64,
    pub ring_size: u64,
    pub payment_id: String,
    /// How the proof of work was found, when one was needed.
    pub pow: Option<String>,
}

/// What the wallet reports back.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum Event {
    /// The wallet files on this device.
    Wallets {
        names: Vec<String>,
    },
    /// A wallet is open.
    Opened {
        summary: WalletSummary,
    },
    /// A wallet was created; its seed is shown once, now.
    Created {
        summary: WalletSummary,
        seed: Option<String>,
    },
    Closed,
    Progress {
        progress: SyncProgress,
    },
    Balance {
        unlocked: u64,
        locked: u64,
    },
    History {
        transactions: Vec<TransactionRow>,
    },
    /// A transaction is ready to confirm.
    SendPrepared {
        prepared: PreparedSend,
    },
    /// It is on its way.
    Sent {
        hash: String,
        fee: u64,
    },
    /// The answer to [`Command::TestPowServer`].
    PowServerProbe {
        ok: bool,
        detail: String,
    },
    /// The seed and keys, after the password was checked.
    Secrets {
        address: String,
        seed: Option<String>,
        spend_key: Option<String>,
        view_key: String,
    },
    /// Something to tell the user, whether or not it stopped anything.
    Notice {
        message: String,
        kind: NoticeKind,
    },
    /// The browser's storage keeps the file for the next visit.
    Persist {
        name: String,
        bytes: Vec<u8>,
    },
    /// The wallet has stopped and the window may close.
    Stopped,
}

/// How loudly a [`Event::Notice`] should be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_read_and_parse_back() {
        assert_eq!(format_amount(1_248_037), "12,480.37");
        assert_eq!(format_amount(0), "0.00");
        assert_eq!(format_amount(5), "0.05");
        assert_eq!(format_amount(100), "1.00");
        assert_eq!(format_amount(u64::MAX), "184,467,440,737,095,516.15");

        for atomic in [0u64, 5, 100, 1_248_037, 999_999_999] {
            assert_eq!(parse_amount(&format_amount(atomic)), Some(atomic), "{atomic}");
        }
        assert_eq!(parse_amount("1"), Some(100));
        assert_eq!(parse_amount(" 1.5 "), Some(150), "one digit is tenths");
        assert_eq!(parse_amount("0.05"), Some(5), "two digits are hundredths");
        assert_eq!(parse_amount("0.50"), Some(50));
        assert_eq!(parse_amount(".5"), Some(50));
        assert_eq!(parse_amount("12,480.37"), Some(1_248_037), "separators are ignored");
        for bad in ["", "1.234", "abc", "1.2.3", "-1", "1e3", ".", "1..2"] {
            assert_eq!(parse_amount(bad), None, "{bad}");
        }
    }

    #[test]
    fn progress_is_a_percentage() {
        let p = SyncProgress { wallet_height: 50, network_height: 100, ..SyncProgress::default() };
        assert_eq!(p.percent(), 50.0);
        assert_eq!(SyncProgress { synced: true, ..SyncProgress::default() }.percent(), 100.0);
        assert_eq!(SyncProgress::default().percent(), 100.0, "nothing known yet");
    }

    /// The browser carries these as JSON; every variant must survive the trip.
    #[test]
    fn commands_and_events_round_trip_as_json() {
        let command = Command::PrepareSend {
            address: "Wrkz…".into(),
            amount: "12.34".into(),
            payment_id: String::new(),
            send_all: false,
        };
        let text = serde_json::to_string(&command).unwrap();
        assert!(text.contains("\"command\":\"prepareSend\""), "{text}");
        assert!(matches!(serde_json::from_str::<Command>(&text).unwrap(), Command::PrepareSend { .. }));

        let event = Event::Balance { unlocked: 1, locked: 2 };
        let text = serde_json::to_string(&event).unwrap();
        assert!(text.contains("\"event\":\"balance\""), "{text}");
        assert!(matches!(serde_json::from_str::<Event>(&text).unwrap(), Event::Balance { unlocked: 1, locked: 2 }));
    }
}
