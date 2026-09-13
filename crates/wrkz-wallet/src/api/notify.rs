// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Transaction notifications: `wrkz-wallet-api --tx-notify`, and the pieces
//! `wrkz-service --tx-notify` / `--tx-confirmed-notify` share.
//!
//! The runner — spec, placeholders, no shell, the queue, the timeout — is
//! [`wrkz_rpc::notify`]. This is what the wallets put into it.
//!
//! # `wrkz-wallet-api`
//!
//! `ApiDispatcher::attachTransactionNotifier` (`ApiDispatcher.cpp:363`)
//! subscribes to `onTransaction`, which `WalletSynchronizer` fires for every
//! transaction it records from a block (`WalletSynchronizer.cpp:407`):
//! incoming, outgoing once it is mined, coinbase and fusion alike. So:
//!
//! - **when**: a sync step recorded the transaction
//!   ([`crate::sync::Synchronizer::last_step_added`]). A send does not fire it;
//!   the send is announced when it is mined.
//! - **placeholders**: `%s` hash, `%h` height, `%a` the transfers' sum (negative
//!   for a send), `%f` fee, `%p` payment id, `%c` always `1`.
//! - **webhook**: `{"event":"tx","hash","height","amount","fee","paymentId",
//!   "confirmed":true,"timestamp","unlockTime","isCoinbase"}`.
//! - **during sync**: a transaction more than [`WALLET_NOTIFY_SYNC_LAG_BLOCKS`]
//!   below the higher of the daemon's two heights is not announced, so a
//!   rescan does not replay a wallet's history, unless `--notify-during-sync`
//!   was given.

use wrkz_rpc::notify::{Field, Notification, Notifier, Options};

use super::OpenWallet;
use crate::file::Transaction;

/// `CryptoNote::WALLET_NOTIFY_SYNC_LAG_BLOCKS` (`config/CryptoNoteConfig.h:537`):
/// about a day of blocks.
pub const WALLET_NOTIFY_SYNC_LAG_BLOCKS: u64 = 1440;

/// A hook named `name` from `spec`: its messages through [`crate::logging`],
/// so `--log-level 0` silences it too, and an `https://` webhook through the
/// build's TLS client.
pub fn hook(name: &str, spec: &str) -> Notifier {
    let options = Options {
        log: Some(crate::logging::notifier_log()),
        post: crate::logging::notifier_post(),
        ..Options::default()
    };
    Notifier::with_options(name, spec, options)
}

/// Whether a transaction in the block at `height` is too far below
/// `daemon_height` to announce while syncing:
/// `blockHeight + WALLET_NOTIFY_SYNC_LAG_BLOCKS < daemonHeight`
/// (`ApiDispatcher.cpp:391`, `WalletService.cpp:783`).
pub fn far_behind(height: u64, daemon_height: u64) -> bool {
    height.saturating_add(WALLET_NOTIFY_SYNC_LAG_BLOCKS) < daemon_height
}

/// The notification both programs send for `tx`: the six placeholders and the
/// eight members of `WalletService::sendTransactionNotification`
/// (`WalletService.cpp:786`). An unconfirmed transaction reports height `0`.
pub fn transaction_notification(event: &str, tx: &Transaction, confirmed: bool) -> Notification {
    let hash = tx.hash.to_hex();
    let height = if confirmed { tx.block_height } else { 0 }.to_string();
    let amount = tx.total_amount().to_string();
    let fee = tx.fee.to_string();
    let flag = if confirmed { "1" } else { "0" };

    Notification {
        event: event.to_string(),
        placeholders: vec![
            ('s', hash.clone()),
            ('h', height.clone()),
            ('a', amount.clone()),
            ('f', fee.clone()),
            ('p', tx.payment_id.clone()),
            ('c', flag.to_string()),
        ],
        fields: vec![
            Field::string("hash", hash),
            Field::raw("height", height),
            Field::raw("amount", amount),
            Field::raw("fee", fee),
            Field::string("paymentId", tx.payment_id.clone()),
            Field::raw("confirmed", if confirmed { "true" } else { "false" }),
            Field::raw("timestamp", tx.timestamp.to_string()),
            Field::raw("unlockTime", tx.unlock_time.to_string()),
        ],
    }
}

