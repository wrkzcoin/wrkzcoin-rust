// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! WrkzCoin node: the P2P connection manager and the block sync state machine
//! (spec/08-p2p-protocol.md; spec/12-roadmap.md, stage 3 step 3).
//!
//! The C++ splits this work between `NodeServer` (`src/p2p/NetNode.cpp`:
//! connections, the white and gray peer lists, the back ping, the timed sync,
//! the peer state file) and `CryptoNoteProtocolHandler`
//! (`src/cryptonoteprotocol/CryptoNoteProtocolHandler.cpp`: the sync state
//! machine, block and transaction relay). This crate keeps the same split:
//!
//! - [`peers`] — the white and gray lists, `p2pstate.wrkz.bin`, bans and the
//!   dial selection (`PeerListManager.cpp`).
//! - [`sync`] — [`sync::PeerCtx`], the per-connection state
//!   `CryptoNoteConnectionContext` carries, and the adaptive batch rules.
//! - [`net`] — one reader and one writer thread per connection over
//!   [`wrkz_p2p::conn::Connection`], with bounded queues, and the events they
//!   send the engine.
//! - [`node`] — [`Node`], the single-threaded engine: every handler of the two
//!   C++ files, with blocks applied through [`wrkz_chain::ChainState`].
//! - [`pool`] — the [`pool::TxPool`] hook the engine talks to, with a bounded
//!   relay-only set ([`pool::BoundedTxSet`]) for a node that only forwards.
//! - [`mempool`] — [`mempool::SharedMempool`], the real `wrkz-mempool` pool
//!   behind that hook, shared with the RPC server so one pool serves relay,
//!   `/sendrawtransaction` and `getblocktemplate`.
//! - [`console`] — the interactive command handler of the C++
//!   `DaemonCommandsHandler`, reading the chain through the same
//!   [`wrkz_rpc::NodeApi`] the RPC server uses.
//! - [`attach`] — `wrkz-node attach <socket>`: that console, for a daemon that
//!   is already running, over its RPC IPC socket.
//! - [`log`] — the logger, sharing one terminal lock with
//!   the console. It lives in `wrkz-rpc` so the wallet programs and the
//!   service log through the same one; this crate re-exports it.
//! - [`zmq`] — the ZMQ publisher of `src/daemon/ZmqPublisher.cpp`: blocks,
//!   reorganisations and pool changes on a PUB socket, speaking ZMTP itself.
//! - [`chain_notifier`] — `--block-notify`, `--reorg-notify` and `--tx-notify`:
//!   the same events, as commands or webhooks, through [`wrkz_rpc::notify`].
//! - [`config_file`] — `--config-file`: the C++ daemon's JSON (or older
//!   `key=value`) configuration, turned into command-line arguments.
//! - [`compaction`] — the boot, periodic and `compact_db` database
//!   compactions of the C++ `DaemonCommandsHandler`, behind one state.
//! - [`upnp`] — the UPnP port mapping of the P2P port (`addPortMapping`),
//!   std-only, on a thread of its own, and removed again on shutdown.
//! - [`snapshot`] — the `snapshot_export` console command around
//!   `wrkz_chain::snapshot`, and the `--snapshot-stats` table.
//!
//! # The daemon
//!
//! `src/bin/node.rs` is the deployable daemon: it opens the chain state, starts
//! the engine and starts `wrkz-rpc` on the same state, behind one
//! [`SharedChain`] and one [`SharedPool`]. See `docs/DAEMON.md`.
//!
//! # What this crate does not do
//!
//! No transaction-by-hash lookup for peers: [`node::Node`] answers
//! `NOTIFY_MISSING_TXS` from the pool only. A lite block only ever references
//! pool transactions, so this is reachable in practice only from a peer asking
//! for a confirmed transaction.
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//! use wrkz_chain::{ChainState, Checkpoints, Config};
//! use wrkz_node::{Node, NodeConfig, pool::BoundedTxSet};
//! use wrkz_storage::MemStore;
//!
//! let chain = ChainState::open_or_genesis(MemStore::default(), Config::default(), Checkpoints::mainnet())?;
//! let mut node = Node::new(chain, BoundedTxSet::new(4096, 1 << 20), NodeConfig::default());
//! node.start()?;
//! node.run_for(Duration::from_secs(30));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod attach;
pub mod chain_notifier;
pub mod compaction;
pub mod config_file;
pub mod console;
pub mod daemon;
pub mod mempool;
pub mod net;
pub mod node;
pub mod peers;
pub mod pool;
pub mod snapshot;
pub mod stratum;
pub mod sync;
pub mod upnp;
pub mod zmq;

pub use console::Console;
pub use mempool::{SharedMempool, SharedPool};
pub use node::{Node, NodeConfig, SharedChain};
pub use peers::PeerManager;
pub use pool::{BoundedTxSet, TxPool};
pub use sync::{PeerState, SyncTuning};
pub use wrkz_rpc::log::{self, Level};
pub use wrkz_rpc::{log_debug, log_error, log_info, log_trace, log_warn};
