# Wallet API

`wrkz-wallet-api` serves one wallet over an HTTP REST API, route for route with
the C++ `wrkz-wallet-api`: the same paths, request and response bodies, status
codes and error codes, so a client written against the C++ program works
unchanged. It is what a service drives when it wants a wallet it can open,
query and send from over HTTP.

The request and response bodies are documented once, on the C++ site:
[Wallet API overview](https://docs.wrkz.work/wallet-api/overview/),
[endpoints](https://docs.wrkz.work/wallet-api/endpoints/) and
[running the wallet API](https://docs.wrkz.work/guides/running-wallet-api/).
This page covers how the Rust program is run, and what differs.

## Running it

`--rpc-password` is required; it is the key every request must carry.

```sh
wrkz-wallet-api --rpc-password API_SECRET
```

It listens on `http://127.0.0.1:7856` and starts with no wallet open. A client
then opens or creates one, which is also when it names the daemon:

```sh
curl -s -X POST http://127.0.0.1:7856/wallet/open \
  -H "X-API-KEY: API_SECRET" -H "Content-Type: application/json" \
  -d '{"filename": "mine.wallet", "password": "WALLET_PASSWORD",
       "daemonHost": "127.0.0.1", "daemonPort": 17856}'

curl -s -H "X-API-KEY: API_SECRET" http://127.0.0.1:7856/status
```

At a terminal, `exit` or `quit` saves the open wallet and shuts down. With
`--no-console` (systemd, a container) it runs until it is signalled. Ctrl-C,
`SIGTERM` and `SIGHUP` shut it down the way `exit` does, so the wallet is saved
either way. The exit status is `0` on a clean shutdown and `1` for a bad
argument or an IPv4 listener that could not bind.

| Option | Default | What it does |
| --- | --- | --- |
| `-h`, `--help` | | Print the options and exit |
| `-v`, `--version` | | Print the version line and exit |
| `-r`, `--rpc-password <password>` | required | The key every request must send in `X-API-KEY` |
| `--rpc-bind-ip <ip>` | `127.0.0.1` | Interface for the IPv4 listener |
| `-p`, `--port <port>` | `7856` | Port for every TCP listener |
| `--rpc-use-ipv6` | off | Enable the IPv6 listener |
| `--rpc-bind-ipv6-address <ipv6>` | empty | Its address, for example `::1`. The IPv6 listener runs only with both options |
| `--rpc-ipc-path <path>` | empty | Also serve the API on a local socket at this path, or `@name` in Linux's abstract namespace. Not on Windows |
| `--rpc-ipc-mode <mode>` | `0600` | Octal permissions of the socket file; `0660` with `--rpc-ipc-group` to share it |
| `--rpc-ipc-group <group>` | | Group to own the socket file |
| `--enable-cors <domain>` | empty | Send `Access-Control-Allow-Origin: <domain>`; `*` for all |
| `--tx-notify <cmd\|url>` | empty | Run a command, or POST to an `http(s)://` URL, for every transaction a sync records for the open wallet |
| `--notify-during-sync` | off | Also fire `--tx-notify` while the wallet is far behind the daemon |
| `--log-level #` | `0` | `0` disabled, `1` fatal, `2` warning, `3` info, `4` debug, `5` trace |
| `--log-file <file>` | none | Also append log lines to this file. A file that cannot be opened stops the program |
| `--no-console` | off | No interactive console |
| `--threads #` | one per core, at most 16 | Threads that scan downloaded blocks |
| `--skip-coinbase-transactions` | off | Do not scan miner (coinbase) transactions; alias `--skip-coinbase` |
| `--scan-coinbase-transactions` | | Accepted so old command lines run; coinbases are scanned already |
| `--sync-windows` | off | Far below the tip, ask the daemon for four height windows a round. Needs `--skip-coinbase-transactions` and a daemon that offers the `heightRange` and `skipEmptyBlocks` sync features |
| `--sync-max-blocks #` | `1000` | Most blocks to ask for in one request, 1 to 10000. Above 1000 helps only against a daemon started with a higher `--rpc-max-block-count` |

Options take their value as the next argument or after `=`. An unknown option,
a `--log-level` outside 0 to 5 and `--threads 0` are refused.

!!! warning "Keep it on loopback"
    The API speaks plain HTTP and holds a wallet that can send. Bind it to
    `127.0.0.1` unless it is behind a reverse proxy you trust that adds TLS,
    and use a long random `--rpc-password`.

## The daemon

The daemon is given in the body of `POST /wallet/open`, `/wallet/create` and
the three `/wallet/import/*` routes, and changed later with `PUT /node`:

| Member | Default | |
| --- | --- | --- |
| `daemonHost` | `127.0.0.1` | A host name or address, or a local IPC socket: `/path`, `ipc:///path` or `@name` |
| `daemonPort` | `17856` | Ignored for a socket |
| `daemonSSL` | `false` | Connect over `https://` |

The API sends no RPC access token to the daemon. A daemon started with
`--rpc-access-token` refuses it over TCP; over that daemon's IPC socket it
works, unless the daemon was also given `--rpc-ipc-require-token`. See
[Console and IPC](../node/console-and-ipc.md).

## Requests and answers

Each request passes these checks, in order:

1. **The route.** A path that matches no route is `404` with an empty body,
   before anything else. `{address}` is `Wrkz` followed by 94 letters and
   digits, `{hash}` 64 hex characters and `{paymentID}` 16 or 64.
2. **The key.** `X-API-KEY` missing or wrong is `401` with an empty body. The
   key is compared by its PBKDF2-SHA256 hash (10,000 iterations, a random salt
   per process), never as the plain string.
3. **The wallet state.** A route that needs an open wallet when none is open,
   or a `POST /wallet/*` while one is, is `403` with an empty body.
4. **View wallets.** A view-only wallet calling a route that needs the spend
   keys is `400` with `{"errorCode": 39, "errorMessage": ...}`.
5. **The handler.** A wallet error is `400` with `{"errorCode", "errorMessage"}`;
   a missing or mistyped JSON parameter is `400` with an empty body.

`OPTIONS` on any path answers the CORS preflight without the key. With
`--enable-cors`, every answer carries `Access-Control-Allow-Origin`.

A JSON answer of at least a kilobyte is gzipped for a client that sends
`Accept-Encoding: gzip`. A full request queue is shed with `503`.

## Routes

The table is the C++ `ApiDispatcher::setupRoutes`
(`src/walletapi/ApiDispatcher.cpp:124`), in its order. "Open" is whether a wallet must be open (`closed`: must not be); "View" is
whether a view-only wallet may call it. Bodies are on the
[C++ endpoint reference](https://docs.wrkz.work/wallet-api/endpoints/).

| Method | Path | Open | View | What it does |
| --- | --- | --- | --- | --- |
| `POST` | `/wallet/open` | closed | yes | Open a wallet file |
| `POST` | `/wallet/import/key` | closed | yes | Restore a wallet from its private spend and view keys |
| `POST` | `/wallet/import/seed` | closed | yes | Restore a wallet from its mnemonic seed |
| `POST` | `/wallet/import/view` | closed | yes | Import a view-only wallet |
| `POST` | `/wallet/create` | closed | yes | Create a new wallet |
| `POST` | `/addresses/create` | open | no | Create a random subwallet address |
| `POST` | `/addresses/import` | open | no | Import a subwallet from its private spend key |
| `POST` | `/addresses/import/deterministic` | open | no | Import the deterministic subwallet at an index |
| `POST` | `/addresses/import/view` | open | yes | Import a view-only subwallet from its public spend key |
| `POST` | `/addresses/validate` | any | yes | Validate an address |
| `POST` | `/transactions/send/prepared` | open | no | Relay a prepared transaction |
| `POST` | `/transactions/prepare/basic` | open | no | Prepare a basic transaction without relaying it |
| `POST` | `/transactions/send/basic` | open | no | Send a basic transaction |
| `POST` | `/transactions/prepare/advanced` | open | no | Prepare an advanced transaction without relaying it |
| `POST` | `/transactions/send/advanced` | open | no | Send an advanced transaction |
| `POST` | `/transactions/send/sweep` | open | no | Sweep an amount to an address |
| `POST` | `/transactions/send/sweep/all` | open | no | Sweep the whole balance to an address |
| `POST` | `/export/json` | open | yes | Write the wallet as plaintext JSON to a file |
| `DELETE` | `/wallet` | open | yes | Save and close the wallet |
| `DELETE` | `/addresses/{address}` | open | yes | Delete a subwallet |
| `DELETE` | `/transactions/prepared/{hash}` | open | no | Discard a prepared transaction |
| `PUT` | `/save` | open | yes | Save the wallet |
| `PUT` | `/reset` | open | yes | Rescan from a height |
| `PUT` | `/node` | open | yes | Change the daemon |
| `PUT` | `/sync/refresh` | open | yes | Restart syncing now |
| `GET` | `/node` | open | yes | The daemon and its fee |
| `GET` | `/keys` | open | yes | The private view key |
| `GET` | `/keys/{address}` | open | no | A subwallet's spend keys |
| `GET` | `/keys/mnemonic/{address}` | open | no | A subwallet's mnemonic seed |
| `GET` | `/status` | open | yes | Wallet, daemon and network heights, peers, hashrate |
| `GET` | `/addresses` | open | yes | Every address in the wallet |
| `GET` | `/addresses/primary` | open | yes | The primary address |
| `GET` | `/addresses/{address}/{paymentID}` | open | yes | Make an integrated address |
| `GET` | `/transactions` | open | yes | Every transaction |
| `GET` | `/transactions/unconfirmed` | open | yes | Transactions not yet in a block |
| `GET` | `/transactions/unconfirmed/{address}` | open | yes | The same, for one address |
| `GET` | `/transactions/{startHeight}` | open | yes | Transactions from a height |
| `GET` | `/transactions/{startHeight}/{endHeight}` | open | yes | Transactions in a height range |
| `GET` | `/transactions/address/{address}/{startHeight}` | open | yes | One address's transactions from a height |
| `GET` | `/transactions/address/{address}/{startHeight}/{endHeight}` | open | yes | One address's transactions in a height range |
| `GET` | `/transactions/privatekey/{hash}` | open | no | A sent transaction's private key |
| `GET` | `/transactions/hash/{hash}` | open | yes | One transaction |
| `GET` | `/transactions/paymentid/{hash}` | open | yes | Transactions with this payment ID |
| `GET` | `/transactions/paymentid` | open | yes | Transactions that carry a payment ID |
| `GET` | `/balance` | open | yes | The total balance |
| `GET` | `/balance/{address}` | open | yes | One address's balance |
| `GET` | `/balances` | open | yes | The balance of every address |
| `OPTIONS` | any | | | CORS preflight; no key needed |

Read-only routes (status, balances, addresses, transactions, keys, node, save,
export) answer from the state after the last completed change, so they never
wait for a sync round trip, a proof-of-work search or a send in progress.
Changes are serialised: two sends never build at once.

The transaction proof of work every send needs is searched on every core of
this machine; the API has no setting for a
[proof-of-work server](txpow-server.md).

## Local socket

```sh
wrkz-wallet-api --rpc-password API_SECRET --rpc-ipc-path /run/wrkz/wallet-api.sock
```

The socket is served in addition to the TCP port, with the same routes, and
**`X-API-KEY` is still required on it**: the socket file's mode decides who may
connect, the key who may do anything. `@name` binds in Linux's abstract
namespace, which has no file and so no permissions. On a platform without
local sockets the option is ignored with a message, and an IPv6 or IPC listener
that cannot start is reported while the others carry on; only the IPv4
listener is fatal.

```sh
curl -s --unix-socket /run/wrkz/wallet-api.sock -H "X-API-KEY: API_SECRET" http://localhost/status
```

## Transaction notifications

`--tx-notify` fires for every transaction a sync step records for the open
wallet: incoming, outgoing once it is mined, and coinbase. A send is announced
when it is mined, not when it is relayed. A command is run without a shell,
with these placeholders substituted; a URL is POSTed JSON:

| Placeholder | Webhook member | |
| --- | --- | --- |
| `%s` | `hash` | the transaction hash |
| `%h` | `height` | the block height |
| `%a` | `amount` | the net amount to the wallet; negative for a send |
| `%f` | `fee` | the fee |
| `%p` | `paymentId` | the payment ID, or empty |
| `%c` | `confirmed` | always `1` / `true` |
| | `timestamp`, `unlockTime`, `isCoinbase` | as the transaction records them |

The webhook body's `"event"` is `tx`. A transaction more than 1440 blocks
(about a day) below the daemon's height is not announced, so a rescan does not
replay the wallet's history, unless `--notify-during-sync` is given. The start-up
banner says whether notifications are on, without printing the command or URL,
which may carry a token. The runner is the one [`wrkz-service`](service.md#transaction-notifications)
uses.

## Logging

`--log-level` is the C++ numbering, 0 disabled to 5 trace, default 0. Each
request is logged with its peer, method, path, status and time: a server error
at warning, a request refused for a wrong key at info, and every other request
at debug. The query string, the headers and the body are never logged, so neither
is the API key, a password or a private key.

## Differences from the C++ wallet API

Confirmed from the code:

- **Coinbase transactions are scanned by default.** The C++ skips them unless
  `--scan-coinbase-transactions` is given (`src/walletapi/ParseArguments.cpp:60`);
  here that flag changes nothing and `--skip-coinbase-transactions` turns
  scanning off.
- **`--threads` defaults to one per core, at most 16**; the C++ default is
  every core (`src/walletapi/ParseArguments.cpp:64-67`).
- **`--skip-coinbase-transactions`, `--sync-windows` and `--sync-max-blocks`**
  are this program's own; the C++ has none of them.
- **No request bodies or keys in the log.** The C++ prints every request and
  its body on standard output (`src/walletapi/ApiDispatcher.cpp:528-547`), and
  on a wrong key prints the expected one next to it
  (`src/walletapi/ApiDispatcher.cpp:645`).
- **Reads do not wait.** The C++ takes a shared lock for reads; here a read
  sees the state after the last completed change, one sync step coarser.
- **The node fee is always `0` and `""`** in `GET /node`. The C++ reads it from a
  `/fee` route no WrkzCoin daemon serves, so it answers the same.
- **No periodic save while syncing.** The file is written on `PUT /save`,
  `PUT /reset`, `DELETE /wallet` and shutdown; a process killed outright opens
  at its last save and syncs that stretch again.
- **No WalletGreen files**: `/wallet/open` reads the modern wallet file only,
  as [`wrkz-wallet`](wallet-cli.md#compatibility-with-the-c-wallet) does.
