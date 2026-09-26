// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `wrkz-simnet`: a private test network from the command line.
//!
//! - `run` — a cluster of simnet nodes in this process, each with its RPC (and
//!   its WebSocket stream) on loopback, mining a block every few seconds.
//!   Point `wrkz-wallet`, `wrkz-wallet-api` or Rust Pluton Wallet at one.
//! - `mine` — mine against a `wrkz-node --simnet` in another process (or on
//!   another host), through its `getblocktemplate` and `submitblock`.
//! - `keys` — print a fresh address and its keys, to mine to and then import.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use wrkz_simnet::{NodeOptions, SimKeys, Simnet};

const USAGE: &str = "\
wrkz-simnet run [options]    a simnet cluster in this process
  --nodes N                  how many nodes (default 3)
  --topology line|ring|mesh  how they are joined (default line)
  --rpc-port PORT            node 0's RPC port; node i gets PORT+i
                             (default 27856; 0 picks free ports)
  --no-websocket             do not serve GET /ws on the RPCs
  --enable-cors ORIGIN       let a browser wallet at ORIGIN use the RPCs
  --mine-to ADDRESS          pay mined blocks here (default: fresh keys,
                             printed at start so a wallet can import them)
  --premine N                mine N blocks before the interval starts
                             (default 60: 40 of them unlock the first reward)
  --block-interval SECS      then one block every SECS seconds (default 10;
                             0 mines none)

