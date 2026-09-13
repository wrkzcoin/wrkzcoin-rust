// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `GET /metrics`, off unless `--enable-metrics`: the node's state and this
//! server's own counters in the Prometheus text format (0.0.4), so a
//! production node can be watched and alerted on.
//!
//! Not a route of the C++ daemon, which is why it does not exist — it is the
//! 404 of any unrouted path — unless it was asked for. It goes through the
//! same middleware as every route, the access token and the rate limit
//! included, so a scraper sends the token as `Authorization: Bearer`, which
//! Prometheus supports natively.

use crate::api::NodeApi;
use crate::http::Response;
use crate::json::Obj;
use crate::sync_cache::SyncCache;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// What this server has answered since it started.
#[derive(Debug, Default)]
pub struct Counters {
    requests: AtomicU64,
    ok: AtomicU64,
    client_errors: AtomicU64,
    server_errors: AtomicU64,
    gzipped: AtomicU64,
}

impl Counters {
    /// One request answered with `status`, gzipped or not.
    pub fn record(&self, status: u16, gzipped: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let class = match status {
            0..=399 => &self.ok,
            400..=499 => &self.client_errors,
            _ => &self.server_errors,
        };
        class.fetch_add(1, Ordering::Relaxed);
        if gzipped {
            self.gzipped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One metric family: `# HELP`, `# TYPE`, and its samples, each with its
/// label set (empty for none).
fn family(out: &mut String, name: &str, kind: &str, help: &str, samples: &[(&str, u64)]) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    for (labels, value) in samples {
        if labels.is_empty() {
            let _ = writeln!(out, "{name} {value}");
        } else {
            let _ = writeln!(out, "{name}{{{labels}}} {value}");
        }
    }
}

/// `GET /health`, off unless `--enable-health`: **200** once the node is
/// synchronized with its peers and **503** before that (or while it cannot
/// read its state), with a small JSON body either way — what a load balancer,
/// a container `HEALTHCHECK` or an uptime monitor asks. Like `/metrics` it is
/// not a C++ route and goes through the same token and rate-limit gate.
pub fn health(api: &dyn NodeApi) -> Response {
    let mut o = Obj::new();
    let status = match api.info() {
        Ok(i) => {
            let synced = api.is_synced();
            o.set("status", if synced { "OK" } else { "SYNCING" })
                .set("synced", synced)
                .set("height", i.height)
                .set("network_height", i.network_height)
                .set("peers", i.incoming_connections_count + i.outgoing_connections_count);
            if synced {
                200
            } else {
                503
            }
        }
        Err(_) => {
            o.set("status", "BUSY").set("synced", false);
            503
        }
    };
    Response::json(status, o.build().to_string())
}

/// The page.
pub fn render(api: &dyn NodeApi, counters: &Counters, cache: &SyncCache) -> Response {
    let mut out = String::with_capacity(4096);
    let gauge = |out: &mut String, name: &str, help: &str, value: u64| family(out, name, "gauge", help, &[("", value)]);
    match api.info() {
        Ok(i) => {
            gauge(
                &mut out,
                "wrkz_info_available",
                "Whether the node answered for its state (0 while it reorganises)",
                1,
            );
            gauge(&mut out, "wrkz_height", "Blocks in the main chain: the top index plus one", i.height);
            gauge(&mut out, "wrkz_network_height", "The tallest chain the node's peers report", i.network_height);
            gauge(
                &mut out,
                "wrkz_blocks_behind",
                "How far the node is below its peers",
                i.network_height.saturating_sub(i.height),
            );
            gauge(&mut out, "wrkz_synchronized", "1 once synced with the network", u64::from(api.is_synced()));
            gauge(&mut out, "wrkz_difficulty", "The difficulty of the next block", i.difficulty);
            gauge(&mut out, "wrkz_transactions", "Transactions in the main chain, coinbases excluded", i.tx_count);
            gauge(&mut out, "wrkz_pool_transactions", "Transactions waiting in the pool", i.tx_pool_size);
            gauge(&mut out, "wrkz_alternative_blocks", "Blocks held on alternative chains", i.alt_blocks_count);
            family(
                &mut out,
                "wrkz_connections",
                "gauge",
                "Open peer connections",
                &[
                    ("direction=\"incoming\"", i.incoming_connections_count),
                    ("direction=\"outgoing\"", i.outgoing_connections_count),
                ],
            );
            family(
                &mut out,
                "wrkz_peerlist_size",
                "gauge",
                "Known peer addresses",
                &[("list=\"white\"", i.white_peerlist_size), ("list=\"grey\"", i.grey_peerlist_size)],
            );
            gauge(&mut out, "wrkz_sync_active_peers", "Connections pulling the chain", i.sync_active_peers);
            gauge(&mut out, "wrkz_block_major_version", "The top block's major version", u64::from(i.major_version));
            gauge(&mut out, "wrkz_start_time_seconds", "When the daemon started, Unix time", i.start_time);
        }
        Err(_) => gauge(
            &mut out,
            "wrkz_info_available",
            "Whether the node answered for its state (0 while it reorganises)",
            0,
        ),
    }
    let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
    family(&mut out, "wrkz_rpc_requests_total", "counter", "RPC requests answered", &[("", load(&counters.requests))]);
    family(
        &mut out,
        "wrkz_rpc_responses_total",
        "counter",
        "RPC responses by status class",
        &[
            ("class=\"ok\"", load(&counters.ok)),
            ("class=\"client_error\"", load(&counters.client_errors)),
            ("class=\"server_error\"", load(&counters.server_errors)),
        ],
    );
    family(
        &mut out,
        "wrkz_rpc_gzip_responses_total",
        "counter",
        "RPC responses sent gzipped",
        &[("", load(&counters.gzipped))],
    );
    gauge(&mut out, "wrkz_rpc_sync_cache_bytes", "Bytes of wallet sync answers cached", cache.bytes() as u64);
    gauge(&mut out, "wrkz_rpc_sync_cache_entries", "Wallet sync answers cached", cache.len() as u64);
    family(
        &mut out,
        "wrkz_rpc_sync_cache_hits_total",
        "counter",
        "Wallet sync answers served from the cache",
        &[("", cache.hits())],
    );
    family(
        &mut out,
        "wrkz_rpc_sync_cache_misses_total",
        "counter",
        "Cacheable wallet sync requests that had to be built",
        &[("", cache.misses())],
    );

    let mut res = Response::new(200);
    res.set_header("Content-Type", "text/plain; version=0.0.4; charset=utf-8");
    res.body = out.into_bytes();
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_are_well_formed() {
        let mut out = String::new();
        family(&mut out, "wrkz_x", "gauge", "An x", &[("", 3)]);
        family(&mut out, "wrkz_y", "counter", "A y", &[("a=\"1\"", 4), ("a=\"2\"", 5)]);
        assert_eq!(
            out,
            "# HELP wrkz_x An x\n# TYPE wrkz_x gauge\nwrkz_x 3\n\
             # HELP wrkz_y A y\n# TYPE wrkz_y counter\nwrkz_y{a=\"1\"} 4\nwrkz_y{a=\"2\"} 5\n"
        );
    }

    #[test]
    fn responses_are_counted_by_class() {
        let c = Counters::default();
        c.record(200, true);
        c.record(404, false);
        c.record(429, false);
        c.record(503, false);
        assert_eq!(c.requests.load(Ordering::Relaxed), 4);
        assert_eq!(c.ok.load(Ordering::Relaxed), 1);
        assert_eq!(c.client_errors.load(Ordering::Relaxed), 2);
        assert_eq!(c.server_errors.load(Ordering::Relaxed), 1);
        assert_eq!(c.gzipped.load(Ordering::Relaxed), 1);
    }
}
