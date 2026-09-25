# systemd

This page gives a systemd unit that runs `wrkz-node` unattended as its own
user, with the hardening the daemon allows: it needs its data directory and
outbound TCP, nothing else.

The C++ guide to locking a node down is
[Security Hardening](https://docs.wrkz.work/guides/security-hardening/) on docs.wrkz.work.

## The unit

```ini
# /etc/systemd/system/wrkz-node.service
[Unit]
Description=WrkzCoin node (Rust)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=wrkz
Group=wrkz
WorkingDirectory=/opt/wrkz-rust
ExecStart=/opt/wrkz-rust/target/release/wrkz-node \
    --data-dir /var/lib/wrkz-rust \
    --p2p-bind-port 17855 \
    --rpc-bind-ip 127.0.0.1 \
    --rpc-bind-port 17856 \
    --log-level info
# The daemon writes the peer file and flushes the chain state on SIGTERM.
KillSignal=SIGTERM
TimeoutStopSec=120
Restart=on-failure
RestartSec=10
# Journald already timestamps every line; the daemon logs to stderr.
StandardOutput=journal
StandardError=journal

# Hardening. The daemon needs its data directory and outbound TCP, nothing else.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/wrkz-rust
ProtectKernelTunables=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
```

## Installing it

```sh
sudo useradd --system --home /var/lib/wrkz-rust --create-home wrkz
sudo chown -R wrkz:wrkz /var/lib/wrkz-rust
sudo systemctl daemon-reload
sudo systemctl enable --now wrkz-node
journalctl -u wrkz-node -f
```

Under systemd stdin is not a terminal, so the daemon starts no console of its
own; the periodic status line still goes to the journal. To type commands at
it, give it `--rpc-ipc-path` and use `wrkz-node attach`; see
[Attaching to a running daemon](console-and-ipc.md#attaching-to-a-running-daemon).
The unit's `RestrictAddressFamilies` then needs `AF_UNIX` as well, and the
socket's directory must be writable by the service (for example
`RuntimeDirectory=wrkz` for `/run/wrkz`).

!!! warning "Exposing the RPC"

    If the RPC must be reachable from other machines, bind it to `0.0.0.0`
    **and** set `--rpc-access-token`, or put it behind a firewall. Anyone who
    can reach the RPC can read the chain and submit transactions and blocks.