wrkz-simnet mine [options]   mine against a wrkz-node --simnet
  --daemon URL               its RPC (default http://127.0.0.1:27856)
  --address ADDRESS          pay blocks here (default: fresh keys, printed)
  --blocks N                 mine N blocks, then exit (default: until Ctrl-C)
  --interval SECS            between blocks (default 10; 0 is back to back)

wrkz-simnet keys             print a fresh address, its keys and how to
                             import them

A simnet is a private network: its own network id (it never peers with
mainnet), no proof of work, difficulty 1, and mainnet's rules otherwise. Its
coins are worthless. A simnet wallet must scan from height 0: import the keys
with scan height 0.
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("run") => run(args.collect()),
        Some("mine") => mine(args.collect()),
        Some("keys") => {
            print_keys(&SimKeys::random());
            Ok(())
        }
        Some("-h") | Some("--help") | None => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => Err(format!("unknown command {other}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wrkz-simnet: {e}");
            eprintln!();
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `--name VALUE` pairs and bare flags, in order.
struct Options(std::vec::IntoIter<String>);

impl Options {
    fn next(&mut self) -> Option<String> {
        self.0.next()
    }

    fn value(&mut self, name: &str) -> Result<String, String> {
        self.0.next().ok_or_else(|| format!("{name} needs a value"))
    }

    fn number(&mut self, name: &str) -> Result<u64, String> {
        let v = self.value(name)?;
        v.parse().map_err(|_| format!("{name}: {v} is not a number"))
    }
}

fn print_keys(keys: &SimKeys) {
    println!("address:           {}", keys.address);
    println!("private spend key: {}", keys.spend_secret_hex());
    println!("private view key:  {}", keys.view_secret_hex());
    println!("import into a wallet with these two keys and scan height 0 (simnet coins only)");
}

fn run(argv: Vec<String>) -> Result<(), String> {
    let mut o = Options(argv.into_iter());
    let (mut nodes, mut topology, mut rpc_port) = (3usize, "line".to_string(), 27856u16);
    let (mut websocket, mut cors, mut mine_to) = (true, String::new(), None);
    let (mut premine, mut interval) = (60u64, 10u64);
    while let Some(arg) = o.next() {
        match arg.as_str() {
            "--nodes" => nodes = o.number("--nodes")? as usize,
            "--topology" => topology = o.value("--topology")?,
            "--rpc-port" => rpc_port = o.number("--rpc-port")? as u16,
            "--no-websocket" => websocket = false,
            "--enable-cors" => cors = o.value("--enable-cors")?,
            "--mine-to" => mine_to = Some(o.value("--mine-to")?),
            "--premine" => premine = o.number("--premine")?,
            "--block-interval" => interval = o.number("--block-interval")?,
            other => return Err(format!("unknown option {other}")),
        }
    }
    if nodes == 0 {
        return Err("--nodes must be at least 1".into());
    }
    let mut builder = Simnet::builder();
    for i in 0..nodes {
        let port = if rpc_port == 0 { 0 } else { rpc_port.checked_add(i as u16).ok_or("--rpc-port is too high")? };
        builder = builder.node(NodeOptions { rpc_port: port, websocket, cors: cors.clone(), ..NodeOptions::default() });
    }
    builder = match topology.as_str() {
        "line" => builder.line(),
        "ring" => builder.ring(),
        "mesh" => builder.mesh(),
        other => return Err(format!("--topology {other}: line, ring or mesh")),
    };
    let net = builder.build().map_err(|e| format!("starting the simnet: {e}"))?;

    let address = match mine_to {
        Some(address) => address,
        None => {
            let keys = SimKeys::random();
            println!("Mining to fresh keys:");
            print_keys(&keys);
            println!();
            keys.address
        }
    };
    for node in net.nodes() {
        println!(
            "node {}: p2p {}  rpc {}  events {}",
            node.index(),
            node.p2p_addr(),
            node.rpc_url().unwrap_or_else(|| "-".into()),
            node.ws_url().unwrap_or_else(|| "-".into())
        );
    }
    if nodes > 1 && !net.wait_for_connections(1, Duration::from_secs(10)) {
        eprintln!("warning: not every node connected within 10 seconds");
    }

    wrkz_rpc::signal::install();
    if premine > 0 {
        net.node(0).mine_many(&address, premine as usize)?;
        println!("premined {premine} blocks on node 0");
    }
    println!("running; Ctrl-C to stop");
    let mut next_block = Instant::now() + Duration::from_secs(interval);
    let mut next_status = Instant::now();
    while !wrkz_rpc::signal::stop_requested() {
        if interval > 0 && Instant::now() >= next_block {
            if let Err(e) = net.node(0).mine(&address) {
                eprintln!("mining: {e}");
            }
            next_block = Instant::now() + Duration::from_secs(interval);
        }
        if Instant::now() >= next_status {
            let line: Vec<String> = net
                .nodes()
                .iter()
                .map(|n| format!("node {} height {} peers {}", n.index(), n.height(), n.connections()))
                .collect();
            println!("{}", line.join(" | "));
            next_status = Instant::now() + Duration::from_secs(30);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("stopping");
    Ok(())
}

fn mine(argv: Vec<String>) -> Result<(), String> {
    let mut o = Options(argv.into_iter());
    let (mut url, mut address, mut blocks, mut interval) =
        ("http://127.0.0.1:27856".to_string(), None, None::<u64>, 10u64);
    while let Some(arg) = o.next() {
        match arg.as_str() {
            "--daemon" => url = o.value("--daemon")?,
            "--address" => address = Some(o.value("--address")?),
            "--blocks" => blocks = Some(o.number("--blocks")?),
            "--interval" => interval = o.number("--interval")?,
            other => return Err(format!("unknown option {other}")),
        }
    }
    let address = match address {
        Some(address) => address,
        None => {
            let keys = SimKeys::random();
            println!("Mining to fresh keys:");
            print_keys(&keys);
            println!();
            keys.address
        }
    };
    let daemon = wrkz_wallet::daemon::Daemon::new(&url).map_err(|e| format!("--daemon {url}: {e}"))?;
    wrkz_rpc::signal::install();
    let mut mined = 0u64;
    while !wrkz_rpc::signal::stop_requested() && blocks.is_none_or(|b| mined < b) {
        // A daemon that is not up yet — the miner of a compose file starts
        // with its node — is asked again, and so is one that went away.
        let template = match daemon.block_template(&address, 0) {
            Ok(template) => template,
            Err(e) => {
                eprintln!("getblocktemplate: {e}; retrying in 5 s");
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && !wrkz_rpc::signal::stop_requested() {
                    std::thread::sleep(Duration::from_millis(100));
                }
                continue;
            }
        };
        // A mainnet daemon's template needs real work: submitting it as it
        // comes is refused, which is the check that this is a simnet.
        match daemon.submit_block(&template.blocktemplate_blob) {
            Ok(()) => {
                mined += 1;
                println!("mined block {} (difficulty {})", template.height, template.difficulty);
            }
            Err(e) => {
                return Err(format!(
                    "submitblock: {e}. Is {url} a wrkz-node --simnet? A mainnet node wants proof of work."
                ))
            }
        }
        let deadline = Instant::now() + Duration::from_secs(interval);
        while Instant::now() < deadline && !wrkz_rpc::signal::stop_requested() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
}
