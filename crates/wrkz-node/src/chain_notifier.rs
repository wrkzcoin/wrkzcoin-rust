// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `--block-notify`, `--reorg-notify`, `--tx-notify` and `--notify-during-sync`
//! (`src/daemon/ChainNotifier.cpp`): run a command, or POST to a URL, when a
//! block joins the main chain, when the chain reorganises, and when a
//! transaction enters the pool.
//!
//! Each hook is a [`wrkz_rpc::notify::Notifier`], the runner the wallet programs
//! use too, so the spec is read the same way everywhere: a command template
//! split into arguments with `%`-placeholders substituted inside them, no
//! shell, or an `http://` URL. This module only decides what to announce, from
//! the [`wrkz_rpc::events`] stream the ZMQ publisher also listens to.
//!
//! | Hook | Placeholders | Webhook body |
//! | --- | --- | --- |
//! | `--block-notify` | `%s` hash, `%h` index | `{"event":"block","height":N,"hash":"…"}` |
//! | `--reorg-notify` | `%s` split height, `%h` new top index, `%n` new blocks, `%d` discarded | `{"event":"reorg","split_height":N,"new_height":N,"new_blocks":N,"discarded_blocks":N}` |
//! | `--tx-notify` | `%s` hash | `{"event":"tx","hash":"…"}` |
//!
//! A reorganisation queues `--reorg-notify`, then `--block-notify` for every
//! block of the new branch in order, as the C++ and monerod do. Each hook has a
//! worker of its own, so one hook's deliveries can interleave with another's;
//! a hook's own are always in order. Alternative blocks and transactions
//! leaving the pool are not announced.
//!
//! Until the node is synchronized nothing is announced, unless
//! `--notify-during-sync`: an event is dropped, not held back
//! (`ChainNotifier::shouldNotifyAt`, `ChainNotifier.cpp:117-128`).
//!
//! An `https://` webhook needs a TLS client, which the daemon does not carry, so
//! such a hook is disabled with a warning — what a C++ build without OpenSSL
//! does (`Notifier.cpp:188-195`).

use std::sync::Mutex;

use wrkz_primitives::Hash;
use wrkz_rpc::events::{ChainEvent, EventListener};
use wrkz_rpc::notify::{Field, Notification, Notifier, Options};

use crate::log_info;

/// Whether the node counts as at the tip for a block at this index: the
/// C++'s `isSynchronized() || blockIndex + 1 >= getObservedHeight()`, which
/// keeps a node with no peers from being muted forever.
pub type AtTip = Box<dyn Fn(u32) -> bool + Send + Sync>;

/// The three hooks and what they need to decide.
pub struct ChainNotifier {
    block: Notifier,
    reorg: Notifier,
    tx: Notifier,
    during_sync: bool,
    at_tip: AtTip,
    /// `m_topIndex`: the main chain's top as the events have left it, which a
    /// reorganisation's discarded count is measured from.
    top_index: Mutex<u32>,
}

/// The operator's settings for the three hooks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookSpecs {
    pub block: String,
    pub reorg: String,
    pub tx: String,
    pub notify_during_sync: bool,
}

impl ChainNotifier {
    /// Build the hooks. `top_index` is the chain's top now
    /// (`m_core.getTopBlockIndex()`, `ChainNotifier.cpp:67`).
    pub fn new(specs: &HookSpecs, top_index: u32, at_tip: AtTip) -> Self {
        Self::with_options(specs, top_index, at_tip, Options::default())
    }

    /// [`ChainNotifier::new`] with the runner's options, which a test uses to
    /// catch webhook deliveries.
    pub fn with_options(specs: &HookSpecs, top_index: u32, at_tip: AtTip, options: Options) -> Self {
        Self {
            block: Notifier::with_options("block-notify", &specs.block, options.clone()),
            reorg: Notifier::with_options("reorg-notify", &specs.reorg, options.clone()),
            tx: Notifier::with_options("tx-notify", &specs.tx, options),
            during_sync: specs.notify_during_sync,
            at_tip,
            top_index: Mutex::new(top_index),
        }
    }

