// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! Structural comparison of two JSON documents, and the whitelist of fields
//! that legitimately differ between two daemons.
//!
//! This is what `wrkz-rpc-diff` reports and what the live shape test asserts
//! on. Two modes:
//!
//! - [`Mode::Shape`] compares **keys and types only**. Two nodes on different
//!   chains, at different heights, with different peers, cannot agree on a
//!   single value; they must agree on every field name and every field type.
//!   Array elements are compared element-wise where both are non-empty, and an
//!   array one side leaves empty is reported as `empty on one side` rather than
//!   as a difference.
//! - [`Mode::Value`] compares values as well, skipping the whitelist. Use it
//!   against the same chain — a port replaying the same database, or two runs
//!   of the same node.
//!
//! A path is `field.sub[0].leaf`, so a report names exactly where to look.

use crate::json::Json;

/// How strict a comparison is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Keys and types.
    Shape,
    /// Keys, types and values.
    Value,
}

/// One difference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Difference {
    /// The reference has this path and we do not.
    Missing { path: String, kind: &'static str },
    /// We have this path and the reference does not.
    Extra { path: String, kind: &'static str },
    /// Both have it, with different types.
    Type { path: String, reference: &'static str, ours: &'static str },
    /// Both have it with the same type, and different values ([`Mode::Value`]
    /// only, or a whitelist miss in [`Mode::Shape`] — never: shape mode does
    /// not compare values at all).
    Value { path: String, reference: String, ours: String },
    /// One side's array was empty, so its element shape could not be checked.
    EmptyOnOneSide { path: String, reference_len: usize, ours_len: usize },
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Difference::Missing { path, kind } => write!(f, "missing: {path} ({kind} in the reference)"),
            Difference::Extra { path, kind } => write!(f, "extra:   {path} ({kind}, not in the reference)"),
            Difference::Type { path, reference, ours } => {
                write!(f, "type:    {path}: reference {reference}, ours {ours}")
            }
            Difference::Value { path, reference, ours } => {
                write!(f, "value:   {path}: reference {reference}, ours {ours}")
            }
            Difference::EmptyOnOneSide { path, reference_len, ours_len } => {
                write!(f, "empty:   {path}: reference has {reference_len} elements, ours {ours_len}")
            }
        }
    }
}

/// Fields whose values are node-specific and cannot match between two daemons.
///
/// Matched on the **last path segment**, so `items[3].blockTimestamp` is
/// covered by `blockTimestamp`. Everything here is either a clock, an identity,
/// a peer count, or something that follows from the chain's own height —
/// spec/09's acceptance says the C++ comparison ignores exactly these.
pub const DEFAULT_WHITELIST: &[&str] = &[
    // clocks and uptime
    "start_time",
    "timestamp",
    "blockTimestamp",
    "last_seed_bootstrap",
    // identity and build
    "version",
    "compression",
    // peers and connections
    "incoming_connections_count",
    "outgoing_connections_count",
    "white_peerlist_size",
    "grey_peerlist_size",
    "seed_nodes_count",
    "peers",
    "peers_gray",
    "sync_active_peers",
    "sync_avg_batch_size",
    "sync_demoted_peers",
    // anything that follows from where the chain is
    "height",
    "network_height",
    "last_known_block_index",
    "top_block_hash",
    "difficulty",
    "hashrate",
    "tx_count",
    "tx_pool_size",
    "alt_blocks_count",
    "count",
    "depth",
    "synced",
    "scannedToHeight",
    "blockHeight",
    "blockHash",
    "hash",
    "prev_hash",
    "nonce",
    "reward",
    "block_size",
    "num_txes",
    "cumul_size",
    "currentHeight",
    "startHeight",
    "fullOffset",
    "isTailBlockActual",
    "blocktemplate_blob",
    "reserved_offset",
    "major_version",
    "minor_version",
];

/// The comparison itself.
#[derive(Clone, Debug)]
pub struct Comparison {
    pub mode: Mode,
    /// Last path segments whose values are not compared.
    pub whitelist: Vec<String>,
}

impl Default for Comparison {
    fn default() -> Self {
        Self { mode: Mode::Shape, whitelist: DEFAULT_WHITELIST.iter().map(|s| (*s).to_string()).collect() }
    }
}

impl Comparison {
    pub fn values() -> Self {
        Self { mode: Mode::Value, ..Default::default() }
    }

    fn whitelisted(&self, path: &str) -> bool {
        let leaf = path.rsplit('.').next().unwrap_or(path);
        // Strip any `[n]` suffix so `items[3]` matches `items`.
        let leaf = leaf.split('[').next().unwrap_or(leaf);
        self.whitelist.iter().any(|w| w == leaf)
    }

    /// Compare `ours` against `reference`, deepest-first, and return every
    /// difference found.
    pub fn compare(&self, reference: &Json, ours: &Json) -> Vec<Difference> {
        let mut out = Vec::new();
        self.walk("", reference, ours, &mut out);
        out
    }

