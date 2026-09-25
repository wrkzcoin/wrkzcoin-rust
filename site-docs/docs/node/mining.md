# Mining

A stock miner such as xmrig points straight at `wrkz-node` through its built-in
stratum server, the same one `Wrkzd` has (`src/daemon/StratumServer.cpp`), with
the same four options; a pool keeps using `getblocktemplate` and `submitblock`
on the RPC port.

The C++ counterpart is [Solo Mining](https://docs.wrkz.work/guides/solo-mining/)
on docs.wrkz.work.

```sh
wrkz-node --data-dir ./wrkz-rust --stratum-bind-port 3333
xmrig -o 127.0.0.1:3333 -u Wrkz...your-address... -p x -a cn/upx2
```

The login is the address the block reward goes to; the password is ignored.
Each job names its algorithm (`cn/upx2` for the current major version 7), and
each connection gets its own extra nonce, so several rigs on one node never
grind the same nonce space.

| Option | Default | Meaning |
| --- | --- | --- |
| `--stratum-bind-port PORT` | `0` | serve the stratum server on `PORT`; `0` leaves it off |
| `--stratum-bind-ip ADDR` | `127.0.0.1` | its listening address |
| `--stratum-share-difficulty N` | `0` | difficulty miners are given; `0` is the network difficulty |
| `--stratum-max-connections N` | 32 | miners allowed at once; `0` is taken as 1 |

## Why stratum and not xmrig's `--daemon` mode {#why-stratum}

Our blocks are Forknote lineage: the real timestamp and nonce live inside a
merge-mining parent block, where a miner expecting Monero's flat header cannot
find them, so xmrig's `--daemon` mode rejects the template — against `Wrkzd` as
well. Over stratum the node assembles the block, writes the merge-mining tag and
hands out the parent block's hashing blob, an ordinary CryptoNote blob with the
nonce at offset 39. The miner never sees a block.

## How it behaves

- **Share difficulty.** At the default `0` a miner is given the network
  difficulty and reports only blocks, which is what solo mining wants. A lower
  `--stratum-share-difficulty` makes it report progress too; shares below the
  network difficulty are counted and answered, and only a block is submitted.
- **Not ready, not mining.** A node still syncing turns logins away with
  "Node is still synchronizing (x of y)" rather than hand out templates the
  network left behind. The miner stays connected and retries.
- **Found blocks** go through the same path as `submitblock` and are announced
  to every peer the moment they are added (`NOTIFY_NEW_LITE_BLOCK`, or the full
  block to a peer too old for lite blocks). Each one is logged twice at `info`:
  once by the stratum server, with the miner and its address, and once as it
  goes out, with how many peers it reached:

    ```text
    stratum miner 10.0.0.7 (Wrkz…) found block at height 4216460, difficulty 30436438, hash 9c1f…
    block mined through this node at height 4216460, difficulty 30436438, hash 9c1f…: announced to 12 peer(s)
    ```

    A block a pool sends through `submitblock` gets the second line only: the
    node never learns which address mined it.

!!! warning "Exposure"

    The stratum server binds to loopback by default and has no
    authentication. Bound anywhere else it draws a warning at start-up: anyone
    who can reach it can make the node build templates and check shares.
    Firewall it to your rigs.

A pool keeps using `getblocktemplate` and `submitblock` on the RPC port, as
with `Wrkzd`; see [RPC interfaces](../rpc/index.md).
