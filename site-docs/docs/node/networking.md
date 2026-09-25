# Networking

This page covers how `wrkz-node` meets its peers beyond the defaults: listening
on IPv6, pinning priority and exclusive nodes, and asking the router to forward
the P2P port with UPnP. The P2P options themselves are listed with the rest in
[Configuration](configuration.md#command-line). The C++ counterpart is
[Networking](https://docs.wrkz.work/guides/networking/) on docs.wrkz.work.

By default the node listens for peers on `0.0.0.0:17855`, keeps up to 15
outbound and 15 inbound connections, and finds peers through the compiled-in
seed nodes and DNS seeds. The RPC listens on `127.0.0.1:17856`.

## IPv6

The option names are the C++ ones: `p2p-bind-ipv6-address`,
`p2p-bind-port-ipv6` ("TCP port for the IPv6 P2P listener (0 = same as
--p2p-bind-port)") and `rpc-bind-ipv6-address`.

- **IPv6 is off unless an address is given**, as in the C++. There is no IPv6
  by default and no way to get it by accident. For the RPC the C++ also wants
  `--rpc-use-ipv6`; here `--rpc-bind-ipv6-address` alone is enough, and
  `--rpc-use-ipv6` is accepted so a C++ command line starts.
- **Two listeners, one node.** Giving an address adds a *second* socket with its
  own accept loop. Everything behind it is shared: the connection table,
  `--in-peers`, the ban list, the white and gray peer lists and the peer state
  file. A node bound to both families is one node with two doors, not two
  nodes.
- **`--p2p-bind-port-ipv6 0` means `--p2p-bind-port`.** Leave it at `0` unless
  you have a reason: the handshake carries **one** `my_port`, the IPv4
  listener's, so a peer that reached you over IPv6 and back-pings that number
  only finds you if both families listen on it. The RPC has no separate port
  at all — the C++ IPv6 server listens on the RPC port, and so does this one.
- **The IPv6 socket is IPv6-only.** `IPV6_V6ONLY` is set to 1 before `bind(2)`,
  which is what the C++ does for its IPv6 RPC server. Without it, Linux
  (`net.ipv6.bindv6only = 0`) would give `::` a dual-stack socket that also owns
  the IPv4 wildcard on that port, so whichever of the two binds ran second would
  fail with `EADDRINUSE` — and every IPv4 peer would arrive on the IPv6 listener
  as `::ffff:a.b.c.d`. With it, the IPv4 listener owns IPv4, the IPv6 listener
  owns IPv6, and each accepted socket carries an address of the family it came
  in on.
- **Peer exchange is version-gated.** IPv6 peers are only sent to, and only
  accepted from, peers advertising at least `P2P_IPV6_CAPABILITY_VERSION` (19).
  Each list carries at most 250 entries, IPv4 and IPv6 counted separately, as
  the C++ does. `p2pstate.wrkz.bin` holds both families (peer-list serializer
  version 2) and is byte-compatible with the C++ file.
- **Loopback.** `::1` counts as loopback everywhere `127.0.0.1` does — the RPC
  rate-limit exemption and the "your RPC is exposed" warning both parse the
  address rather than comparing strings. As in the C++, loopback is never a
  valid *peer* address, with or without `--allow-local-ip`.

Both families at once, on the standard ports:

```sh
wrkz-node --data-dir ~/.wrkz-rust \
    --p2p-bind-ip 0.0.0.0 --p2p-bind-ipv6-address :: \
    --rpc-bind-ip 127.0.0.1 --rpc-bind-ipv6-address ::1
```

Start-up then logs `listening on 0.0.0.0:17855` and
`IPv6 P2P net service bound on [::]:17855`, the second line being the C++'s own
wording.

## Priority and exclusive nodes

```sh
wrkz-node --data-dir DIR --add-priority-node 203.0.113.7:17855
wrkz-node --data-dir DIR --add-exclusive-node 10.0.0.2 --add-exclusive-node 10.0.0.3
```

Both follow the C++'s connection maker, which runs once a second:

- **An exclusive node** is dialled every round it has no outbound connection
  and no dial on its way — an inbound connection from the same address does not
  count. When any exclusive node is configured, that is **all** the node dials:
  no seeds (they are not even looked up), no anchors from the last run, no
  `--add-peer` (still put on the white list, as the C++ puts it there), nothing
  from the peer lists. Inbound connections are still accepted within
  `--in-peers`, the peer lists an exclusive node sends are still merged, and
  ours is still sent.
- **A priority node** is dialled the same way, every round, before the peer
  lists fill the rest of `--out-peers`; the list selection never dials it a
  second time.
- Either is dialled whatever the outbound count says, and its connection counts
  toward `--out-peers` without ever being refused for it; with `--out-peers 0`
  the pinned nodes are the only outbound connections. A banned host is skipped;
  a failed dial is logged at `debug` only, and nothing gives up.

### Where this differs from the C++ {#priority-differences}

- The C++ takes `a.b.c.d:port` and nothing else. These take what `--add-peer`
  and `--seed-node` take: a hostname, an IPv6 literal (`[2001:db8::1]:17855`),
  or a bare host on port 17855. A name that resolves to several addresses is
  **one** node: it counts as connected when any of them is, and a failed dial
  tries the next.
- An entry that does not resolve stops the start with a message naming it, where
  a malformed entry makes the C++ fail with "Failed to initialize p2p server."
  Names are resolved once, at start.
- A priority node that keeps failing is backed off: one second after the first
  failure, doubling up to a minute, and back to every second once a dial
  succeeds. The C++ dials it every round for as long as it runs. An exclusive
  node is never backed off — it is all the node has.
- With exclusive nodes the seeds are never resolved, so `/info` reports a
  `seed_nodes_count` of 0 where the C++ reports its seed list.
- `--add-peer` does not overwrite a white-list entry that already exists for the
  same address, which would zero the `last_seen` a real handshake earned.

## UPnP port mapping

At start-up the daemon asks the router to forward its P2P port to it, as
`Wrkzd` does through miniupnpc: an SSDP search for an internet gateway device on
the LAN, a check that its WAN connection is up with a public address, then
`AddPortMapping` for TCP — the listening port outside and in — to this host's
LAN address, described `WRKZCoin`, with no lease limit. The log says how it
went, in the C++'s own words:

| Line | Meaning |
| --- | --- |
| `Attempting to add IGD port mapping.` | the search has started |
| `Added IGD port mapping.` | the port is forwarded |
| `UPNP_AddPortMapping failed.` | the router refused; the line after it says why (`718 ConflictInMappingEntry`: another host holds the port) |
| `IGD was found but its external address is reserved (double NAT).` | the router is itself behind a NAT: forward the port on the outer one by hand |
| `IGD was found but reported as not connected.` | the router says its WAN link is down |
| `UPnP device was found but not recognized as IGD.` | something answered, and it is not a gateway |
| `No IGD was found.` | nothing answered: no UPnP on this network, or it is switched off on the router |

### Where this differs from the C++ {#upnp-differences}

- **It does not hold up the start.** The C++ searches and maps synchronously at
  the end of its P2P initialisation, several seconds when no router answers;
  here it runs on a thread of its own.
- **It can be turned off, and is skipped where it cannot help.** `--no-upnp`
  (config key `no-upnp`) turns it off. It is not tried with `--no-listen`, with
  `--hide-my-port` (no peer is told the port) or for a listener on loopback, and
  a listener bound to one address is mapped only when that is the address that
  reaches the router. Each of these says so in the log.
- **The mapping is removed on a clean shutdown** (`Removed IGD port mapping.`),
  after asking the router what the port maps to now: a mapping it reports as
  another host's, or another port's, is left alone. The C++ never removes its
  mapping. A crash leaves it in place, as the C++ always does.
- No `minissdpd` step; every exchange with the router has a deadline and a size
  cap; an HTTP error from the router is a failure even without a SOAP fault.

As in the C++, `--p2p-external-port` does not change what is mapped, and the
IPv6 listener is not mapped at all.
