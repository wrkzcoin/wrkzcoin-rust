# Transaction PoW Server

`wrkz-txpow-server` computes the transaction proof of work for wallets that
would rather not — phones and browsers — and hands back the nonce. It is the
Rust port of the C++ `wrkz-txpow-server`, whose page on the C++ site is the
[Transaction PoW Server guide](https://docs.wrkz.work/guides/txpow-server/).

Every WrkzCoin transaction carries a small proof of work: the wallet appends a
nonce to the transaction extra and searches until the `cn_upx` hash of the
unsigned prefix meets a difficulty derived from the number of inputs and
outputs — `40,000 + (inputs + 4 × outputs) × 1,000`, so two inputs and six
outputs need 66,000. The daemon refuses a transaction without it unless the
transaction pays at least 10,000 atomic units (100 WRKZ) in fees. The rule is
in [spec 06, Transactions](../spec/06-transactions.md).

On a desktop that search takes seconds. On a phone it takes longer, and in a
browser, which has one thread and no AES instructions, it is impractical.
`wrkz-txpow-server` moves the search to a machine that is good at it.

This port has the same options, defaults, routes, JSON and status codes as the
C++ server (`src/txpowserver/main.cpp`, `src/txpowserver/HttpApi.cpp`), so
either server serves either wallet. The only deliberate difference is that job
ids come from the system CSPRNG rather than a seeded Mersenne Twister
(`src/txpowserver/PowService.cpp:285-288`) — a job id is what `DELETE`
accepts, so it should not be guessable.

## What the server sees, and what it cannot do

The proof of work is computed over the prefix *before* the ring signatures are
made, so the server receives exactly the bytes the daemon sees at broadcast a
few seconds later: key images, ring member offsets, output amounts and
one-time keys, and the extra field.

It never sees a key, a seed or a signature. It cannot alter the transaction:
the wallet accepts only eight nonce bytes back and re-checks them with one
hash before signing. **The worst a bad server can do is waste the wallet's
time**, after which the wallet computes the proof itself.

Run it where the wallet's remote node already runs and nothing new is learned
by anyone. A third-party server is one more party that learns the sender's IP
address and transaction shortly before broadcast.

## Running it

```sh
cargo build --release -p wrkz-txpow-server
./target/release/wrkz-txpow-server --bind-ip 0.0.0.0 --bind-port 17870 --threads 8
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--bind-ip <ip>` | `127.0.0.1` | Interface to listen on. `0.0.0.0` or `::` for remote wallets |
| `--bind-port #` | `17870` | TCP port |
| `--bind-ipv6-address <ipv6>` | off | Second, IPv6-only listener on the same port |
| `--trusted-proxy <ip>` | none | A reverse proxy in front of the server. Repeat or comma-separate for several |
| `--threads #` | every hardware thread | Hashing threads. `0` also means one per hardware thread. All of them work on one job at a time |
| `--rate-limit #` | `60` | Requests per minute from one client address. `0` disables |
| `--max-jobs-per-minute #` | `120` | Jobs accepted per minute across all clients. `0` disables |
| `--max-queue #` | `64` | Jobs allowed to wait before new ones get `503` |
| `--max-difficulty #` | `1000000` | Refuse anything harder |
| `--max-wait-ms #` | `30000` | Longest a request may be held open waiting for its result |
| `--job-timeout #` | `600` | Seconds after which an uncollected queued job is dropped |
| `--result-ttl #` | `300` | Seconds a finished result stays available for polling |
| `--api-key <key>` | none | Require this value in the `X-API-KEY` header |
| `--enable-cors <domain>` | none | `Access-Control-Allow-Origin` value. The web wallet needs this |
| `--log-level <level>` | `info` | `trace`, `debug`, `info`, `warning`, `fatal` or `disabled` |
| `--log-file <file>` | none | Also append log lines to this file |

`-h`/`--help` and `-v`/`--version` print the options and the version. Options
take their value as the next argument or after `=`. `--enable-cors` must be
`*`, `null` or a full origin such as `https://rust-wallet.wrkz.work`, with no
trailing slash; anything else is refused at start-up, since a browser would
silently ignore it. Quote `'*'`, or the shell expands it to a file name.

There is no service wrapper. On Linux a systemd unit does the job (the node's is
under [systemd](../node/systemd.md)); on Windows, Task Scheduler or NSSM.

### Behind a reverse proxy

The server speaks plain HTTP. For HTTPS, put it behind the proxy that already
terminates TLS for the node. Two things matter:

- **Tell the server who the proxy is.** Every request then arrives from the
  proxy's address, so without `--trusted-proxy` the per-address rate limit
  would treat all wallets as one client. With it, requests from that address
  are attributed to the client in `X-Real-IP`, or the last entry of
  `X-Forwarded-For` — the one the proxy itself appended. Requests from any
  other address keep their own address, so the headers cannot be forged.
- **Let the proxy hold a request open** for at least the long-poll length.
  Wallets ask for 20 seconds; the server caps it at `--max-wait-ms`.

```nginx
server {
    listen 443 ssl;
    server_name node.example.com;

    # The trailing slash on proxy_pass strips the prefix, so the server keeps
    # seeing /pow, /stats and /health.
    location /txpow/ {
        proxy_pass         http://127.0.0.1:17870/;
        proxy_http_version 1.1;
        proxy_set_header   Host              $host;
        proxy_set_header   X-Real-IP         $remote_addr;
        proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_read_timeout 90s;
        proxy_buffering    off;
    }
}
```

```sh
wrkz-txpow-server --bind-ip 127.0.0.1 --bind-port 17870 --trusted-proxy 127.0.0.1
```

In the wallet, enter `https://node.example.com/txpow`. Serving from the root
works the same with a plain host name.

**Do not set CORS twice.** For the web wallet it comes either from
`--enable-cors` here or from `add_header` in nginx — not both, or the browser
sees the header twice and rejects it.

### Throughput

The hashing is this repository's Rust `cn_upx`, the same function the wallets
and the daemon use. A 16-thread machine manages a few thousand hashes a
second, so a typical transaction is tens of seconds of the whole machine.
`--max-queue` and `--max-jobs-per-minute` exist so that a burst turns into
refusals — which make the wallets compute locally — rather than a queue nobody
lives to see the end of.

## Protocol

All bodies are JSON. Every job reply has a `status` of `done`, `pending`,
`cancelled` or `error`; an error reply carries its reason in `error`.

### `POST /pow`

```json
{ "prefix": "<hex of the serialized transaction prefix>", "wait_ms": 20000, "height": 4300000 }
```

The prefix must already end with the PoW tag byte `0x04` and eight nonce bytes
(the wallet zero-fills them). `wait_ms` is optional and capped at
`--max-wait-ms`; the request is held open that long waiting for a result.
`height` is optional and only selects historical difficulty rules.

While the job is queued or running (HTTP `200`):

```json
{ "status": "pending", "job_id": "…32 hex…", "state": "running",
  "difficulty": 66000, "hashes": 12800, "elapsed_ms": 3100 }
```

Once solved (HTTP `200`):

```json
{ "status": "done", "job_id": "…", "nonce": "3a9f1c0000000000",
  "difficulty": 66000, "hashes": 71424, "elapsed_ms": 14020 }
```

`nonce` is the eight bytes to copy over the trailing eight bytes of the
prefix, in that byte order. The wallet checks `cn_upx(prefix)` against the
difficulty before it signs.

Refusals:

| Code | When |
| --- | --- |
| `400` | The prefix does not parse, is not canonically serialized, has no fee, carries too many outputs or an unreasonable ring, lacks the nonce tag, or is above `--max-difficulty` |
| `401` | API key missing or wrong |
| `429` | The client, or the server as a whole, is over its per-minute limit |
| `503` | The queue is full, or the server is shutting down |

### Other routes

| Route | What it does |
| --- | --- |
| `GET /pow/<job_id>?wait_ms=20000` | The same job reply. `404` for an unknown or expired job |
| `DELETE /pow/<job_id>` | Cancels a queued or running job |
| `GET /stats` | Counters since start-up |
| `GET /health` | Queue depth and capacity, for load balancers. Outside the API key and the rate limit |
| `GET /` | Name, version and the endpoint list |

`/stats` reports jobs received, accepted, completed, failed, cancelled,
expired and rejected by reason; total hashes, busy time and average hash rate;
average and worst solve time; queue depth and the shape of the job currently
running (never its id, which would let a reader cancel it); HTTP request
counts; the two limits a client can act on; and the version.

It deliberately does **not** describe the deployment — bind addresses, trusted
proxies, rate limits, the CORS origin and whether an API key is required stay
in the start-up banner and the operator's command line.

## Wallet side

[Rust Pluton Wallet](pluton.md) has a *Transaction proof-of-work server*
section in Settings: a URL, an optional API key, a *Test* button and *Apply*.

- **An empty URL means no server**, and that is how a new install starts:
  desktop and Android then compute the proof of work on their own CPU. The
  field suggests `https://txpow.wrkz.work` as a placeholder, which is not used
  until it is entered and applied.
- In the browser the built-in search is impractical, so without a server the
  web wallet pays the 100 WRKZ fee that lets a transaction skip the proof of
  work. With a server it pays the ordinary minimum fee.
- *Test* calls `/health` over the same path a transaction would take and
  reports latency, thread count and queue occupancy without saving anything.
- When a server is configured the wallet asks it first and waits up to two
  minutes. If it is unreachable, refuses, times out, or returns a nonce that
  does not verify, the wallet computes the proof itself.

The Rust client differs from the C++ one (`src/nigel/TxPowClient.cpp`) in
three ways: the server is one URL — scheme, host, port and mount path together
— instead of a host, a port and an SSL switch; it can send an API key, which
the C++ client cannot; and it sends the height, so the server works to the
difficulty the wallet will be judged at.

[`wrkz-wallet`](wallet-cli.md), [`wrkz-wallet-api`](wallet-api.md),
[`wrkz-service`](service.md) and `wrkz-wallet-send` always compute the proof of
work on their own CPU and have no setting for this.
