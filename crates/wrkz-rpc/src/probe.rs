// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The request set `wrkz-rpc-diff` puts to both daemons, and the runner that
//! does it.
//!
//! One entry per endpoint the C++ routes, with a body chosen so that both a
//! mainnet daemon at 4.2 million blocks and a synthetic chain of a few hundred
//! answer *something* — a shape comparison needs both sides to produce a body,
//! not the same body.

use crate::diff::{Comparison, Difference};
use crate::http::client::{self, BaseUrl};
use crate::json::{parse, Json, ParseLimits};
use std::time::Duration;

/// One request, put to both daemons unchanged.
#[derive(Clone, Debug)]
pub struct Probe {
    /// The name `--endpoints` selects it by.
    pub name: &'static str,
    pub method: &'static str,
    pub path: &'static str,
    /// `None` for a GET.
    pub body: Option<String>,
    /// Whether a non-200 from either side is expected (the explorer routes on a
    /// `Standard` daemon answer 403, which is the *correct* answer and worth
    /// comparing as such).
    pub expect_ok: bool,
}

/// The block index both daemons are asked about. 4,213,000 is the height every
/// vector in `spec/vectors` was captured at, so the live daemon has it.
pub const REFERENCE_HEIGHT: u64 = 4_213_000;

/// The full probe set.
///
/// A caller comparing against a chain that does not reach [`REFERENCE_HEIGHT`]
/// passes its own height to [`probes_at`].
pub fn probes() -> Vec<Probe> {
    probes_at(REFERENCE_HEIGHT, REFERENCE_HEIGHT)
}

