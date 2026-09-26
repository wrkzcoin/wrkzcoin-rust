// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The simnet: a private WrkzCoin network for tests and for trying things out.
//!
//! It is mainnet, with three differences, all of them in
//! `wrkz_chain::Config::simnet` and `wrkz_node::NodeConfig::network_id`:
//!
//! - its own network id, so a simnet node and a mainnet node never finish a
//!   handshake;
//! - blocks need no proof of work, so a test mines a block in a millisecond;
//! - every block has difficulty 1, so blocks can come as fast as a test likes
//!   without the difficulty running away.
//!
//! Everything else — the genesis block, the rules of each height, the
//! transaction format, the P2P protocol, the RPC — is mainnet's, unchanged. A
//! simnet chain is a young chain: its blocks are at heights 1, 2, 3 … and
//! follow the rules of those heights, not today's mainnet rules at 4,200,000.
//!
//! Two ways to use it:
//!
//! - **In a test**, whole nodes in the test's own process: [`Simnet::builder`]
//!   starts them, joined by [`Link`]s that can be cut and healed, and
//!   [`SimNode::mine`] makes blocks. See `tests/simnet.rs`.
//! - **From the command line**, `wrkz-simnet run` keeps a cluster going for a
//!   wallet to be pointed at, and `wrkz-simnet mine` mines against a
//!   `wrkz-node --simnet` in another process. See `wrkz-simnet --help`.
//!
//! - [`keys`] — throwaway keys and addresses to mine to.
//! - [`link`] — the relay that joins two nodes and can be cut.
//! - [`net`] — the nodes and the network.

#![forbid(unsafe_code)]

pub mod keys;
pub mod link;
pub mod net;

pub use keys::SimKeys;
pub use link::Link;
pub use net::{NodeOptions, SimNode, Simnet, SimnetBuilder};
