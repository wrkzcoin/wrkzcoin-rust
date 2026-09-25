# Wallet Service

`wrkz-service` is the Rust port of the C++ `src/walletservice` (`wrkz-service`,
walletd): one wallet container, held open, served over JSON-RPC 2.0 at
`POST /json_rpc`. It is what an exchange, a tipbot or a payment processor talks
to when it wants a wallet it can drive from another process.

The C++ pages for the same program are the
[wallet service overview](https://docs.wrkz.work/wallet-service-json-rpc/overview/)
and its [method reference](https://docs.wrkz.work/wallet-service-json-rpc/methods/);
the method names, parameters and results there hold here, with the exceptions
below.

## The one thing that is not drop-in

**The API is the C++ service's; the container is the modern one.**

The C++ service is built on `WalletGreen`, the legacy wallet, and opens
WalletGreen containers. The roadmap lists `WalletGreen` under
["things that must not be copied"](../spec/12-roadmap.md#things-that-must-not-be-copied),
and this port has no second container format: the wallet core here reads and
writes the `WalletBackend` container that [`wrkz-wallet`](wallet-cli.md),
[`wrkz-wallet-api`](wallet-api.md) and the [Pluton apps](pluton.md) use.

So:

- **an integration needs no change.** Every method name, parameter, result
  field and error code below is the C++ service's;
- **a container from `wrkz-wallet`, `wrkz-wallet-api` or Pluton opens as it
  is.** A WalletGreen container from the C++ service does not, and no converter
  is planned here — nothing in the field is running that service. If one ever
  turns up, the C++ `wrkz-walletupgrader` converts it.

Two smaller consequences, both answered rather than hidden:

- `getBlockHashes`, and the block-ranged `getTransactions` and
  `getTransactionHashes`, want a block hash per height. A WalletGreen container
  stores every hash it ever synced; a modern one stores the last 50 plus one
  every 5000 blocks, because that is all syncing needs. The hashes the
  container does not have are fetched from the daemon, and `blockCount` is
  capped at **1000** so one call cannot turn into a million round trips. The
  C++ has no such cap because it never needed one.
- `export` writes the container's own plaintext JSON, not a WalletGreen
  container.

## Running it

Generate a container, or restore one you have, then serve it:

```sh
# a new container
wrkz-service -g -w wallet.container -p CONTAINER_PASSWORD

# or restore one
wrkz-service -g -w wallet.container -p CONTAINER_PASSWORD \
    --mnemonic-seed "twenty five words ..." --scan-height 4200000

# serve it
wrkz-service -w wallet.container -p CONTAINER_PASSWORD \
    --rpc-password RPC_SECRET \
    --daemon-address 127.0.0.1 --daemon-port 17856
```

It listens on `127.0.0.1:7856` by default and prints the address it bound. At a
terminal, `exit` (or `quit`, or `stop`) saves and shuts down; `save` and
`status` are there too, and `help` lists them. With no terminal (systemd, a
container) it runs until the process is signalled, and the container is saved
on the way out.

The daemon may be a local IPC socket rather than a port, exactly as for the
wallets: `--daemon-address /run/wrkz/wrkzd.sock`, `--daemon-address @wrkzd` or
`--daemon-address ipc:///run/wrkz/wrkzd.sock` (see
[Console and IPC](../node/console-and-ipc.md)). The service sends no RPC access
token, so a daemon started with `--rpc-access-token` refuses it over TCP; its
IPC socket works unless the daemon was also given `--rpc-ipc-require-token`.

## Options

| Option | What it does |
| --- | --- |
| `-h`, `--help` | print the options and exit |
| `-v`, `--version` | print the version and commit and exit |
| `-w`, `--container-file <file>` | the wallet container |
| `-p`, `--container-password <password>` | its password |
| `-g`, `--generate-container` | make a container and exit |
| `--view-key`, `--spend-key`, `--mnemonic-seed` | with `-g`, restore from these; keys need both `--spend-key` and `--view-key` |
| `--scan-height <n>` | with `-g`, start scanning here |
| `--address` | print the container's addresses and exit |
| `--rpc-password <password>` | the `password` every request must carry. Required |
| `--rpc-legacy-security` | serve with no password at all. Insecure; last resort |
| `--bind-address <ip>`, `--bind-port <port>` | where to listen (default `127.0.0.1:7856`) |
| `--bind-ipc-path <path>` | serve on a local socket at this path *instead of* the port; see below |
| `--bind-ipc-mode <mode>`, `--bind-ipc-group <group>` | the socket file's permissions (default `0600`) and group |
| `--daemon-address <ip or path>`, `--daemon-port <port>` | the daemon to sync from (default `127.0.0.1:17856`) |
| `--daemon-ssl` | the daemon URL is https (needs the `https` feature) |
| `--enable-cors <domain>` | `Access-Control-Allow-Origin`; `*` for all |
| `--tx-notify <cmd or url>` | run a command, or POST to a URL, for every new wallet transaction |
| `--tx-confirmed-notify <cmd or url>` | the same, once a transaction is in a block |
| `--notify-during-sync` | also notify while the wallet is far behind the daemon |
| `-c`, `--config <file>` | read settings from a file; command-line flags win |
| `--dump-config` | print the effective settings and exit |
| `-l`, `--log-file <file>` | the log file (default `service.log`) |
| `--log-level <0-5>` | 0 fatal, 1 error, 2 warning, 3 info, 4 debug, 5 trace (default 3) |
| `--skip-coinbase-transactions` | do not scan coinbase outputs; alias `--skip-coinbase` |
| `--sync-windows` | far below the tip, ask the daemon for four height windows a round; needs `--skip-coinbase-transactions` and a daemon offering `skipEmptyBlocks` |
| `--sync-max-blocks <n>` | most blocks one sync request asks for (default 1000, at most 10000; above 1000 needs a daemon with a higher `--rpc-max-block-count`) |

`--daemon-ssl`, `--skip-coinbase-transactions`, `--sync-windows` and
`--sync-max-blocks` are this port's own; the C++ service has none of them.
Coinbase outputs are scanned unless `--skip-coinbase-transactions` is given;
`--scan-coinbase-transactions` is accepted and changes nothing. Each option
takes its value as the next argument (`--bind-port 7856`, not
`--bind-port=7856`).

`--dump-config` writes `key=value` lines that `--config` reads back, and never
prints either password. It does print `tx-notify` and `tx-confirmed-notify` as
given, so a token inside one of those is in the dump too.

Every option has a configuration-file key of the same name. A boolean key
(`rpc-legacy-security`, `notify-during-sync`, `daemon-ssl`,
`skip-coinbase-transactions`, `sync-windows`) is on for `1` or `true`, as the
C++ reads it, and a line is a comment only when it *starts* with `#` or `;`,
so a command or a URL may contain either character. A JSON object of the same
keys, one per line, is read too, which is what the C++ `--save-config` writes.

```text
daemon-address=127.0.0.1
daemon-port=17856
bind-port=7856
container-file=wallet.container
log-level=3
tx-notify=/usr/local/bin/on-tx %s %a
```

## A local socket instead of a port

```sh
wrkz-service -w wallet.container -p CONTAINER_PASSWORD --rpc-password RPC_SECRET \
    --bind-ipc-path /run/wrkz/wrkz-service.sock
```

With `--bind-ipc-path` the JSON-RPC is served on an `AF_UNIX` socket and **no
TCP port is opened**, which is what the C++ does
(`src/walletservice/PaymentGateService.cpp:203`). `@name` binds in Linux's
abstract namespace, which has no file and so no permissions: every process in
the network namespace can connect.

- The socket file is created owner-only and widened to `--bind-ipc-mode` only
  after `--bind-ipc-group` is applied, so it is never more open than asked. Use
  `0660` with a group to share it.
- **The `password` member is still required** on the socket: the C++ checks it
  in the JSON-RPC layer whatever the transport. The file's mode decides who may
  connect; the password decides who may do anything.
- A stale socket file from an earlier run is replaced; a socket another process
  is still listening on, or a path that is not a socket, is refused.
- Not available on Windows, where the service refuses to start with this
  option, as the C++ does.

## Transaction notifications

`--tx-notify` and `--tx-confirmed-notify` take either an `http://` or
`https://` URL, which each notification is POSTed to as JSON, or a command,
which is run **without a shell** — split on whitespace, with `'` and `"`
grouping — and with these placeholders substituted in each argument:

| Placeholder | Webhook member | |
| --- | --- | --- |
| `%s` | `hash` | the transaction hash |
| `%h` | `height` | the block height, `0` while unconfirmed |
| `%a` | `amount` | the net amount to the wallet; negative for a send |
| `%f` | `fee` | the fee |
| `%p` | `paymentId` | the payment id, or empty |
| `%c` | `confirmed` | `1`/`true` once in a block, `0`/`false` before |
| | `timestamp`, `unlockTime` | as the transaction records them |

`%%` is a literal `%`. The webhook body starts with `"event"`: `tx` for
`--tx-notify`, `tx_confirmed` for `--tx-confirmed-notify`.

```sh
wrkz-service ... --tx-notify "/usr/local/bin/on-tx %s %a" \
                 --tx-confirmed-notify http://127.0.0.1:9000/confirmed
```

When they fire, following `WalletService::onTransactionEvent`
(`src/walletservice/WalletService.cpp:829`):

- **`tx`**, once per transaction: for a send, as soon as `sendTransaction` or
  `sendDelayedTransaction` has relayed it (height `0`, `%c` `0`); for anything
  else, when sync finds it in a block.
- **`tx_confirmed`**, once, when the transaction is in a block: straight after
  `tx` for one sync found, and when it confirms for one this service sent.
- A transaction sync finds more than 1440 blocks (a day) below the daemon's
  height is not announced, so a rescan does not replay history, unless
  `--notify-during-sync` is given.

One difference follows from the container: a WalletGreen container sees
*incoming* transactions while they are still in the pool and announces them
with height `0`; the modern container does not track other people's pool
transactions, so an incoming one is announced when it is mined, with `tx` and
`tx_confirmed` together.

Each hook runs one delivery at a time on its own thread, with a queue of 1024;
a command still running after ten seconds is killed. A command runs with the
service's user and environment and an empty standard input. A shell is only
involved if the command names one (`sh -c ...`), and then that shell interprets
whatever it is given. An `https://` webhook needs a build with the `https`
feature, which is the default.

## Logging

Log lines go to standard error and to the log file, `service.log` in the
working directory unless `-l` says otherwise, as the C++ always writes one. The
file is created if it is missing and appended to; unlike the C++'s, it is capped
and rotated. `--log-level` is the C++ `Logging::Level` numbering, 0 fatal to 5
trace, default 3 (info). At info the log carries start-up, the listener, the
transactions sync finds, forks, a daemon that stops or starts answering again,
and the notification hooks. Block-by-block progress is debug, and so is each
request: its peer, path and status, and its JSON-RPC method name. No password,
key, seed or request body is ever logged.

## The API

One route, `POST /json_rpc`; anything else is a 404. A request is a JSON object:

```json
{"jsonrpc": "2.0", "id": 1, "password": "RPC_SECRET",
 "method": "getBalance", "params": {}}
```

**The password is a member of the request, not a header.** That is the C++'s
design and it is kept, so an existing client works unchanged.

The methods, in the order the C++ registers them:

`save`, `export`, `reset`, `createAddress`, `createAddressList`,
`deleteAddress`, `getSpendKeys`, `getBalance`, `getBlockHashes`,
`getTransactionHashes`, `getTransactions`, `getUnconfirmedTransactionHashes`,
`getTransaction`, `sendTransaction`, `createDelayedTransaction`,
`getDelayedTransactionHashes`, `deleteDelayedTransaction`,
`sendDelayedTransaction`, `getViewKey`, `getMnemonicSeed`, `getStatus`,
`getAddresses`, `createIntegratedAddress`, `getFeeInfo`, `getNodeFeeInfo`.

Each one's parameters and result are in the crate documentation
(`cargo doc -p wrkz-service --open`, module `methods`), which cites the C++
handler and the `src/walletservice/WalletService.cpp` line for every one; the source is
[`crates/wrkz-service/src/methods.rs`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/crates/wrkz-service/src/methods.rs).

A delayed transaction is built but not relayed. `createDelayedTransaction`
returns its hash and fee, `getDelayedTransactionHashes` lists them,
`sendDelayedTransaction` relays one after re-checking that its inputs are still
spendable, and `deleteDelayedTransaction` throws one away. They live in memory:
a restart forgets them, as it does in the C++.

The transaction proof of work a send needs is computed on this machine; the
service has no setting for a [proof-of-work server](txpow-server.md).

## Errors

An error is still HTTP **200**, with the failure in the envelope:

```json
{"jsonrpc": "2.0", "id": 1,
 "error": {"code": -32700, "message": "Bad address",
           "data": {"application_code": 7}}}
```

**`code` is `-32700` for every application error**, whatever went wrong:
`makeErrorResponse` in the C++ (`src/jsonrpcserver/JsonRpcServer.cpp:121`)
writes `errParseError` unconditionally. Branch on
`data.application_code` and read `message`. Do not "fix" the code — an
integration written against the C++ service depends on it.

The other envelope codes are the real JSON-RPC ones: `-32601` method not found,
`-32604` invalid or missing password, `-3600` a request with no usable `method`,
and `-32700` with the message `Parse error` for a body that is not JSON.

`application_code` comes from two C++ categories that share their numbering, so
the message is what tells them apart: `WalletErrorCodes` (1..=35, e.g. 7
`BAD_ADDRESS`, 9 `WRONG_AMOUNT`, 23 `OBJECT_NOT_FOUND`) and
`WalletServiceErrorCode` (1..=6, e.g. 1 `WRONG_KEY_FORMAT`, 3
`WRONG_HASH_FORMAT`, 4 `OBJECT_NOT_FOUND`, 5 `DUPLICATE_KEY`). A missing
required parameter is the generic `Request error` with **no** `data` member.

## What is not covered

- **No WalletGreen containers**; see
  [the one thing that is not drop-in](#the-one-thing-that-is-not-drop-in).
- **No incoming pool notifications**; see
  [Transaction notifications](#transaction-notifications).
- **No Windows service registration** (`--daemonize`, `--register-service`,
  `--unregister-service`).
- **No `--server-root`, `--save-config` or `--init-timeout`**
  (`src/walletservice/WalletServiceConfiguration.cpp:61-69`). They are refused
  as unknown options, on the command line and as keys in a `--config` file;
  `--dump-config` stands in for `--save-config`.
- **`getFeeInfo` is always empty.** `Nigel` fills the node fee from a `/fee`
  route no WrkzCoin daemon serves, so the C++ answers `""` and `0` too.
- **`transaction.extra` is empty** in `getTransaction` and `getTransactions`.
  The modern container keeps a transaction's payment id rather than its raw
  `tx_extra` bytes, so there is nothing to hex-encode; `paymentId` carries what
  the extra was read for.