    /// Whether any hook survived its spec; a daemon with none runs without
    /// the notifier (`Daemon.cpp:1146`).
    pub fn any_enabled(&self) -> bool {
        self.block.enabled() || self.reorg.enabled() || self.tx.enabled()
    }

    /// The start-up line (`ChainNotifier.cpp:75-76`).
    pub fn log_started(&self) {
        let mode = if self.during_sync { " (notifying during sync)" } else { " (suppressed until synchronized)" };
        log_info!("Chain notifier started{mode}");
    }

    /// Stop every hook; what is queued is discarded (`ChainNotifier::stop`).
    pub fn stop(&self) {
        self.block.stop();
        self.reorg.stop();
        self.tx.stop();
    }

    fn should_notify_at(&self, index: u32) -> bool {
        self.during_sync || (self.at_tip)(index)
    }

    fn top_index(&self) -> std::sync::MutexGuard<'_, u32> {
        self.top_index.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn notify_block(&self, index: u32, hash: &Hash) {
        if !self.block.enabled() {
            return;
        }
        let (hash, height) = (hex::encode(hash), index.to_string());
        self.block.notify(Notification {
            event: "block".to_string(),
            placeholders: vec![('s', hash.clone()), ('h', height.clone())],
            fields: vec![Field::raw("height", height), Field::string("hash", hash)],
        });
    }
}

impl EventListener for ChainNotifier {
    fn on_event(&self, event: &ChainEvent) {
        match event {
            ChainEvent::BlockAdded { index, hash, .. } => {
                *self.top_index() = *index;
                if self.should_notify_at(*index) {
                    self.notify_block(*index, hash);
                }
            }
            ChainEvent::ChainSwitched { common_root_index, hashes } => {
                let root = *common_root_index;
                let new_blocks = hashes.len().saturating_sub(1) as u32;
                let new_top = root + new_blocks;
                let discarded = {
                    let mut top = self.top_index();
                    let discarded = top.saturating_sub(root);
                    *top = new_top;
                    discarded
                };
                if !self.should_notify_at(new_top) {
                    return;
                }
                if self.reorg.enabled() {
                    let (split, height) = ((root + 1).to_string(), new_top.to_string());
                    let (new, gone) = (new_blocks.to_string(), discarded.to_string());
                    let placeholders =
                        vec![('s', split.clone()), ('h', height.clone()), ('n', new.clone()), ('d', gone.clone())];
                    self.reorg.notify(Notification {
                        event: "reorg".to_string(),
                        placeholders,
                        fields: vec![
                            Field::raw("split_height", split),
                            Field::raw("new_height", height),
                            Field::raw("new_blocks", new),
                            Field::raw("discarded_blocks", gone),
                        ],
                    });
                }
                for (offset, hash) in hashes.iter().enumerate().skip(1) {
                    self.notify_block(root + offset as u32, hash);
                }
            }
            ChainEvent::PoolAdded { hash } => {
                let top = *self.top_index();
                if self.tx.enabled() && self.should_notify_at(top) {
                    let hash = hex::encode(hash);
                    self.tx.notify(Notification {
                        event: "tx".to_string(),
                        placeholders: vec![('s', hash.clone())],
                        fields: vec![Field::string("hash", hash)],
                    });
                }
            }
            ChainEvent::AlternativeBlockAdded { .. } | ChainEvent::PoolRemoved { .. } => {}
        }
    }
}

impl Drop for ChainNotifier {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use wrkz_rpc::events::PoolRemoval;

    /// The URL and body of every webhook delivery, in order.
    type Posts = Arc<Mutex<Vec<(String, String)>>>;

    /// Every hook a webhook whose deliveries land in the returned [`Posts`].
    fn notifier(during_sync: bool, at_tip: AtTip) -> (ChainNotifier, Posts) {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let caught = Arc::clone(&posts);
        let options = Options {
            post: Some(Arc::new(move |url: &str, body: &str, _| {
                caught.lock().unwrap().push((url.to_string(), body.to_string()));
                Ok(200)
            })),
            ..Options::default()
        };
        let specs = HookSpecs {
            block: "http://127.0.0.1:1/block".into(),
            reorg: "http://127.0.0.1:1/reorg".into(),
            tx: "http://127.0.0.1:1/tx".into(),
            notify_during_sync: during_sync,
        };
        (ChainNotifier::with_options(&specs, 12, at_tip, options), posts)
    }

