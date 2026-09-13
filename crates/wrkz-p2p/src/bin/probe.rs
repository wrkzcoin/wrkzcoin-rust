// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Live network probe: handshake with a seed node, request the sparse chain,
//! download the first blocks over P2P and validate them with the primitives.
//!
//!     wrkz-p2p-probe [host:port] [blocks-to-fetch]
//!
//! Exit code 0 means every step matched the C++ node (spec/08 acceptance 1-2,
//! stage-1 scale).

use std::io::{self, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};
use wrkz_p2p::levin::{self, Header};
use wrkz_p2p::msg::*;
use wrkz_primitives::block::{genesis_block_hash, BlockTemplate};
use wrkz_primitives::constants::{block_major_version_for_index, P2P_MINIMUM_VERSION};
use wrkz_primitives::tx::Transaction;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let target = args.get(1).cloned().unwrap_or_else(|| "node-fin.wrkz.work:17855".to_string());
    let count: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    match run(&target, count) {
        Ok(()) => println!("PROBE OK"),
        Err(e) => {
            eprintln!("PROBE FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn run(target: &str, count: usize) -> Result<(), Box<dyn std::error::Error>> {
    let addr = target.to_socket_addrs()?.next().ok_or("no address")?;
    println!("connecting to {target} ({addr})");
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(5000))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    s.set_write_timeout(Some(Duration::from_secs(30)))?;

    // ---- handshake ----
    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd).map_err(|e| format!("csprng: {e}"))?;
    let our_peer_id = u64::from_le_bytes(rnd);
    let genesis = genesis_block_hash();
    let our_sync = CoreSyncData { current_height: 1, top_id: genesis, ..Default::default() };
    let node = BasicNodeData::ours(our_peer_id, 0); // my_port 0: no back ping, we are a client
    let payload = handshake_request(&node, &our_sync);
    levin::write_frame(&mut s, &Header::request(COMMAND_HANDSHAKE, true), &payload)?;
    let (h, body) = wait_for(&mut s, COMMAND_HANDSHAKE, true, our_peer_id, &our_sync)?;
    if h.return_code != levin::RETCODE_SUCCESS {
        return Err(format!("handshake return code {}", h.return_code).into());
    }
    let hs = parse_handshake_response(&body)?;
    if hs.node_data.network_id != wrkz_primitives::constants::CRYPTONOTE_NETWORK {
        return Err("network id mismatch".into());
    }
    if hs.node_data.version < P2P_MINIMUM_VERSION {
        return Err(format!("peer version {} below minimum", hs.node_data.version).into());
    }
    println!(
        "handshake ok: peer version {} peer_id {:016x} height {} top {} peers {} (v4) {} (v6)",
        hs.node_data.version,
        hs.node_data.peer_id,
        hs.payload_data.current_height,
        hex::encode(hs.payload_data.top_id),
        hs.local_peerlist.len(),
        hs.local_peerlist6.len()
    );
    for p in hs.local_peerlist.iter().take(5) {
        println!("  peer {} id {:016x} last_seen {}", p.addr(), p.id, p.last_seen);
    }

    // ---- sparse chain: we hold only genesis ----
    let payload = request_chain(&[genesis]);
    levin::write_frame(&mut s, &Header::request(NOTIFY_REQUEST_CHAIN, false), &payload)?;
    let (_, body) = wait_for(&mut s, NOTIFY_RESPONSE_CHAIN_ENTRY, false, our_peer_id, &our_sync)?;
    let entry = parse_chain_entry(&body)?;
    println!("chain entry: start {} total {} ids {}", entry.start_height, entry.total_height, entry.block_ids.len());
    if entry.start_height != 0 || entry.block_ids.first() != Some(&genesis) {
        return Err("chain entry does not start at our genesis".into());
    }
    let known = [
        "93bb1fd850d9e904ca810cdb57935b6df45cd75fc3a86358a421e126c1ae7b51",
        "4fc480b6507b6df08a92496f3af83dd16b5b44ea1ba76792bd4e6381696c29c3",
        "e2c36c96876cec05e1e9b0f488eef4a0e1487ba38a2f52a3054123bab9bff5de",
        "bc9ecbdcde0fc6ca467025af49ba239e49148702af9503bce8627714f6974a31",
        "513e3cbe87ff9ca63ee30197ac358de63ac30216b68359231917c1e10169e1cb",
    ];
    if entry.block_ids.len() <= known.len() {
        return Err(format!("chain entry has {} ids, need at least {}", entry.block_ids.len(), known.len() + 1).into());
    }
    for (i, k) in known.iter().enumerate() {
        if hex::encode(entry.block_ids[i + 1]) != *k {
            return Err(format!("block id {} differs from the spec header table", i + 1).into());
        }
    }
    println!("block ids 1..5 match spec/09 headers");

    // ---- fetch the first `count` blocks after genesis ----
    let want: Vec<[u8; 32]> = entry.block_ids[1..1 + count.min(entry.block_ids.len() - 1)].to_vec();
    let payload = request_get_objects(&want);
    levin::write_frame(&mut s, &Header::request(NOTIFY_REQUEST_GET_OBJECTS, false), &payload)?;
    let (_, body) = wait_for(&mut s, NOTIFY_RESPONSE_GET_OBJECTS, false, our_peer_id, &our_sync)?;
    let objs = parse_get_objects_response(&body)?;
    println!(
        "get objects: {} blocks, {} missed, peer height {}",
        objs.blocks.len(),
        objs.missed_ids.len(),
        objs.current_blockchain_height
    );
    if objs.blocks.len() != want.len() {
        return Err("block count mismatch".into());
    }
    let mut prev = genesis;
    for (i, rb) in objs.blocks.iter().enumerate() {
        let index = (i + 1) as u64;
        let b = BlockTemplate::from_bytes(&rb.block)?;
        let id = b.hash()?;
        if id != want[i] {
            return Err(format!("block {index}: id mismatch").into());
        }
        if b.previous_block_hash != prev {
            return Err(format!("block {index}: prev_id mismatch").into());
        }
        if b.major_version != block_major_version_for_index(index) {
            return Err(format!("block {index}: wrong major version").into());
        }
        if b.coinbase_height() != Some(index) {
            return Err(format!("block {index}: coinbase height").into());
        }
        if rb.txs.len() != b.transaction_hashes.len() {
            return Err(format!("block {index}: tx count").into());
        }
        for (t, want_h) in rb.txs.iter().zip(&b.transaction_hashes) {
            if Transaction::from_bytes(t)?.hash()? != *want_h {
                return Err(format!("block {index}: tx hash").into());
            }
        }
        // PoW must satisfy the difficulty from the spec header table (1,1,60,3660,24806)
        let diffs = [1u64, 1, 60, 3660, 24806];
        if let Some(d) = diffs.get(i) {
            if !b.check_proof_of_work(*d)? {
                return Err(format!("block {index}: proof of work").into());
            }
        }
        println!("  block {index}: v{} id {} txs {} pow ok", b.major_version, hex::encode(id), rb.txs.len());
        prev = id;
    }
    Ok(())
}

