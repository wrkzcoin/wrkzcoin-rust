// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The published copy of the open wallet that every read-only route answers
//! from.
//!
//! # Why a copy
//!
//! The wallet is changed by three kinds of work, and two of them are slow: the
//! sync thread holds it across a daemon round trip while it downloads and
//! applies a batch of blocks, and a send spends seconds on the transaction
//! proof of work and more on the daemon. In the C++ neither blocks a reader:
//! `ApiDispatcher::middleware` takes only a shared lock for everything but
//! opening and closing (`ApiDispatcher.cpp:512-526`), sends serialise on
//! `WalletBackend`'s own transaction mutex, and the synchroniser applies blocks
//! under fine-grained locks of its own.
//!
//! Here the synchroniser owns the container, so a step cannot be split around
//! its round trip without rewriting the wallet core. Instead every change —
//! each sync step that moved anything, each write route, each relayed send —
//! ends by publishing a fresh [`WalletView`], and the read-only routes (status,
//! balances, addresses, transactions, keys, node, save, export) read the last
//! one. Publishing is swapping an [`Arc`](std::sync::Arc) under a lock nobody
//! holds for longer than that swap, so a reader never waits for a daemon, a
//! proof of work or another request.
//!
//! What a reader sees is exactly the state after the last completed change:
//! never a sync step half applied, never a send half recorded. It is the same
//! consistency the C++'s shared lock gives, one step coarser — a status read
//! while a step is in flight reports the step before it, which is what the C++
//! would report a moment earlier too.
//!
//! The price is one clone of the container per published change. The sync
//! thread publishes only when a step changed something a route can see
//! ([`Fingerprint`]), so a synced wallet idling at the tip copies nothing.

use zeroize::Zeroizing;

use super::{DynDaemon, OpenWallet};
use crate::file::{Result, Wallet};
use crate::sync::SyncStatus;

/// The open wallet as it stood after the last completed change.
pub struct WalletView {
    wallet: Wallet,
    status: SyncStatus,
    lite_start_height: u64,
    /// `Nigel::peerCount`.
    pub peer_count: u64,
    /// `Nigel::hashrate`.
    pub hashrate: u64,
    /// Where sync stopped at a lite node's floor, when it has.
    pub sync_gap: Option<(u64, u64)>,
    pub daemon_host: String,
    pub daemon_port: u16,
    pub daemon_ssl: bool,
    filename: String,
    /// For `PUT /save`, which writes this copy rather than waiting for the
    /// working one; zeroized when the view is dropped.
    password: Zeroizing<String>,
    /// The synchroniser's daemon, shared, so a send can fetch decoys and relay
    /// without holding the working wallet.
    daemon: DynDaemon,
}

impl WalletView {
    /// Copy what the routes read out of `open`.
    pub fn of(open: &OpenWallet) -> WalletView {
        WalletView {
            wallet: open.wallet().clone(),
            status: open.sync.sync_status(),
            lite_start_height: open.sync.daemon_state().lite_start_height,
            peer_count: open.peer_count,
            hashrate: open.hashrate,
            sync_gap: open.sync_gap,
            daemon_host: open.daemon_host.clone(),
            daemon_port: open.daemon_port,
            daemon_ssl: open.daemon_ssl,
            filename: open.filename.clone(),
            password: open.password.clone(),
            daemon: open.sync.daemon().clone(),
        }
    }

    /// The container.
    pub fn wallet(&self) -> &Wallet {
        &self.wallet
    }

    /// `WalletBackend::getSyncStatus`.
    pub fn sync_status(&self) -> SyncStatus {
        self.status
    }

    /// The height every balance and every send is measured against.
    pub fn network_height(&self) -> u64 {
        self.status.network_block_count
    }

    /// `Nigel::liteStartHeight`.
    pub fn lite_start_height(&self) -> u64 {
        self.lite_start_height
    }

    /// `WalletBackend::getTotalBalance`: unlocked and locked at the network
    /// height.
    pub fn total_balance(&self) -> (u64, u64) {
        self.wallet.balance(self.status.network_block_count)
    }

    /// The daemon the wallet syncs from.
    pub fn daemon(&self) -> &DynDaemon {
        &self.daemon
    }

    /// `WalletBackend::save`, of this copy.
    pub fn save(&self) -> Result<()> {
        let _one = super::one_save_at_a_time();
        let saved = self.wallet.save(&self.filename, &self.password);
        super::log_failed_save(&self.filename, &saved);
        saved
    }
}

/// The parts of an open wallet a route can observe that a sync step may
/// change, cheap to compare. Two equal fingerprints mean there is nothing new
/// to publish.
///
/// Every way a step changes what a route shows moves one of these: applying a
/// block moves the height, and a transaction found, spent or confirmed moves a
/// count; a fork moves the fork counter; `/info` moves the status, the peers or
/// the hashrate; a lite node's floor moves the gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    wallet_height: u64,
    transactions: usize,
    locked_transactions: usize,
    sub_wallets: usize,
    forks: u64,
    status: SyncStatus,
    lite_start_height: u64,
    peer_count: u64,
    hashrate: u64,
    sync_gap: Option<(u64, u64)>,
}

impl Fingerprint {
    /// `open`'s fingerprint now.
    pub fn of(open: &OpenWallet) -> Fingerprint {
        let wallet = open.wallet();
        Fingerprint {
            wallet_height: wallet.wallet_height(),
            transactions: wallet.sub_wallets.transactions.len(),
            locked_transactions: wallet.sub_wallets.locked_transactions.len(),
            sub_wallets: wallet.sub_wallets.sub_wallet.len(),
            forks: open.sync.forks_resolved(),
            status: open.sync.sync_status(),
            lite_start_height: open.sync.daemon_state().lite_start_height,
            peer_count: open.peer_count,
            hashrate: open.hashrate,
            sync_gap: open.sync_gap,
        }
    }
}
