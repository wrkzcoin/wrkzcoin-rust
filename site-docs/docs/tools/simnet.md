# Simnet

A simnet is a private WrkzCoin network: nodes that mine a block in a
millisecond, for trying a wallet, a pool, an exchange integration or a change
to the node without touching mainnet. Its coins are worthless.

It is mainnet with three differences:

- **its own network id**, so a simnet node and a mainnet node never finish a
  handshake;
- **no proof of work**: a block template is a valid block as it is;
- **difficulty 1 for every block**, so blocks can come as fast as you like and
  the longest chain wins a reorganisation.

Everything else is mainnet's: the genesis block, the transaction format, the
P2P protocol, the RPC, and the consensus rules of each height. A simnet chain
is a young chain, though. Its blocks are at heights 1, 2, 3 … and follow the
rules of *those* heights, not today's mainnet rules at 4,200,000. Transaction
proof of work, the current fee and mixin tiers and the 4,300,000 fork do not
apply at simnet heights.

There are three ways to run one.

## One process: `wrkz-simnet run`

```sh
wrkz-simnet run                      # three nodes in a line, a block every 10 s
wrkz-simnet run --nodes 5 --topology mesh --block-interval 2
```

It prints each node's P2P port, RPC URL and [WebSocket](../node/websocket.md)
URL (node 0 on RPC port 27856, node 1 on 27857, and so on), mines 60 blocks
straight away so the first rewards unlock, then one block every
`--block-interval` seconds on node 0. Unless `--mine-to` names an address, it
mines to fresh keys and prints them:

```text
Mining to fresh keys:
address:           Wrkz…
private spend key: …
private view key:  …
import into a wallet with these two keys and scan height 0 (simnet coins only)
```

Import those keys into `wrkz-wallet` or Rust Pluton Wallet **with scan height
0**, point the wallet at `http://127.0.0.1:27856`, and it has coins to send.
`--enable-cors ORIGIN` lets a browser wallet at `ORIGIN` use the nodes.

## Separate processes: `wrkz-node --simnet`

`--simnet` turns any `wrkz-node` into a simnet node:

```sh
wrkz-node --data-dir ./sim1 --simnet --enable-websocket
wrkz-node --data-dir ./sim2 --simnet --p2p-bind-port 27955 --rpc-bind-port 27956 \
    --add-exclusive-node 127.0.0.1:27855 --allow-local-ip
wrkz-simnet mine --daemon http://127.0.0.1:27856 --interval 5
```

With `--simnet` the node uses its own network id, checks no proof of work, has
no checkpoints, no seeds and no UPnP, and listens on ports 27855 (P2P), 27856
(RPC) and 27857 (ZMQ) unless others are given. On its own, a simnet node counts
as synchronized, so its RPC takes transactions without a peer.

The choice is **permanent for the database**, in both directions: a simnet
data directory refuses to open without `--simnet`, and a mainnet one refuses
to open with it. `--load-checkpoints` and `--import-lite-snapshot` are refused
with `--simnet`, since both are mainnet's.

`wrkz-simnet mine` asks for a template and submits it unchanged. A mainnet node
refuses such a block, so pointing it at the wrong node fails at once. It
retries a node that is not answering yet.

## Docker: `compose.simnet.yml`

[`compose.simnet.yml`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/compose.simnet.yml)
runs three simnet nodes in a line and a miner, from the node image:

```sh
docker build -t wrkz-rust .
docker compose -f compose.simnet.yml up -d
docker compose -f compose.simnet.yml logs miner     # the keys it mines to
docker compose -f compose.simnet.yml down -v        # stop and wipe
```

The nodes' RPCs, each with `/ws`, are on `127.0.0.1:27856`, `:27866` and
`:27876`. Stopping `node2` cuts `node3` off; starting it again lets `node3`
catch up.

## In a test: the `wrkz-simnet` crate

The `wrkz-simnet` crate starts whole nodes inside a test's own process. Each
has the chain, the pool, the P2P engine, the RPC and its WebSocket stream,
assembled as `wrkz-node` assembles them. They are joined by links a test can
cut and heal:

```rust
use std::time::Duration;
use wrkz_simnet::{SimKeys, Simnet};

let net = Simnet::builder().nodes(2).line().build()?;
net.wait_for_connections(1, Duration::from_secs(30));
let (a, b) = (SimKeys::random(), SimKeys::random());

net.cut(0, 1);                                   // a partition
net.node(0).mine_many(&a.address, 3)?;
let longer = net.node(1).mine_many(&b.address, 6)?;
net.heal(0, 1);                                  // node 0 reorganises onto node 1's chain
let (height, top) = net.wait_for_agreement(&[0, 1], Duration::from_secs(30))?;
assert_eq!(top, *longer.last().unwrap());
```

`crates/wrkz-simnet/tests/simnet.rs` covers:

- relay along a line of nodes;
- catching up after a partition;
- a reorganisation to the longer side, with the `chainswitch` it publishes;
- the WebSocket stream announcing a block mined elsewhere;
- a wallet finding its mining rewards and being woken by the stream;
- a mainnet node being turned away.

A link is a TCP relay on loopback. Nodes dial only their links, as exclusive
nodes, so an address learned from a peer list can never go round a cut link.
Simnet nodes compare heights with their peers every two seconds, where mainnet
does it every minute, so a test does not wait long for a node that missed a
relay.