    fn walk(&self, path: &str, reference: &Json, ours: &Json, out: &mut Vec<Difference>) {
        if reference.type_name() != ours.type_name() {
            out.push(Difference::Type {
                path: path_or_root(path),
                reference: reference.type_name(),
                ours: ours.type_name(),
            });
            return;
        }
        match (reference, ours) {
            (Json::Object(a), Json::Object(b)) => {
                for (key, value) in a {
                    let child = join(path, key);
                    match b.iter().find(|(k, _)| k == key) {
                        Some((_, ours)) => self.walk(&child, value, ours, out),
                        None => out.push(Difference::Missing { path: child, kind: value.type_name() }),
                    }
                }
                for (key, value) in b {
                    if !a.iter().any(|(k, _)| k == key) {
                        out.push(Difference::Extra { path: join(path, key), kind: value.type_name() });
                    }
                }
            }
            (Json::Array(a), Json::Array(b)) => {
                if a.is_empty() != b.is_empty() {
                    out.push(Difference::EmptyOnOneSide {
                        path: path_or_root(path),
                        reference_len: a.len(),
                        ours_len: b.len(),
                    });
                    return;
                }
                if self.mode == Mode::Value && a.len() != b.len() {
                    out.push(Difference::Value {
                        path: format!("{}.length", path_or_root(path)),
                        reference: a.len().to_string(),
                        ours: b.len().to_string(),
                    });
                }
                // In shape mode every element of an array has the same shape,
                // so comparing the overlap is enough — and comparing element 0
                // of a 1,000-block response against element 0 of a 2-block one
                // is the only comparison that means anything.
                for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                    self.walk(&format!("{}[{i}]", path_or_root(path)), x, y, out);
                }
            }
            _ => {
                if self.mode == Mode::Value && !self.whitelisted(path) && reference != ours {
                    out.push(Difference::Value {
                        path: path_or_root(path),
                        reference: reference.to_string(),
                        ours: ours.to_string(),
                    });
                }
            }
        }
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn path_or_root(path: &str) -> String {
    if path.is_empty() {
        "<root>".to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{parse, ParseLimits};

    fn p(s: &str) -> Json {
        parse(s.as_bytes(), ParseLimits::default()).unwrap()
    }

    #[test]
    fn identical_shapes_report_nothing() {
        let a = p(r#"{"height":1,"status":"OK","items":[{"k":1}]}"#);
        let b = p(r#"{"height":999,"status":"OK","items":[{"k":7}]}"#);
        assert!(Comparison::default().compare(&a, &b).is_empty());
    }

    #[test]
    fn a_missing_or_extra_key_is_reported_with_its_path() {
        let a = p(r#"{"a":{"b":1}}"#);
        let b = p(r#"{"a":{"c":1}}"#);
        let d = Comparison::default().compare(&a, &b);
        assert_eq!(
            d,
            vec![
                Difference::Missing { path: "a.b".into(), kind: "number" },
                Difference::Extra { path: "a.c".into(), kind: "number" },
            ]
        );
    }

    #[test]
    fn a_type_change_is_reported() {
        let a = p(r#"{"alreadyGeneratedCoins":"12"}"#);
        let b = p(r#"{"alreadyGeneratedCoins":12}"#);
        assert_eq!(
            Comparison::default().compare(&a, &b),
            vec![Difference::Type { path: "alreadyGeneratedCoins".into(), reference: "string", ours: "number" }]
        );
    }

    #[test]
    fn value_mode_honours_the_whitelist() {
        let a = p(r#"{"height":1,"status":"OK"}"#);
        let b = p(r#"{"height":2,"status":"Failed"}"#);
        let d = Comparison::values().compare(&a, &b);
        assert_eq!(
            d,
            vec![Difference::Value { path: "status".into(), reference: "\"OK\"".into(), ours: "\"Failed\"".into() }],
            "height is whitelisted, status is not"
        );
    }

    #[test]
    fn an_array_empty_on_one_side_is_called_out_not_silently_passed() {
        let a = p(r#"{"items":[{"x":1}]}"#);
        let b = p(r#"{"items":[]}"#);
        assert_eq!(
            Comparison::default().compare(&a, &b),
            vec![Difference::EmptyOnOneSide { path: "items".into(), reference_len: 1, ours_len: 0 }]
        );
        // Different non-empty lengths are fine in shape mode: only the overlap
        // is compared.
        let c = p(r#"{"items":[{"x":1},{"x":2}]}"#);
        assert!(Comparison::default().compare(&a, &c).is_empty());
    }

    #[test]
    fn whitelisting_matches_the_last_path_segment_through_arrays() {
        let c = Comparison::default();
        assert!(c.whitelisted("items[3].blockTimestamp"));
        assert!(c.whitelisted("block_header.height"));
        assert!(!c.whitelisted("block_header.orphan_status"));
        assert!(c.whitelisted("peers"));
    }
}
