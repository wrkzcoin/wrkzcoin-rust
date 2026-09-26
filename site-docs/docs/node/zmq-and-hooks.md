# ZMQ and notify hooks

`wrkz-node` tells other programs what happens to the chain and the pool in two
ways, both as `Wrkzd` does: a ZMQ PUB socket, on by default, and the
`--block-notify`, `--reorg-notify` and `--tx-notify` hooks, which run a command
or POST to a URL.

The C++ counterpart is [Notification Hooks](https://docs.wrkz.work/guides/notify-hooks/) on
docs.wrkz.work.

The same events are also available as a WebSocket stream on the RPC port, for
browsers and anything behind a reverse proxy: see
[WebSocket events](websocket.md).

## ZMQ

Like `Wrkzd` (`src/daemon/ZmqPublisher.cpp`), the node publishes what happens to the chain and the pool on a
ZMQ PUB socket, **on by default** at `tcp://127.0.0.1:17857`:

```sh
wrkz-node --data-dir ./wrkz-rust                                # tcp://127.0.0.1:17857
wrkz-node --data-dir ./wrkz-rust --zmq-pub ipc:///run/wrkz/zmq  # a local socket
wrkz-node --data-dir ./wrkz-rust --no-zmq                       # off
scripts/zmq-subscribe.py --topic hashblock                      # watch it (needs pyzmq)
```

[`scripts/zmq-subscribe.py`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/zmq-subscribe.py)
takes `--topic` more than once, and with no `--topic` prints every topic.

Every message is two frames, the topic and a compact JSON body, byte for byte
what the C++ sends. `height` is the block's index.

| Topic | Body | Sent when |
| --- | --- | --- |
| `hashblock` | `{"height":N,"hash":"…"}` | a block joins the main chain |
| `chain_main` | `{"height":N,"hash":"…","transaction_hashes":["…",…]}` | straight after it; the coinbase first |
| `hashblock_alt` | `{"height":N,"hash":"…"}` | a block is kept on an alternative chain |
| `chainswitch` | `{"common_root_height":R,"hashes":["…",…]}` | a reorganisation; the common root first |
| `txpool_add` | `{"hashes":["…"]}` | a transaction enters the pool |
| `txpool_del` | `{"hashes":[…],"reason":"InBlock"}`, or `Outdated`, `NotActual` | transactions leave it |

- The blocks a reorganisation brings in are announced by `chainswitch` alone,
  never by `hashblock`, as in the C++.
- Blocks are published during the initial sync too, as the C++ publishes them.
  A subscriber that only cares about the tip checks `/info` before acting.
- A subscriber gets the topics it subscribed to, matched by prefix as libzmq
  matches them: `hashblock` also brings `hashblock_alt`, and the empty prefix
  brings everything. One that stops reading loses messages once 1,000 are
  waiting for it, as a libzmq PUB socket drops at its high-water mark, and
  never slows the node or the other subscribers.
- A failure to bind is two warnings and the node runs on without ZMQ; an
  address that is not loopback draws a warning, since anyone who can reach it
  can watch the pool. `"zmq-pub": ""` in a configuration file is off.

### Where this differs from the C++, on purpose {#zmq-differences}

- **`txpool_del` with `InBlock` lists what the block mined.** The C++'s list is
  always empty — the vector is reserved and never filled — and it sends one
  such message per block and an `Outdated` one a minute whether or not anything
  left. An empty `txpool_del` is not sent here.
- **No libzmq.** The daemon speaks the protocol itself: ZMTP 3.1 with the NULL
  mechanism, which is all the C++'s socket offers (it is built without CURVE
  and sets no ZAP handler). That keeps a C++ library and its build off every
  platform in [Cross-compiling](../building/cross-compile.md). Any libzmq
  subscriber works —
  [`crates/wrkz-node/tests/zmq.rs`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/crates/wrkz-node/tests/zmq.rs)
  checks the wire against the RFC, and
  `WRKZ_ZMQ_PYTHON=python3 cargo test -p wrkz-node --test zmq -- --ignored`
  checks it against libzmq through pyzmq.
- **IPv6 and IPC.** A bracketed IPv6 address binds; the C++ never sets
  `ZMQ_IPV6`, so libzmq refuses one there. An `ipc://` socket is created
  owner-only, as the RPC's is; libzmq leaves it to the umask.

## Notify hooks

`--block-notify`, `--reorg-notify` and `--tx-notify`, as `Wrkzd` has them
(`src/daemon/ChainNotifier.cpp`): run a
command, or POST to a URL, when a block joins the main chain, when the chain
reorganises, and when a transaction enters the pool. They hear the same events
the [ZMQ](#zmq) publisher publishes, and they run through the same notification
runner the wallet programs use (`wrkz_rpc::notify`).

```sh
wrkz-node --data-dir ./wrkz-rust --block-notify '/usr/local/bin/on-block %s %h'
wrkz-node --data-dir ./wrkz-rust --tx-notify http://127.0.0.1:8080/wrkz
```

| Hook | Placeholders | Webhook body |
| --- | --- | --- |
| `--block-notify` | `%s` hash, `%h` index | `{"event":"block","height":N,"hash":"…"}` |
| `--reorg-notify` | `%s` split height, `%h` new top index, `%n` new blocks, `%d` discarded | `{"event":"reorg","split_height":N,"new_height":N,"new_blocks":N,"discarded_blocks":N}` |
| `--tx-notify` | `%s` hash | `{"event":"tx","hash":"…"}` |

- **No shell.** A template is split into arguments once — quotes group, there
  are no escapes — and each placeholder is filled in *inside* its argument, so
  a value can never become a second argument or a second command. A spec that
  names a shell (`sh -c '…'`) gets a shell, and everything a shell does with
  what it is given.
- A reorganisation queues `--reorg-notify`, then `--block-notify` for every
  block of the new branch in order, as the C++ and monerod do. Each hook has a
  worker of its own, so one hook's deliveries can interleave with another's; a
  hook's own are always in order. Alternative blocks and transactions leaving
  the pool are not announced.
- **Nothing is announced until the node is synchronized** — an event during
  the initial sync is dropped, not held back — unless `--notify-during-sync`.
- Each hook has one worker that delivers in order. A command gets ten seconds
  and is then killed; at most 1,024 notifications wait, and past that new ones
  are dropped with a warning.
- A webhook is a `POST` of `application/json` with ten-second timeouts, no
  redirects and one retry after a transport failure. **An `https://` URL is not
  available in the daemon**, which carries no TLS client: that hook is disabled
  with a warning, as a C++ build without OpenSSL disables it.
- Specs of which none can be used leave the node without hooks, and it says so
  once at start-up.

Where this differs from the C++, on purpose: a command's standard input is
empty rather than the daemon's own, which would let a hook swallow a line being
typed at the console; and a hook that keeps failing is logged on its first
failure, every hundredth after that and when it recovers, not once per event.
