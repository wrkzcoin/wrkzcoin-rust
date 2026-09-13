// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `--tx-notify`, `--tx-confirmed-notify` and `--notify-during-sync`:
//! `WalletService::onTransactionEvent` (`walletservice/WalletService.cpp:829`)
//! over the modern container.
//!
//! The hooks themselves are [`wrkz_rpc::notify`]; the notification is
//! [`wrkz_wallet::api::notify::transaction_notification`], with the C++'s six
//! placeholders (`%s %h %a %f %p %c`) and eight webhook members.
//!
//! # When each fires
//!
//! The C++ reacts to `WalletGreen`'s transaction events. The modern container
//! has no such events, so the same decisions are taken at the two places the
//! service changes it — a relayed send, and a sync step:
//!
//! - **a send** (`sendTransaction`, `sendDelayedTransaction`) fires `tx` once
//!   it has been relayed, with height `0` and `%c` `0` — the C++ announces an
//!   outgoing transfer when it leaves the `CREATED` state
//!   (`WalletService.cpp:899`). The hash is remembered.
//! - **a sync step** that records a remembered hash has seen that send mined:
//!   `tx_confirmed` fires, and `tx` does not fire again (`:917-942`).
//! - **a sync step** that records any other transaction has found a new, mined
//!   one: `tx`, then `tx_confirmed` at once, since it is already in a block
//!   (`announce`, `:856`) — unless it is more than
//!   [`WALLET_NOTIFY_SYNC_LAG_BLOCKS`](wrkz_wallet::api::notify::WALLET_NOTIFY_SYNC_LAG_BLOCKS)
//!   below the daemon's height and `--notify-during-sync` was not given
//!   (`shouldNotifyTransaction`, `:768`).
//!
//! A send still waiting for its block when the service starts is remembered
//! too, so it is not announced as new when it confirms; its `tx_confirmed`
//! does fire, where the C++ — which starts with an empty
//! `m_awaitingConfirmation` — would say nothing at all.
//!
//! What the modern container cannot give: WalletGreen also sees *incoming*
//! transactions while they are in the pool and fires `tx` for them with height
//! `0`. The modern container does not follow other people's pool transactions,
//! so an incoming transaction is announced when it is mined.

use std::collections::HashSet;
use std::sync::Mutex;

use wrkz_rpc::log::Level;
use wrkz_rpc::notify::Notifier;
use wrkz_wallet::api::notify::{far_behind, hook, transaction_notification};
use wrkz_wallet::api::OpenWallet;
use wrkz_wallet::file::{Hash, Wallet};

/// The service's two transaction hooks and what they remember between events.
pub struct TxNotifiers {
    tx: Option<Notifier>,
    confirmed: Option<Notifier>,
    during_sync: bool,
    /// `m_pendingSend` and `m_awaitingConfirmation` in one: sends this service
    /// has announced that no block has carried yet.
    sent: Mutex<HashSet<Hash>>,
}

impl TxNotifiers {
    /// `WalletService::initNotifiers` (`WalletService.cpp:730`): a hook for
    /// each spec that is not empty, over `wallet`'s unconfirmed sends.
    pub fn new(tx_spec: &str, confirmed_spec: &str, during_sync: bool, wallet: &Wallet) -> TxNotifiers {
        let hook_for = |name: &str, spec: &str| (!spec.trim().is_empty()).then(|| hook(name, spec));
        let notifiers = TxNotifiers {
            tx: hook_for("tx-notify", tx_spec),
            confirmed: hook_for("tx-confirmed-notify", confirmed_spec),
            during_sync,
            sent: Mutex::new(wallet.sub_wallets.locked_transactions.iter().map(|t| t.hash).collect()),
        };
        if notifiers.enabled() {
            let when = if during_sync { "including during sync" } else { "suppressed while far behind the daemon" };
            wrkz_wallet::logging::log(Level::Info, format_args!("Transaction notifications enabled ({when})"));
        }
        notifiers
    }

    /// No hooks at all.
    pub fn none() -> TxNotifiers {
        TxNotifiers { tx: None, confirmed: None, during_sync: false, sent: Mutex::new(HashSet::new()) }
    }

    /// Whether either hook will deliver anything.
    pub fn enabled(&self) -> bool {
        self.new_hook().is_some() || self.confirmed_hook().is_some()
    }

    fn new_hook(&self) -> Option<&Notifier> {
        self.tx.as_ref().filter(|n| n.enabled())
    }

    fn confirmed_hook(&self) -> Option<&Notifier> {
        self.confirmed.as_ref().filter(|n| n.enabled())
    }

    fn remembered(&self) -> std::sync::MutexGuard<'_, HashSet<Hash>> {
        self.sent.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A send has been relayed and recorded in `open` under `hash`.
    pub fn sent(&self, open: &OpenWallet, hash: &Hash) {
        if !self.enabled() {
            return;
        }
        let wallet = open.wallet();
        let Some(tx) = wallet.sub_wallets.locked_transactions.iter().find(|t| t.hash == *hash) else { return };
        if let Some(hook) = self.new_hook() {
            hook.notify(transaction_notification("tx", tx, false));
        }
        self.remembered().insert(*hash);
    }

    /// `open` has just finished a sync step.
    pub fn sync_step(&self, open: &OpenWallet) {
        if !self.enabled() {
            return;
        }
        let wallet = open.wallet();
        let mut sent = self.remembered();

        for hash in open.sync.last_step_added() {
            let Some(tx) = wallet.transactions().iter().rev().find(|t| t.hash == *hash) else { continue };

            if sent.remove(hash) {
                // One of ours, now in a block: the confirmation, and only once.
                if let Some(hook) = self.confirmed_hook() {
                    hook.notify(transaction_notification("tx_confirmed", tx, true));
                }
                continue;
            }

            if !self.during_sync && far_behind(tx.block_height, open.network_height()) {
                continue;
            }
            if let Some(hook) = self.new_hook() {
                hook.notify(transaction_notification("tx", tx, true));
            }
            if let Some(hook) = self.confirmed_hook() {
                hook.notify(transaction_notification("tx_confirmed", tx, true));
            }
        }

        // A send the daemon never heard of, or one forked out and dropped, will
        // not confirm; do not remember it for ever.
        let locked = &wallet.sub_wallets.locked_transactions;
        sent.retain(|h| locked.iter().any(|t| t.hash == *h));
    }
}
