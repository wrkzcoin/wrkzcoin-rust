// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The window: it fills `State`, answers `Actions`, and never touches a wallet
//! itself. Every command goes to the wallet thread and every [`Event`] comes
//! back onto the drawing thread through `upgrade_in_event_loop`.

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::protocol::{format_amount, Command, Event, EventSink, NewWallet, NoticeKind, TransactionRow, WalletHandle};
use crate::service::{DEFAULT_NODE, DEFAULT_NODE_BROWSER, SUGGESTED_POW_SERVER};
use crate::{Actions, AppWindow, State, TxRow};

/// Open the window and run until it closes. `make_wallet` starts the wallet on
/// whatever this platform runs it on — a thread, or a Web Worker — and is given
/// the sink every event goes to.
pub fn run<F>(make_wallet: F) -> Result<(), slint::PlatformError>
where
    F: FnOnce(EventSink) -> Rc<dyn WalletHandle>,
{
    let ui = AppWindow::new()?;
    let browser = cfg!(target_family = "wasm");
    let state = ui.global::<State>();
    state.set_version(format!("Rust Pluton Wallet {}", env!("CARGO_PKG_VERSION")).into());
    state.set_node_url(if browser { DEFAULT_NODE_BROWSER.into() } else { DEFAULT_NODE.into() });
    state.set_suggested_pow_url(SUGGESTED_POW_SERVER.into());

    // Events arrive wherever the wallet runs; hop to the drawing thread.
    let weak = ui.as_weak();
    let sink: EventSink = Box::new(move |event| {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply(&ui, event);
            }
        });
    });

    let wallet = make_wallet(sink);
    bind(&ui, &wallet);
    wallet.send(Command::List);
    ui.run()
}

/// Point every callback at the wallet.
fn bind(ui: &AppWindow, wallet: &Rc<dyn WalletHandle>) {
    let actions = ui.global::<Actions>();

    let send = |wallet: &Rc<dyn WalletHandle>| Rc::clone(wallet);
    let (w, weak) = (send(wallet), ui.as_weak());
    actions.on_open_wallet(move |name, password| {
        busy(&weak, true);
        w.send(Command::Open { name: name.into(), password: password.into(), bytes: None });
    });

    let (w, weak) = (send(wallet), ui.as_weak());
    actions.on_create_wallet(move |name, password, kind, secret, second, scan_height| {
        let scan_height = scan_height.max(0) as u64;
        let new = match kind.as_str() {
            "seed" => NewWallet::Seed { seed: secret.into(), scan_height },
            "keys" => NewWallet::Keys { spend_key: secret.into(), view_key: second.into(), scan_height },
            "view" => NewWallet::ViewOnly { address: second.into(), view_key: secret.into(), scan_height },
            _ => NewWallet::Create,
        };
        busy(&weak, true);
        w.send(Command::Create { name: name.into(), password: password.into(), wallet: new });
    });

    let w = send(wallet);
    actions.on_close_wallet(move || w.send(Command::Close));

    let (w, weak) = (send(wallet), ui.as_weak());
    actions.on_prepare_send(move |address, amount, payment_id, send_all| {
        busy(&weak, true);
        w.send(Command::PrepareSend {
            address: address.into(),
            amount: amount.into(),
            payment_id: payment_id.into(),
            send_all,
        });
    });

    let (w, weak) = (send(wallet), ui.as_weak());
    actions.on_confirm_send(move || {
        busy(&weak, true);
        if let Some(ui) = weak.upgrade() {
            ui.global::<State>().set_confirm_visible(false);
        }
        w.send(Command::ConfirmSend);
    });

    let (w, weak) = (send(wallet), ui.as_weak());
    actions.on_cancel_send(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<State>().set_confirm_visible(false);
        }
        w.send(Command::CancelSend);
    });

    let (w, weak) = (send(wallet), ui.as_weak());
    actions.on_optimize(move || {
        busy(&weak, true);
        w.send(Command::Optimize);
    });

    let w = send(wallet);
    actions.on_set_node(move |url| w.send(Command::SetNode { url: url.into() }));

    let w = send(wallet);
    actions.on_set_pow_server(move |url, key| w.send(Command::SetPowServer { url: url.into(), api_key: key.into() }));

    let w = send(wallet);
    actions.on_test_pow_server(move |url, key| w.send(Command::TestPowServer { url: url.into(), api_key: key.into() }));

    let w = send(wallet);
    actions.on_reveal_secrets(move |password| w.send(Command::RevealSecrets { password: password.into() }));

    let w = send(wallet);
    actions.on_change_password(move |old, new| w.send(Command::ChangePassword { old: old.into(), new: new.into() }));

    let w = send(wallet);
    actions.on_rescan(move |height| w.send(Command::Rescan { height: height.max(0) as u64 }));

    let weak = ui.as_weak();
    actions.on_dismiss_notice(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<State>().set_notice(SharedString::new());
        }
    });
}

fn busy(weak: &slint::Weak<AppWindow>, busy: bool) {
    if let Some(ui) = weak.upgrade() {
        ui.global::<State>().set_busy(busy);
    }
}

