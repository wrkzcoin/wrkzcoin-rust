# Wallet CLI

`wrkz-wallet` is the interactive command-line wallet: it opens, creates or
restores a wallet file, syncs it from a daemon and sends from it, command for
command with the C++ `wrkz-wallet` (zedwallet++). Its wallet files are the
C++ wallet's own, so one file works in both.

The C++ pages for the same program are the
[Wallet CLI guide](https://docs.wrkz.work/guides/wallet-cli/) and the
[command reference](https://docs.wrkz.work/guides/cli-command-reference/); what
is different here is listed under
[Differences from the C++ wallet](#differences-from-the-c-wallet).

## Starting it

With no options it shows the opening menu and uses the daemon at
`127.0.0.1:17856`:

```sh
wrkz-wallet
```

A wallet file or a password on the command line skips the menu and goes
straight to opening that wallet:

```sh
wrkz-wallet --wallet-file mine.wallet --remote-daemon node-fin.wrkz.work:17856
```

Given `--wallet-file` without `--password`, the password is asked for and never
echoed. A password given with `--password` is visible to anyone who can list
the machine's processes.

| Option | Default | What it does |
| --- | --- | --- |
| `-h`, `--help` | | Print the options and exit |
| `-v`, `--version` | | Print the version line and exit |
| `-r`, `--remote-daemon <host:port>` | `127.0.0.1:17856` | The daemon to sync from and send through. The port may be left off; an IPv6 address goes in brackets, `[::1]:17856`. See [The daemon](#the-daemon) for local sockets |
| `--ssl` | off | Connect to the daemon over `https://` |
| `-w`, `--wallet-file <file>` | | Open this wallet file |
| `-p`, `--password <pass>` | | Its password. An empty password is a password, so `--password ""` also skips the menu |
| `--log-level #` | `1` | `0` disabled, `1` fatal, `2` warning, `3` info, `4` debug, `5` trace |
| `--log-file <file>` | none | Also append log lines to this file. A file that cannot be opened stops the wallet |
| `--threads #` | one per core, at most 16 | Threads that scan downloaded blocks |
| `--skip-coinbase-transactions` | off | Do not scan miner (coinbase) transactions; alias `--skip-coinbase`. Syncs faster, but block rewards paid to this wallet are not seen |
| `--scan-coinbase-transactions` | | Accepted so old command lines run; coinbases are scanned already |
| `--sync-windows` | off | Far below the tip, ask the daemon for four height windows a round instead of one batch. Needs `--skip-coinbase-transactions` and a daemon that offers the `heightRange` and `skipEmptyBlocks` sync features |
| `--sync-max-blocks #` | `1000` | Most blocks to ask for in one request, 1 to 10000. Above 1000 helps only against a daemon started with a higher `--rpc-max-block-count` |

Options take their value as the next argument or after `=`
(`--remote-daemon=[::1]:17856`). An option the wallet does not know is an
error, and so is a `--log-level` outside 0 to 5 or `--threads 0`. The exit
status is `0` on a clean exit and `1` otherwise.

Colour is on unless the `NO_COLOR` environment variable is set.

## The daemon

The wallet needs a daemon for everything but opening a file: a `wrkz-node` or a
C++ `Wrkzd`, local or remote. The daemon is written in one of these forms,
on the command line and at the `swap_node` prompt alike:

| Written as | Means |
| --- | --- |
| `host`, `host:port` | TCP. The port defaults to `17856` |
| `[v6-address]:port` | TCP over IPv6 |
| `/run/wrkz/wrkzd.sock` | the daemon's local IPC socket at that absolute path |
| `ipc:///run/wrkz/wrkzd.sock` | the same, said explicitly |
| `@wrkzd` | a socket in Linux's abstract namespace |

A local socket has no port and no TLS, so `--ssl` and the port do not apply to
it, and `swap_node` does not ask "Does this daemon support SSL?" for one. Local
sockets are not available on Windows, and `@name` only on Linux; either is
refused at start-up with the reason. The daemon side is
[Console and IPC](../node/console-and-ipc.md).

The wallet sends no RPC access token. A daemon started with
`--rpc-access-token` refuses it over TCP; over that daemon's IPC socket it
works, unless the daemon was also given `--rpc-ipc-require-token`.

[Checking a node](../node/checking.md) shows how to confirm a daemon answers
before pointing a wallet at it.

## Creating, opening and restoring

The opening menu takes a command's name or its number:

| Command | What it does |
| --- | --- |
| `open` | Open a wallet already on your system |
| `create` | Create a new wallet |
| `seed_restore` | Restore a wallet using a seed phrase of words |
| `key_restore` | Restore a wallet using a view and spend key |
| `view_wallet` | Import a view only wallet |
| `exit` | Exit the program |

- **`open`** asks for the file name, with or without `.wallet`, and the
  password.
- **`create`** asks for a name and a password (twice), writes `<name>.wallet`,
  and starts the new wallet at the daemon's current height, so there is nothing
  behind it to scan (from zero if the daemon does not answer). It then prints the address, the private spend and view
  keys and the 25-word mnemonic seed, once.
- **`seed_restore`** asks for the 25 words; **`key_restore`** for the private
  spend key and then the private view key. Both then ask for a name, a
  password and the height to start scanning from. Enter for the default of
  zero scans the whole chain; if you do not know the exact height, err on the
  side of caution so transactions are not missed.
- **`view_wallet`** asks for the private view key and the public address. A
  view wallet sees incoming transactions only: it cannot see its own outputs
  being spent, so its balance appears inflated once anything has been spent,
  and it cannot send.

A new wallet name is always given the `.wallet` extension, and a name that is
already taken is refused.

!!! warning "Write the seed down"
    The mnemonic seed, or the pair of private keys, is the wallet. Anyone who
    has it can spend the funds, and if the file and its password are lost the
    seed is the only way back. `backup` shows it again later, after asking for
    the wallet password.

If the daemon does not answer once the wallet is open, a second menu offers
`try_again`, `continue` (to the wallet regardless), `swap_node` and `exit`.

## Commands

At the prompt, type a command by name (case does not matter) or by its number
in the list `help` prints. The list and its order are the C++ wallet's
`allCommands()`, `src/zedwallet++/Commands.cpp:35`. `check_tx` and `decode_integrated` also take their
argument on the same line; every other command asks for what it needs. A view
wallet is offered only the commands marked in the View column.

| Command | Alias | View | What it does |
| --- | --- | --- | --- |
| `help` | `advanced` | yes | List all commands |
| `exit` | | yes | Exit and save your wallet |
| `status` | | yes | Display sync status and network hashrate |
| `refresh` | | yes | Retry syncing with the daemon now |
| `swap_node` | | yes | Specify a new daemon address and port to sync from |
| `address` | `addr` | yes | Display your payment address |
| `balance` | `bal` | yes | Display how much WRKZ you have |
| `incoming_transfers` | `in` | yes | Show incoming transfers |
| `outgoing_transfers` | `out` | no | Show outgoing transfers |
| `list_transfers` | | no | Show all transfers |
| `txs` | | no | Show all transfers in one-line format |
| `txs_full` | | no | Show all transfers with full details |
| `transfer` | | no | Send WRKZ to someone |
| `ab_send` | | no | Send WRKZ to someone in your address book |
| `send_all` | | no | Send all your balance to someone |
| `sweep` | | no | Sweep a specific amount to an address in multiple transactions (no fusion) |
| `sweep_all` | | no | Sweep the entire balance to an address in multiple transactions (no fusion) |
| `get_tx_private_key` | | yes | Get the private key of a transaction |
| `check_tx <hash>` | | yes | Check the wallet's and the node's status for a transaction hash |
| `decode_integrated [address]` | | yes | Decode an integrated address to a standard address and payment ID |
| `ab_add` | | yes | Add a person to your address book |
| `ab_delete` | | yes | Delete a person from your address book |
| `ab_list` | | yes | List everyone in your address book |
| `make_integrated_address` | | yes | Make a combined address and payment ID |
| `backup` | | yes | Back up your private keys and seed, after confirming the password |
| `change_password` | | yes | Change your wallet password |
| `save` | | yes | Save your wallet state |
| `save_csv` | | yes | Save all wallet transactions to a CSV file |
| `reset` | | yes | Recheck the chain from zero for transactions |
| `set_log_level` | | yes | Alter the logging level |

The transaction proof of work every send needs is always computed on this
machine; `wrkz-wallet` has no setting for a
[proof-of-work server](txpow-server.md).

## Files

| File | Where | What it is |
| --- | --- | --- |
| `<name>.wallet` | where you named it | The encrypted wallet |
| `.addressBook.json` | the working directory | The address book, in the shape the C++ writes, so the two wallets share one |
| `transactions.csv` | the working directory | What `save_csv` writes |

The wallet is saved on `exit`, on `save`, after a send or a sweep, and when the
program is stopped with Ctrl-C, `SIGTERM` or `SIGHUP`. Every save writes
`<file>.tmp` first and renames it over the wallet, so a crash mid-save leaves
the previous file intact. Sync progress is not saved as it is made: a wallet
killed outright opens at its last save and syncs that stretch again.

### Compatibility with the C++ wallet

The file is the C++ `WalletBackend` format
(`src/walletbackend/WalletBackend.cpp`): a plaintext marker, a salt, then
the wallet JSON encrypted with AES under a PBKDF2 key from the password
([the wallet file format](../spec/10-wallet.md#wallet-file)). Files written
here open in the C++ `wrkz-wallet` and re-export byte-identically, and files
the C++ writes open here.

The legacy WalletGreen container is not read. Where the C++ wallet, given a
file without the modern marker, tries WalletGreen and converts it, this one
reports that it is not a wallet file.

## Logging

`--log-level` and `--log-file` are the C++ wallet's: 0 disabled to 5 trace,
default 1 (fatal), and `set_log_level` changes the level while the wallet runs.
What is logged is the sync thread's progress, the forks it resolves and the
transactions it adds, a daemon that stops answering, and a save that fails.

Log lines go to standard error. While the wallet is waiting at a prompt, the
prompt is taken off before a log line and drawn again after it, so what you are
typing stays on the last row. Passwords are never echoed or logged, and keys
and the seed are printed only by `create`, `backup` and `get_tx_private_key`.

## Differences from the C++ wallet

Confirmed from the code:

- **Coinbase transactions are scanned by default.** The C++ skips them unless
  `--scan-coinbase-transactions` is given (`src/zedwallet++/ParseArguments.cpp:92`);
  here that flag changes nothing and `--skip-coinbase-transactions` turns
  scanning off.
- **`--threads` defaults to one per core, at most 16**; the C++ default is
  every core (`src/zedwallet++/ParseArguments.cpp:86-89`).
- **`--skip-coinbase-transactions`, `--sync-windows` and `--sync-max-blocks`**
  are this wallet's own; the C++ has none of them.
- **Log lines go to standard error**, around the prompt, rather than straight
  across it on standard output.
- **Dates are printed in UTC.** The C++ uses the local time zone.
- **No WalletGreen files**; see above.
- **No periodic save while syncing.** The C++ `WalletBackend` also saves
  every so often during sync ([spec 10](../spec/10-wallet.md#wallet-file));
  here the file is written at the moments listed under [Files](#files).
- **No node fee warning.** The C++ reads a node fee from a `/fee` route that no
  WrkzCoin daemon serves, so its warning can never fire; it is not reproduced.
- **ANSI colour is written directly**, so an old Windows console that does not
  understand it shows the escape codes; set `NO_COLOR` there.
