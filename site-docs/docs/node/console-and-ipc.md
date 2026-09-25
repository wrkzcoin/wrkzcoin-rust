# Console and IPC

`wrkz-node` can serve its RPC on a local socket as well as on TCP, and it has an
interactive console like the C++ `Wrkzd`'s: typed at the daemon's own terminal,
or, for a daemon with no terminal, reached over that socket with
`wrkz-node attach`.

The C++ counterparts are [IPC and Console](https://docs.wrkz.work/guides/ipc-and-console/) and
[Console Commands](https://docs.wrkz.work/guides/console-commands/) on docs.wrkz.work.

## Local IPC socket

```sh
wrkz-node --data-dir DIR --rpc-ipc-path /run/wrkz/wrkzd.sock
```

serves the whole RPC on a local socket as well as on TCP — the same routes,
off the same workers — as the C++ `--rpc-ipc-path` does. The socket file's
permissions are what decide who may use it, and the kernel enforces them:

- it is created **owner only** (`0600`) and never, even for an instant, wider
  than `--rpc-ipc-mode` asks. To share it, run the node as a service user and
  give it `--rpc-ipc-mode 0660 --rpc-ipc-group wrkz`; a mode that opens it to
  every user, or a group mode with no group named, draws a warning;
- a caller on the socket is **not** asked for `--rpc-access-token` (unless
  `--rpc-ipc-require-token`), and is never rate limited;
- a socket file left by a run that died is cleared at start. A path another
  process is still listening on, or anything that is not a socket, is never
  taken: the IPC listener is reported as not started, and TCP serves on;
- `@name` binds in Linux's abstract namespace: no file, and so **no
  permissions** — every process in the network namespace can connect. It says
  so at start-up;
- not on Windows, for the C++'s reason: there is no dependable permission
  enforcement on a socket file there.

A wallet uses it by giving the path as its daemon address (`/run/wrkz/wrkzd.sock`,
`ipc:///run/wrkz/wrkzd.sock` or `@wrkzd`):

```sh
wrkz-wallet-sync --daemon /run/wrkz/wrkzd.sock ...
```

or by hand: `curl --unix-socket /run/wrkz/wrkzd.sock http://localhost/info`. The
daemon's console is on the socket as well, and nowhere else: see
[Attaching to a running daemon](#attaching-to-a-running-daemon).

## The console

An operator at the terminal can type commands at a running daemon, as they can
at the C++ `Wrkzd`. The reader runs on its own thread and never touches the
engine's; a command takes the same locks the RPC does, in the same order, and
holds none of them while it writes to the terminal, so a slow terminal cannot
delay a block. Every chain, pool and peer value a command prints is read
through the same accessors the RPC handlers use, so the console and the RPC
cannot disagree.

It starts only when **stdin is a terminal**. Under systemd, with stdin from
`/dev/null`, or in a pipeline, nothing is started and the daemon runs exactly as
it did before — there is no keyboard to read. `--no-console` turns it off
explicitly; the periodic status line is printed either way.

On a terminal the prompt has line editing and history, as the C++'s does: Up
and Down go back through the last hundred commands, the line can be edited
where the cursor is, and Tab walks the commands that start the way the word
being typed does. The history lasts until the daemon stops. Where the terminal
cannot take that — `TERM=dumb`, a Windows console too old for virtual-terminal
input — the line is read in the terminal's own line mode instead: backspace
works, the arrow keys and the history do not.

Log lines and console output share one lock, so the two never interleave
mid-line, and a log line arriving while you are typing takes the half-typed
line off the screen and puts it back under itself, cursor and all. Each of the
three streams is checked separately, so redirecting the log away from a
terminal gets no escape sequences in the file.

| Command | What it does | In `Wrkzd` |
| --- | --- | --- |
| `help`, `?` | the command list | yes |
| `exit`, `quit`, `stop` | shut the daemon down, through the SIGINT path | yes |
| `status` | the full status table | yes |
| `height` | local and network height in one line | ours (`/height`) |
| `sync_info` | the same two numbers, the C++ spelling | yes |
| `sync_peers` | active, average batch, demoted | yes |
| `sync_tune` | the above plus the configured tuning | yes |
| `prune_status` | prune mode, depth, floor height | yes |
| `db_status` | the engine, compression, the path, its size and file types, the caches, and the compaction state beside RocksDB's own counters | yes |
| `compact_db [start|force|status|wait]` | start a database compaction, report on it or wait for it ([Database compaction](database.md#database-compaction)) | yes |
| `save` | flush and sync the chain state | yes |
| `snapshot_export [start [height] [path] | status | cancel]` | write a lite node snapshot in the background ([Exporting one](lite-snapshots.md#exporting-one)) | yes |
| `print_pl` | the white and gray peer lists | yes |
| `print_cn` | one row per connection | yes |
| `print_bc <begin> [end]` | block headers over a range, at most 1000 | ours |
| `print_block <hash|height>` | one block as fields | yes |
| `print_tx <hash>` | one transaction as fields, plus its hex | yes |
| `print_pool` | the pool, long format | yes |
| `print_pool_sh` | the pool, one line each | yes |
| `set_log <0-4|name>` | change the log level | yes |
| `log_tail [count]` | the last log lines this daemon emitted, default 20, max 200 | ours |
| `ban list | add <ip> [secs] | delete <ip>` | in-memory host bans | yes |

`print_block` takes a **height** (a count) or a 64-character hash, exactly as
the C++ does. Where our output differs from the C++ it is because the C++ prints
raw JSON and we print labelled fields; `print_pl` prints addresses only, because
that is all `/peers` exposes and the console reads nothing the RPC cannot.
`log_tail` is the way to see the log on a node whose stderr went somewhere the
operator cannot reach.

Every command of the C++ console is here. `compact_db` on a build without
RocksDB says there is nothing on disk to compact, and `snapshot_export` on a
console without the daemon's chain behind it says what it is missing. There is
no `show_hr`/`hide_hr` in either: neither daemon has a built-in miner.

## Attaching to a running daemon

A daemon run by systemd has no terminal to type at. With `--rpc-ipc-path` its
console can be reached anyway, as `Wrkzd attach` reaches the C++ daemon's
(`src/daemon/AttachConsole.cpp`):

```text
$ wrkz-node attach /run/wrkz/wrkzd.sock
Attached to socket /run/wrkz/wrkzd.sock
wrkz-node 1.0.0 (935a145), daemon RPC compatible with WrkzCoin 0.4.8
Commands:
  help         Show this help
  ...
exit or quit leaves this console. stop shuts the daemon down.
> height
Height: 4216460 / 4216460 (100.00%)
> exit
```

`ipc:///run/wrkz/wrkzd.sock`, `@wrkzd` and `--attach PATH` work too. `help` is
sent first, as the connection test. Every line after it runs inside the daemon
exactly as if it had been typed at the daemon's own console, and what the
command printed comes back. On a terminal the prompt has the same history and
line editing as the daemon's own. `exit` and `quit` leave the attached console
and the daemon running; **`stop` shuts the daemon down**, and the console
leaves with it. End of input and Ctrl+C leave too.

- **Who may attach** is whoever may open the socket file: `--rpc-ipc-mode` and
  `--rpc-ipc-group` decide it and the kernel enforces it. No token is sent, so
  a daemon started with `--rpc-ipc-require-token` refuses the attach with a
  401.
- **Every command is allowed**, `ban`, `set_log` and `stop` included, as in the
  C++. A command typed at the daemon's terminal and one sent over the socket
  take turns.
- The daemon serves this as `POST /console` with `{"command":"<line>"}`, **on
  the IPC socket only**: over TCP that path is a 404, token or not. It answers
  `{"output":"…","status":"OK"}` — `Unknown command: <word>` for a word that is
  no command; 503 until the daemon's console is ready, and again once the
  engine has stopped; 400 for a body that is not JSON or has no `command`; 500
  for a `command` that is not a string. `--no-console` does not affect it. Only
  the command's output comes back, not the log lines written while it ran.
- At start-up the daemon logs how to reach it:
  `Console commands are available over socket /run/wrkz/wrkzd.sock: wrkz-node attach /run/wrkz/wrkzd.sock`.
- Exit status 1, with a message: on Windows ("Cannot attach: IPC sockets are not
  available on Windows builds: …"), for an address that is not a socket, when
  the connection test fails ("Could not attach to …"), and for
  `wrkz-node attach` with no socket or more than one ("Usage: …").

Where it differs from `Wrkzd attach`: the `> ` prompt is drawn only when stdin
is a terminal, so a script piped in gets the output and nothing else; and
Ctrl+C leaves with status 0 even in the middle of a command, where the C++ is
killed by the signal.