/// Show one event.
fn apply(ui: &AppWindow, event: Event) {
    let state = ui.global::<State>();
    match event {
        Event::Wallets { names } => {
            let names: Vec<SharedString> = names.into_iter().map(SharedString::from).collect();
            state.set_wallets(ModelRc::new(VecModel::from(names)));
        }
        Event::Opened { summary } => {
            state.set_busy(false);
            state.set_wallet_open(true);
            state.set_wallet_name(summary.name.into());
            state.set_address(summary.address.into());
            state.set_view_only(summary.is_view_only);
            state.set_page("overview".into());
        }
        Event::Created { summary, seed } => {
            state.set_busy(false);
            state.set_wallet_open(true);
            state.set_wallet_name(summary.name.into());
            state.set_address(summary.address.into());
            state.set_view_only(summary.is_view_only);
            state.set_page("overview".into());
            // The one moment a seed is shown; the overlay covers the window
            // until it is dismissed.
            state.set_new_seed(seed.unwrap_or_default().into());
        }
        Event::Closed => {
            state.set_wallet_open(false);
            state.set_page("wallets".into());
            state.set_address(SharedString::new());
            state.set_transactions(ModelRc::new(VecModel::from(Vec::<TxRow>::new())));
        }
        Event::Progress { progress } => {
            state.set_sync_percent(progress.percent());
            state.set_synced(progress.synced);
            state.set_sync_text(
                if progress.synced {
                    "Synced".to_string()
                } else if progress.network_height == 0 {
                    "Connecting…".to_string()
                } else {
                    format!("Block {} of {}", progress.wallet_height, progress.network_height)
                }
                .into(),
            );
        }
        Event::Balance { unlocked, locked } => {
            state.set_unlocked(format_amount(unlocked).into());
            state.set_locked(format_amount(locked).into());
        }
        Event::History { transactions } => {
            let rows: Vec<TxRow> = transactions.iter().map(row).collect();
            state.set_transactions(ModelRc::new(VecModel::from(rows)));
        }
        Event::SendPrepared { prepared } => {
            state.set_busy(false);
            state.set_confirm_address(prepared.address.into());
            state.set_confirm_amount(format_amount(prepared.amount).into());
            state.set_confirm_fee(format_amount(prepared.fee).into());
            state.set_confirm_ring(prepared.ring_size.to_string().into());
            state.set_confirm_pow(prepared.pow.unwrap_or_default().into());
            state.set_confirm_visible(true);
        }
        Event::Sent { hash, fee } => {
            state.set_busy(false);
            notice(ui, format!("Sent. Fee {} WRKZ. Transaction {hash}", format_amount(fee)), NoticeKind::Info);
        }
        Event::PowServerProbe { ok, detail } => {
            state.set_pow_result(if ok { format!("Works. {detail}") } else { format!("No: {detail}") }.into());
        }
        Event::Secrets { address, seed, spend_key, view_key } => {
            state.set_secret_seed(seed.unwrap_or_else(|| "This wallet has no seed.".into()).into());
            state.set_secret_spend(spend_key.unwrap_or_else(|| "None (view-only).".into()).into());
            state.set_secret_view(view_key.into());
            state.set_address(address.into());
            state.set_secrets_visible(true);
        }
        Event::Notice { message, kind } => {
            state.set_busy(false);
            notice(ui, message, kind);
        }
        Event::Persist { .. } | Event::Stopped => {}
    }
}

fn notice(ui: &AppWindow, message: String, kind: NoticeKind) {
    let state = ui.global::<State>();
    state.set_notice(message.into());
    state.set_notice_kind(
        match kind {
            NoticeKind::Info => "info",
            NoticeKind::Warning => "warning",
            NoticeKind::Error => "error",
        }
        .into(),
    );
}

/// One history row, as the window shows it.
fn row(tx: &TransactionRow) -> TxRow {
    let incoming = tx.amount >= 0;
    let amount = format_amount(tx.amount.unsigned_abs());
    TxRow {
        hash: short_hash(&tx.hash).into(),
        when: when(tx.timestamp).into(),
        amount: format!("{}{amount}", if incoming { "+" } else { "−" }).into(),
        incoming,
        status: if tx.is_fusion {
            "Optimized".into()
        } else if tx.is_coinbase {
            "Mined".into()
        } else if tx.block_height == 0 {
            SharedString::from("Pending")
        } else if incoming {
            "Received".into()
        } else {
            "Sent".into()
        },
    }
}

fn short_hash(hash: &str) -> String {
    if hash.len() > 12 {
        format!("{}…{}", &hash[..6], &hash[hash.len() - 4..])
    } else {
        hash.to_string()
    }
}

/// A block timestamp as a date, without a date crate (the same civil-date
/// arithmetic the proof-of-work server logs with).
fn when(timestamp: u64) -> String {
    if timestamp == 0 {
        return "—".into();
    }
    let (days, rem) = (timestamp / 86_400, timestamp % 86_400);
    let (hour, minute) = (rem / 3600, rem % 3600 / 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_is_shortened_and_a_timestamp_is_a_date() {
        assert_eq!(short_hash("7c1ea9f3b2c4d5e6f70819"), "7c1ea9…0819");
        assert_eq!(short_hash("abc"), "abc");
        // The genesis timestamp of this chain.
        assert_eq!(when(1_529_831_318), "2018-06-24 09:08");
        assert_eq!(when(0), "—");
    }
}