/// The probe set with the heights a caller's own chain can answer.
///
/// `reference_height` is a block index both daemons hold; `range_start` is the
/// start of the two-block window the range endpoints ask about.
pub fn probes_at(reference_height: u64, range_start: u64) -> Vec<Probe> {
    let get = |name, path| Probe { name, method: "GET", path, body: None, expect_ok: true };
    let post = |name, path, body: String| Probe { name, method: "POST", path, body: Some(body), expect_ok: true };
    let explorer =
        |name, body: String| Probe { name, method: "POST", path: "/json_rpc", body: Some(body), expect_ok: false };
    let rpc = |name, method: &str, params: String| {
        post(name, "/json_rpc", format!(r#"{{"jsonrpc":"2.0","id":"diff","method":"{method}","params":{params}}}"#))
    };

    vec![
        get("info", "/info"),
        get("getinfo", "/getinfo"),
        get("height", "/height"),
        get("getheight", "/getheight"),
        get("peers", "/peers"),
        rpc("getblockcount", "getblockcount", "{}".into()),
        rpc("getlastblockheader", "getlastblockheader", "{}".into()),
        rpc("getblockheaderbyheight", "getblockheaderbyheight", format!(r#"{{"height":{reference_height}}}"#)),
        post(
            "getwalletsyncdata",
            "/getwalletsyncdata",
            format!(
                r#"{{"blockHashCheckpoints":[],"startHeight":{range_start},"startTimestamp":0,"blockCount":2,"skipCoinbaseTransactions":false}}"#
            ),
        ),
        post(
            "getrawblocks",
            "/getrawblocks",
            format!(
                r#"{{"blockHashCheckpoints":[],"startHeight":{range_start},"startTimestamp":0,"blockCount":1,"skipCoinbaseTransactions":false}}"#
            ),
        ),
        post(
            "get_global_indexes_for_range",
            "/get_global_indexes_for_range",
            format!(r#"{{"startHeight":{range_start},"endHeight":{}}}"#, range_start + 1),
        ),
        post("getrandom_outs", "/getrandom_outs", r#"{"amounts":[10000],"outs_count":3}"#.into()),
        post(
            "get_transactions_status",
            "/get_transactions_status",
            r#"{"transactionHashes":["0000000000000000000000000000000000000000000000000000000000000001"]}"#.into(),
        ),
        post("sendrawtransaction_bad_hex", "/sendrawtransaction", r#"{"tx_as_hex":"zz"}"#.into()),
        post(
            "get_pool_changes_lite",
            "/get_pool_changes_lite",
            r#"{"tailBlockId":"0000000000000000000000000000000000000000000000000000000000000001","knownTxsIds":[]}"#
                .into(),
        ),
        // Error paths: both daemons must refuse in the same shape.
        post("bad_range", "/get_global_indexes_for_range", r#"{"startHeight":10,"endHeight":1}"#.into()),
        post("missing_param", "/get_global_indexes_for_range", "{}".into()),
        Probe { name: "bad_json", method: "POST", path: "/json_rpc", body: Some("not json".into()), expect_ok: false },
        Probe { name: "missing_method", method: "POST", path: "/json_rpc", body: Some("{}".into()), expect_ok: false },
        Probe {
            name: "unknown_method",
            method: "POST",
            path: "/json_rpc",
            body: Some(r#"{"jsonrpc":"2.0","id":1,"method":"on_getblockhash","params":[1]}"#.into()),
            expect_ok: false,
        },
        Probe { name: "unknown_path", method: "GET", path: "/fee", body: None, expect_ok: false },
        explorer(
            "f_block_json",
            format!(r#"{{"jsonrpc":"2.0","id":1,"method":"f_block_json","params":{{"hash":"{reference_height}"}}}}"#),
        ),
        explorer(
            r#"f_on_transactions_pool_json"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"f_on_transactions_pool_json","params":{}}"#.into(),
        ),
    ]
}

/// The highest block **index** both daemons hold, so the range probes ask about
/// something each of them can answer.
///
/// Two nodes at different heights are the normal case — a port catching up
/// against a synced C++ daemon — and asking both about a height only one has is
/// a content difference dressed up as a shape difference. This reads `/height`
/// from each and takes the lower top index.
pub fn common_top_index(a: &BaseUrl, b: &BaseUrl, timeout: Duration) -> Result<u64, String> {
    let top_of = |base: &BaseUrl| -> Result<u64, String> {
        let (status, body) = client::request(base, "GET", "/height", None, timeout, 64 * 1024)?;
        if status != 200 {
            return Err(format!("/height answered {status}"));
        }
        let value = parse(&body, ParseLimits::default()).map_err(|e| e.to_string())?;
        // `height` is a count; the top index is one less.
        value
            .get("height")
            .and_then(Json::as_u64)
            .map(|h| h.saturating_sub(1))
            .ok_or_else(|| "no height in /height".to_string())
    };
    Ok(std::cmp::min(top_of(a)?, top_of(b)?))
}

/// What one probe produced on both sides.
#[derive(Clone, Debug)]
pub struct ProbeResult {
    pub name: &'static str,
    pub reference_status: u16,
    pub ours_status: u16,
    pub differences: Vec<Difference>,
    /// A transport or parse fault, which is not a difference but a failure to
    /// compare at all.
    pub error: Option<String>,
}

impl ProbeResult {
    pub fn is_clean(&self) -> bool {
        self.error.is_none() && self.differences.is_empty() && self.reference_status == self.ours_status
    }
}

/// Largest body either daemon may return before the client gives up.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Run one probe against both daemons and diff the answers.
pub fn run_one(reference: &BaseUrl, ours: &BaseUrl, probe: &Probe, cmp: &Comparison, timeout: Duration) -> ProbeResult {
    let mut result =
        ProbeResult { name: probe.name, reference_status: 0, ours_status: 0, differences: Vec::new(), error: None };
    let fetch = |base: &BaseUrl| -> std::result::Result<(u16, Json), String> {
        let (status, body) =
            client::request(base, probe.method, probe.path, probe.body.as_deref(), timeout, MAX_RESPONSE_BYTES)?;
        if body.is_empty() {
            return Ok((status, Json::Null));
        }
        let value = parse(&body, ParseLimits { max_bytes: MAX_RESPONSE_BYTES, max_depth: 256 })
            .map_err(|e| format!("body is not JSON: {e}"))?;
        Ok((status, value))
    };
    let (reference_status, reference_body) = match fetch(reference) {
        Ok(v) => v,
        Err(e) => {
            result.error = Some(format!("reference: {e}"));
            return result;
        }
    };
    let (ours_status, ours_body) = match fetch(ours) {
        Ok(v) => v,
        Err(e) => {
            result.error = Some(format!("ours: {e}"));
            return result;
        }
    };
    result.reference_status = reference_status;
    result.ours_status = ours_status;
    result.differences = cmp.compare(&reference_body, &ours_body);
    result
}

/// Run a whole set.
pub fn run(
    reference: &BaseUrl,
    ours: &BaseUrl,
    probes: &[Probe],
    cmp: &Comparison,
    timeout: Duration,
) -> Vec<ProbeResult> {
    probes.iter().map(|p| run_one(reference, ours, p, cmp, timeout)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_probe_has_a_unique_name_and_a_body_where_it_needs_one() {
        let set = probes();
        let mut names: Vec<&str> = set.iter().map(|p| p.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "probe names are unique");
        for p in &set {
            assert!(p.path.starts_with('/'));
            assert_eq!(p.method == "POST", p.body.is_some(), "{} :{} needs a body", p.method, p.path);
        }
        assert!(set.iter().any(|p| p.name == "unknown_path"));
    }

    #[test]
    fn the_probe_set_covers_every_routed_plain_endpoint() {
        let set = probes();
        for path in [
            "/info",
            "/getinfo",
            "/height",
            "/getheight",
            "/peers",
            "/json_rpc",
            "/sendrawtransaction",
            "/getrandom_outs",
            "/getwalletsyncdata",
            "/get_global_indexes_for_range",
            "/get_transactions_status",
            "/get_pool_changes_lite",
            "/getrawblocks",
        ] {
            assert!(set.iter().any(|p| p.path == path), "{path} is probed");
        }
    }
}