/// `wrkz-wallet-api`'s notification (`ApiDispatcher.cpp:397-415`): always
/// confirmed, and with `isCoinbase` after the rest.
pub fn api_notification(tx: &Transaction) -> Notification {
    let mut n = transaction_notification("tx", tx, true);
    n.fields.push(Field::raw("isCoinbase", if tx.is_coinbase_transaction { "true" } else { "false" }));
    n
}

/// Announce what the last sync step of `open` recorded, the way the C++'s
/// `onTransaction` subscriber does. Queues only; never waits on the hook.
pub fn announce_sync_step(notifier: &Notifier, open: &OpenWallet, notify_during_sync: bool) {
    let added = open.sync.last_step_added();
    if added.is_empty() || !notifier.enabled() {
        return;
    }

    // `std::max(localDaemonBlockCount, networkBlockCount)`.
    let status = open.sync.sync_status();
    let daemon_height = status.local_daemon_block_count.max(status.network_block_count);

    for hash in added {
        // Newest last, so a reverse search finds it at once. A block later in
        // the same step may have forked it away again; then there is nothing
        // to describe, and the C++ would have announced a transaction that no
        // longer exists.
        let Some(tx) = open.wallet().transactions().iter().rev().find(|t| t.hash == *hash) else { continue };
        if !notify_during_sync && far_behind(tx.block_height, daemon_height) {
            continue;
        }
        notifier.notify(api_notification(tx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::{Hex32, Transfer};

    fn tx() -> Transaction {
        Transaction {
            block_height: 4_213_000,
            fee: 10_000,
            hash: Hex32([0xab; 32]),
            is_coinbase_transaction: false,
            payment_id: "0102030405060708".into(),
            timestamp: 1_788_894_799,
            transfers: vec![
                Transfer { amount: -500_000, public_key: Hex32([1; 32]) },
                Transfer { amount: 120_000, public_key: Hex32([2; 32]) },
            ],
            unlock_time: 0,
        }
    }

    #[test]
    fn the_api_notification_is_the_cpps() {
        let n = api_notification(&tx());
        let hash = "ab".repeat(32);
        assert_eq!(n.event, "tx");
        assert_eq!(
            n.placeholders,
            vec![
                ('s', hash.clone()),
                ('h', "4213000".to_string()),
                ('a', "-380000".to_string()),
                ('f', "10000".to_string()),
                ('p', "0102030405060708".to_string()),
                ('c', "1".to_string()),
            ]
        );
        assert_eq!(
            wrkz_rpc::notify::build_json(&n),
            format!(
                r#"{{"event":"tx","hash":"{hash}","height":4213000,"amount":-380000,"fee":10000,"paymentId":"0102030405060708","confirmed":true,"timestamp":1788894799,"unlockTime":0,"isCoinbase":false}}"#
            )
        );
    }

    #[test]
    fn an_unconfirmed_notification_reports_height_zero() {
        let n = transaction_notification("tx", &tx(), false);
        assert!(n.placeholders.contains(&('h', "0".to_string())));
        assert!(n.placeholders.contains(&('c', "0".to_string())));
        assert!(wrkz_rpc::notify::build_json(&n).contains(r#""height":0,"#));
        assert!(wrkz_rpc::notify::build_json(&n).contains(r#""confirmed":false,"#));
        assert!(!wrkz_rpc::notify::build_json(&n).contains("isCoinbase"), "the service sends no isCoinbase");
    }

    #[test]
    fn a_transaction_a_day_behind_the_daemon_is_quiet_while_syncing() {
        // `blockHeight + 1440 < daemonHeight`: exactly a day behind still fires.
        assert!(!far_behind(1000, 2440));
        assert!(far_behind(1000, 2441));
        assert!(!far_behind(5000, 10), "a daemon behind the wallet is not a rescan");
        assert!(!far_behind(u64::MAX, u64::MAX));
    }
}