    /// The bodies delivered once `count` have arrived.
    fn bodies(posts: &Mutex<Vec<(String, String)>>, count: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let got = posts.lock().unwrap().clone();
            if got.len() >= count {
                return got.into_iter().map(|(_, body)| body).collect();
            }
            assert!(Instant::now() < deadline, "only {} of {count} deliveries arrived: {got:?}", got.len());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_block_is_announced_once_at_the_tip_and_not_while_syncing() {
        let synced = Arc::new(AtomicBool::new(false));
        let gate = Arc::clone(&synced);
        let (hooks, posts) = notifier(false, Box::new(move |_| gate.load(Ordering::SeqCst)));
        assert!(hooks.any_enabled());
        hooks.on_event(&ChainEvent::BlockAdded { index: 13, hash: [1; 32], transaction_hashes: Vec::new() });
        synced.store(true, Ordering::SeqCst);
        hooks.on_event(&ChainEvent::BlockAdded { index: 14, hash: [2; 32], transaction_hashes: Vec::new() });
        let got = bodies(&posts, 1);
        assert_eq!(got, [format!("{{\"event\":\"block\",\"height\":14,\"hash\":\"{}\"}}", "02".repeat(32))]);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(posts.lock().unwrap().len(), 1, "the block during sync was dropped, not held back");
    }

    /// Each hook has a worker of its own, so the reorg and the block
    /// deliveries can reach their endpoints in either order; each hook's own
    /// deliveries are in order.
    #[test]
    fn a_reorganisation_announces_itself_and_every_new_block() {
        let (hooks, posts) = notifier(true, Box::new(|_| false));
        // The top is 12; the new branch forks after 9 and runs to 13.
        let hashes: Vec<Hash> = (9..=13).map(|i| [i as u8; 32]).collect();
        hooks.on_event(&ChainEvent::ChainSwitched { common_root_index: 9, hashes });
        let _ = bodies(&posts, 5);
        let to = |path: &str| -> Vec<String> {
            posts.lock().unwrap().iter().filter(|(url, _)| url.ends_with(path)).map(|(_, body)| body.clone()).collect()
        };
        assert_eq!(
            to("/reorg"),
            ["{\"event\":\"reorg\",\"split_height\":10,\"new_height\":13,\"new_blocks\":4,\"discarded_blocks\":3}"]
        );
        let blocks = to("/block");
        assert_eq!(blocks.len(), 4);
        for (body, index) in blocks.iter().zip(10u32..) {
            let hash = hex::encode([index as u8; 32]);
            assert_eq!(*body, format!("{{\"event\":\"block\",\"height\":{index},\"hash\":\"{hash}\"}}"));
        }
    }

    #[test]
    fn a_pool_transaction_is_announced_and_nothing_else_is() {
        let (hooks, posts) = notifier(true, Box::new(|_| false));
        hooks.on_event(&ChainEvent::AlternativeBlockAdded { index: 20, hash: [3; 32] });
        hooks.on_event(&ChainEvent::PoolRemoved { hashes: vec![[4; 32]], reason: PoolRemoval::InBlock });
        hooks.on_event(&ChainEvent::PoolAdded { hash: [5; 32] });
        assert_eq!(bodies(&posts, 1), [format!("{{\"event\":\"tx\",\"hash\":\"{}\"}}", "05".repeat(32))]);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(posts.lock().unwrap().len(), 1);
    }

    #[test]
    fn no_spec_is_no_hook() {
        let hooks = ChainNotifier::new(&HookSpecs::default(), 0, Box::new(|_| true));
        assert!(!hooks.any_enabled());
        hooks.on_event(&ChainEvent::PoolAdded { hash: [5; 32] });
    }
}