/// How long, and for how many frames, [`wait_for`] will keep reading before it
/// gives up. Without these a peer that streams notifications forever keeps the
/// probe alive indefinitely: the socket read timeout never fires because every
/// individual read succeeds.
const WAIT_BUDGET: Duration = Duration::from_secs(120);
const WAIT_MAX_FRAMES: usize = 1000;

/// Read frames until `command` arrives (a response when `response` is set),
/// answering the peer's own timed-sync and ping requests on the way and
/// ignoring other notifications.
fn wait_for(
    s: &mut TcpStream,
    command: u32,
    response: bool,
    our_peer_id: u64,
    our_sync: &CoreSyncData,
) -> io::Result<(Header, Vec<u8>)> {
    let deadline = Instant::now() + WAIT_BUDGET;
    for _ in 0..WAIT_MAX_FRAMES {
        let (h, body) = levin::read_frame(s)?;
        if h.command == command && h.is_response() == response {
            return Ok((h, body));
        }
        if !h.is_expected_version() {
            println!("  (peer sent protocol version {}, expected 1)", h.protocol_version);
        }
        match h.command {
            COMMAND_TIMED_SYNC if h.have_to_return_data => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let p = timed_sync_response(now, our_sync, &[], &[]);
                levin::write_frame(s, &Header::response(COMMAND_TIMED_SYNC, levin::RETCODE_SUCCESS), &p)?;
            }
            COMMAND_PING if h.have_to_return_data => {
                let p = ping_response(our_peer_id);
                levin::write_frame(s, &Header::response(COMMAND_PING, levin::RETCODE_SUCCESS), &p)?;
            }
            other if h.have_to_return_data => {
                levin::write_frame(s, &Header::response(other, levin::ERROR_HANDLER_NOT_DEFINED), &[])?;
            }
            other => {
                println!("  (ignoring notification {other}, {} bytes)", body.len());
            }
        }
        io::stdout().flush().ok();
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no command {command} within {WAIT_BUDGET:?}"),
            ));
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, format!("no command {command} in {WAIT_MAX_FRAMES} frames")))
}
