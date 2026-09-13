// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `--config-file`: the C++ daemon's configuration file
//! (`DaemonConfiguration.cpp:1598`, `handleSettings(configFile, …)`), in its
//! JSON form and in the older `key=value` form the C++ still reads and upgrades
//! (`updateConfigFormat`, `:700`).
//!
//! The file is turned into command-line arguments that go **in front of** the
//! real ones. Every value therefore passes through the same parser and the same
//! checks as the flag it stands for, and a flag given on the command line wins
//! over the file — the C++'s order: command line, file, command line again
//! (`Daemon.cpp:394-461`). A list (`add-peer`, `seed-node`,
//! `add-exclusive-node`, `add-priority-node`) adds to what the command line
//! gives.
//!
//! The keys are the C++'s, so a `Wrkzd` configuration file runs this daemon
//! unchanged: every key `Wrkzd --dump-config` writes is read. A key neither
//! daemon knows is reported: nlohmann ignores it silently, and a misspelt
//! `rpc-bind-ip` is worth a line in the log.

use std::path::Path;
use wrkz_rpc::json::{self, Json, ParseLimits};

/// What a configuration file came to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileArgs {
    /// Command-line arguments, to be parsed before the real ones.
    pub args: Vec<String>,
    /// A line each for what was set in the file and will not be honoured.
    pub notes: Vec<String>,
}

/// How one key of the file becomes arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    /// A value flag, `--name VALUE`. An empty string is the C++'s "unset" and
    /// emits nothing.
    Value(&'static str),
    /// A value flag that may be negative, which [`Key::Value`] refuses.
    Signed(&'static str),
    /// A presence flag, `--name`, emitted when the value is true.
    Flag(&'static str),
    /// A boolean that defaults to on: `--name` when true, `--name=false` when
    /// false, which is how the C++ command line turns one off.
    Switch(&'static str),
    /// A repeatable value flag, one per array element.
    List(&'static str),
    /// Handled by name in [`to_args`].
    Special,
    /// Read and dropped without a word, as the C++ drops it
    /// (`fee-address`, `fee-amount`: "Deprecated: accepted for backward
    /// compatibility, ignored", `:1812`).
    Ignored,
    /// A one-off action both daemons take on the command line only: the C++
    /// `handleSettings` for a file never reads it (`DaemonConfiguration.cpp:1598`).
    /// A file is read at every start, which is no place for `--resync`, so a
    /// set one is ignored with a note saying why.
    CommandLineOnly,
}

/// Every key `handleSettings` reads (`:1615-2005`) and `asJSON` writes
/// (`:2014-2085`), plus this port's own. The flag each maps to is the C++
/// command-line spelling, which is also this daemon's.
const KEYS: &[(&str, Key)] = &[
    ("data-dir", Key::Value("--data-dir")),
    ("load-checkpoints", Key::Special),
    ("log-file", Key::Value("--log-file")),
    ("log-level", Key::Value("--log-level")),
    ("log-format", Key::Value("--log-format")),
    ("no-console", Key::Flag("--no-console")),
    ("skip-boot-compaction", Key::Flag("--skip-boot-compaction")),
    ("db-enable-compression", Key::Switch("--db-enable-compression")),
    ("db-compression-dict-bytes", Key::Value("--db-compression-dict-bytes")),
    ("db-compression-level", Key::Signed("--db-compression-level")),
    ("db-row-cache-percent", Key::Value("--db-row-cache-percent")),
    ("db-bottom-filters", Key::Flag("--db-bottom-filters")),
    ("db-block-size", Key::Value("--db-block-size")),
    ("db-max-open-files", Key::Signed("--db-max-open-files")),
    ("db-read-buffer-size", Key::Value("--db-read-buffer-size")),
    ("db-threads", Key::Value("--db-threads")),
    ("db-write-buffer-size", Key::Value("--db-write-buffer-size")),
    ("allow-local-ip", Key::Flag("--allow-local-ip")),
    ("hide-my-port", Key::Flag("--hide-my-port")),
    ("p2p-bind-ip", Key::Value("--p2p-bind-ip")),
    ("p2p-bind-port", Key::Value("--p2p-bind-port")),
    ("p2p-external-port", Key::Value("--p2p-external-port")),
    ("out-peers", Key::Value("--out-peers")),
    ("in-peers", Key::Value("--in-peers")),
    ("p2p-reset-peerstate", Key::Flag("--p2p-reset-peerstate")),
    ("p2p-bind-ipv6-address", Key::Value("--p2p-bind-ipv6-address")),
    ("p2p-bind-port-ipv6", Key::Value("--p2p-bind-port-ipv6")),
    ("rpc-bind-ipv6-address", Key::Value("--rpc-bind-ipv6-address")),
    ("rpc-use-ipv6", Key::Flag("--rpc-use-ipv6")),
    ("rpc-bind-ip", Key::Value("--rpc-bind-ip")),
    ("rpc-bind-port", Key::Value("--rpc-bind-port")),
    ("add-exclusive-node", Key::List("--add-exclusive-node")),
    ("add-peer", Key::List("--add-peer")),
    ("add-priority-node", Key::List("--add-priority-node")),
    ("seed-node", Key::List("--seed-node")),
    ("daemon-mode", Key::Special),
    ("enable-cors", Key::Value("--enable-cors")),
    ("fee-address", Key::Ignored),
    ("fee-amount", Key::Ignored),
    ("rpc-access-token", Key::Value("--rpc-access-token")),
    ("rpc-read-timeout", Key::Value("--rpc-read-timeout")),
    ("rpc-write-timeout", Key::Value("--rpc-write-timeout")),
    ("rpc-max-body-bytes", Key::Value("--rpc-max-body-bytes")),
    ("rpc-max-rpm", Key::Value("--rpc-max-rpm")),
    ("rpc-max-global-index-range", Key::Value("--rpc-max-global-index-range")),
    ("rpc-sync-cache-size", Key::Value("--rpc-sync-cache-size")),
    ("rpc-stream-threshold", Key::Value("--rpc-stream-threshold")),
    ("rpc-max-block-count", Key::Value("--rpc-max-block-count")),
    ("rpc-trust-proxy", Key::Flag("--rpc-trust-proxy")),
    ("rpc-ipc-path", Key::Value("--rpc-ipc-path")),
    ("rpc-ipc-mode", Key::Value("--rpc-ipc-mode")),
    ("rpc-ipc-group", Key::Value("--rpc-ipc-group")),
    ("rpc-ipc-require-token", Key::Flag("--rpc-ipc-require-token")),
    ("transaction-validation-threads", Key::Special),
    ("sync-max-peers", Key::Value("--sync-max-peers")),
    ("sync-peer-failure-threshold", Key::Value("--sync-peer-failure-threshold")),
    ("sync-batch-min", Key::Value("--sync-batch-min")),
    ("sync-batch-max", Key::Value("--sync-batch-max")),
    ("block-sync-size", Key::Value("--block-sync-size")),
    ("block-sync-bytes", Key::Value("--block-sync-bytes")),
    ("auto-prune-min-gap-blocks", Key::Value("--auto-prune-min-gap-blocks")),
    ("auto-compaction-min-gap-blocks", Key::Value("--auto-compaction-min-gap-blocks")),
    ("auto-prune-min-free-bytes", Key::Value("--auto-prune-min-free-bytes")),
    ("auto-compaction-min-free-bytes", Key::Value("--auto-compaction-min-free-bytes")),
    ("prune", Key::Flag("--prune")),
    ("prune-depth", Key::Special),
    ("stratum-bind-ip", Key::Value("--stratum-bind-ip")),
    ("stratum-bind-port", Key::Value("--stratum-bind-port")),
    ("stratum-share-difficulty", Key::Value("--stratum-share-difficulty")),
    ("stratum-max-connections", Key::Value("--stratum-max-connections")),
    ("zmq-pub", Key::Special),
    ("no-zmq", Key::Flag("--no-zmq")),
    ("block-notify", Key::Value("--block-notify")),
    ("reorg-notify", Key::Value("--reorg-notify")),
    ("tx-notify", Key::Value("--tx-notify")),
    ("notify-during-sync", Key::Flag("--notify-during-sync")),
    // The one-off actions: command line only, in both daemons.
    ("resync", Key::CommandLineOnly),
    ("rewind-to-height", Key::CommandLineOnly),
    ("import-blockchain", Key::CommandLineOnly),
    ("export-blockchain", Key::CommandLineOnly),
    ("dump-file", Key::CommandLineOnly),
    ("max-export-blocks", Key::CommandLineOnly),
    ("import-validate", Key::CommandLineOnly),
    // This port's own; `--dump-config` writes them, so a dump reads back.
    ("lite", Key::Flag("--lite")),
    ("lite-height", Key::Value("--lite-height")),
    ("no-listen", Key::Flag("--no-listen")),
    ("no-upnp", Key::Flag("--no-upnp")),
    ("no-default-seeds", Key::Flag("--no-default-seeds")),
    ("no-rpc", Key::Flag("--no-rpc")),
    ("rpc-workers", Key::Value("--rpc-workers")),
    ("rpc-max-connections-per-ip", Key::Value("--rpc-max-connections-per-ip")),
    ("enable-metrics", Key::Flag("--enable-metrics")),
    ("enable-health", Key::Flag("--enable-health")),
    ("decoy-selection", Key::Value("--decoy-selection")),
    ("threads", Key::Value("--threads")),
    ("batch-blocks", Key::Value("--batch-blocks")),
    ("batch-bytes", Key::Value("--batch-bytes")),
    ("wal", Key::Flag("--wal")),
];

/// Read a configuration file.
pub fn load(path: &Path) -> Result<FileArgs, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "--config-file {}: {e}. The --config-file you specified does not exist, please check the filename \
             and try again.",
            path.display()
        )
    })?;
    from_text(&text).map_err(|e| format!("--config-file {}: {e}", path.display()))
}

/// The arguments a configuration file's text stands for. JSON when it opens
/// with `{`, the `key=value` form otherwise.
pub fn from_text(text: &str) -> Result<FileArgs, String> {
    let entries = if text.trim_start().starts_with('{') { parse_json(text)? } else { parse_key_values(text)? };
    to_args(&entries)
}

fn parse_json(text: &str) -> Result<Vec<(String, Json)>, String> {
    let limits = ParseLimits { max_bytes: 1 << 20, max_depth: 8 };
    match json::parse(text.as_bytes(), limits) {
        Ok(Json::Object(entries)) => Ok(entries),
        Ok(other) => Err(format!("the configuration must be a JSON object, not {}", other.type_name())),
        Err(e) => Err(format!("Failed to parse the config file as JSON: {e}")),
    }
}

/// The form `updateConfigFormat` upgrades from: one `key=value` per line, `#`
/// comments, a list key repeated once per element. Every value is text, which
/// is what a flag takes anyway.
fn parse_key_values(text: &str) -> Result<Vec<(String, Json)>, String> {
    let mut entries: Vec<(String, Json)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| format!("line {}: expected key=value", n + 1))?;
        let (key, value) = (key.trim(), value.trim().trim_matches('"'));
        if matches!(lookup(key), Some(Key::List(_))) {
            match entries.iter_mut().find(|(k, _)| k == key) {
                Some((_, Json::Array(items))) => items.push(Json::Str(value.to_string())),
                _ => entries.push((key.to_string(), Json::Array(vec![Json::Str(value.to_string())]))),
            }
        } else {
            entries.push((key.to_string(), Json::Str(value.to_string())));
        }
    }
    Ok(entries)
}

fn lookup(key: &str) -> Option<Key> {
    KEYS.iter().find(|(k, _)| *k == key).map(|(_, kind)| *kind)
}

/// A scalar as a flag's value; `None` for the C++'s "unset" — an empty string
/// or null.
fn scalar(key: &str, value: &Json) -> Result<Option<String>, String> {
    match value {
        Json::Str(s) if s.is_empty() => Ok(None),
        Json::Str(s) => Ok(Some(s.clone())),
        Json::U64(n) => Ok(Some(n.to_string())),
        Json::Null => Ok(None),
        other => Err(format!("{key}: expected a string or a non-negative integer, not {}", other.type_name())),
    }
}

fn boolean(key: &str, value: &Json) -> Result<bool, String> {
    match value {
        Json::Bool(b) => Ok(*b),
        Json::U64(0) => Ok(false),
        Json::U64(1) => Ok(true),
        Json::Str(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" | "" => Ok(false),
            _ => Err(format!("{key}: expected true or false, not {s:?}")),
        },
        Json::Null => Ok(false),
        other => Err(format!("{key}: expected true or false, not {}", other.type_name())),
    }
}

fn list(key: &str, value: &Json) -> Result<Vec<String>, String> {
    match value {
        Json::Array(items) => items.iter().filter_map(|v| scalar(key, v).transpose()).collect(),
        Json::Null => Ok(Vec::new()),
        other => Err(format!("{key}: expected an array, not {}", other.type_name())),
    }
}

/// Whether a value of a command-line-only key says anything: a default a
/// dump would carry — an empty string, `false`, 0, an empty list — does not.
fn is_set(value: &Json) -> bool {
    match value {
        Json::Null | Json::Bool(false) | Json::U64(0) => false,
        Json::Str(s) => !s.is_empty(),
        Json::Array(items) => !items.is_empty(),
        _ => true,
    }
}

fn to_args(entries: &[(String, Json)]) -> Result<FileArgs, String> {
    let mut out = FileArgs::default();
    let prune = match entries.iter().rev().find(|(k, _)| k == "prune") {
        Some((k, v)) => boolean(k, v)?,
        None => false,
    };
    for (key, value) in entries {
        let push = |out: &mut FileArgs, flag: &str, v: String| {
            out.args.push(flag.to_string());
            out.args.push(v);
        };
        match lookup(key) {
            Some(Key::Value(flag)) => {
                if let Some(v) = scalar(key, value)? {
                    push(&mut out, flag, v);
                }
            }
            Some(Key::Signed(flag)) => {
                let v = match value {
                    Json::I64(n) => Some(n.to_string()),
                    other => scalar(key, other)?,
                };
                if let Some(v) = v {
                    push(&mut out, flag, v);
                }
            }
            Some(Key::Flag(flag)) => {
                if boolean(key, value)? {
                    out.args.push(flag.to_string());
                }
            }
            Some(Key::Switch(flag)) => match value {
                Json::Null => {}
                Json::Str(s) if s.is_empty() => {}
                _ if boolean(key, value)? => out.args.push(flag.to_string()),
                _ => out.args.push(format!("{flag}=false")),
            },
            Some(Key::List(flag)) => {
                for v in list(key, value)? {
                    push(&mut out, flag, v);
                }
            }
            Some(Key::Special) => match key.as_str() {
                // "default" is the compiled-in table, which is also what no
                // flag at all means — so it is left out, and `--no-checkpoints`
                // on the command line still works over a C++ dump. An empty
                // string is the C++'s "none" (`use_checkpoints =
                // !config.checkPoints.empty()`, `Daemon.cpp:635`).
                "load-checkpoints" => match value {
                    Json::Str(s) if s.is_empty() => out.args.push("--no-checkpoints".to_string()),
                    _ => match scalar(key, value)?.as_deref() {
                        None | Some("default") => {}
                        Some(v) => push(&mut out, "--load-checkpoints", v.to_string()),
                    },
                },
                "daemon-mode" => match scalar(key, value)?.as_deref() {
                    None | Some("standard") => {}
                    Some("explorer") => push(&mut out, "--daemon-mode", "explorer".to_string()),
                    Some(other) => return Err(format!("daemon-mode: expected standard or explorer, not {other:?}")),
                },
                // The C++ default is the core count; 0 would mean the same
                // here, and `--threads` refuses 0, so it is left to the default.
                "transaction-validation-threads" => match scalar(key, value)?.as_deref() {
                    None | Some("0") => {}
                    Some(v) => push(&mut out, "--threads", v.to_string()),
                },
                // A dump always carries `prune-depth`, pruning or not; a depth
                // only means something with `prune` on, and this daemon refuses
                // the one without the other.
                "prune-depth" => {
                    if prune {
                        if let Some(v) = scalar(key, value)? {
                            push(&mut out, "--prune-depth", v);
                        }
                    }
                }
                // An empty address is the C++'s "off" (`Daemon.cpp:1094`); left
                // out, it would be the default address, which is on.
                "zmq-pub" => match value {
                    Json::Str(s) if s.is_empty() => push(&mut out, "--zmq-pub", String::new()),
                    _ => {
                        if let Some(v) = scalar(key, value)? {
                            push(&mut out, "--zmq-pub", v);
                        }
                    }
                },
                _ => unreachable!("every Special key is handled above"),
            },
            Some(Key::Ignored) => {}
            Some(Key::CommandLineOnly) => {
                if is_set(value) {
                    out.notes
                        .push(format!("{key}: a one-off action taken on the command line only, as in Wrkzd; ignored"));
                }
            }
            None => out.notes.push(format!("{key}: not a setting of this daemon or of Wrkzd, ignored")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        from_text(text).unwrap().args
    }

    #[test]
    fn a_cpp_dump_becomes_the_flags_it_stands_for() {
        let dump = r#"{
          "data-dir": "/var/lib/wrkz", "load-checkpoints": "default", "log-file": "", "log-level": 2,
          "no-console": true, "db-threads": 8, "allow-local-ip": false, "p2p-bind-ip": "0.0.0.0",
          "p2p-bind-port": 17855, "out-peers": 8, "p2p-bind-ipv6-address": "", "rpc-bind-ip": "127.0.0.1",
          "rpc-bind-port": 17856, "add-peer": ["1.2.3.4:17855", "5.6.7.8:17855"], "add-exclusive-node": [],
          "seed-node": [], "daemon-mode": "explorer", "enable-cors": "", "rpc-access-token": "s3cret",
          "rpc-max-rpm": 600, "rpc-sync-cache-size": 128, "transaction-validation-threads": 6,
          "prune": false, "prune-depth": 10080, "fee-address": "WrkzX", "zmq-pub": "", "stratum-bind-port": 0,
          "db-compression-level": 3, "rpc-ipc-path": "/run/wrkz.sock", "tx-notify": "/usr/bin/true %s"
        }"#;
        let got = from_text(dump).unwrap();
        assert_eq!(
            got.args,
            [
                "--data-dir",
                "/var/lib/wrkz",
                "--log-level",
                "2",
                "--no-console",
                "--db-threads",
                "8",
                "--p2p-bind-ip",
                "0.0.0.0",
                "--p2p-bind-port",
                "17855",
                "--out-peers",
                "8",
                "--rpc-bind-ip",
                "127.0.0.1",
                "--rpc-bind-port",
                "17856",
                "--add-peer",
                "1.2.3.4:17855",
                "--add-peer",
                "5.6.7.8:17855",
                "--daemon-mode",
                "explorer",
                "--rpc-access-token",
                "s3cret",
                "--rpc-max-rpm",
                "600",
                "--rpc-sync-cache-size",
                "128",
                "--threads",
                "6",
                // "zmq-pub": "" is the C++'s off, which the default is not.
                "--zmq-pub",
                "",
                // The dump's "stratum-bind-port": 0, which is "off" either way.
                "--stratum-bind-port",
                "0",
                "--db-compression-level",
                "3",
                "--rpc-ipc-path",
                "/run/wrkz.sock",
                "--tx-notify",
                "/usr/bin/true %s",
            ]
        );
        assert!(got.notes.is_empty(), "every key of a C++ dump is read: {:?}", got.notes);
    }

    #[test]
    fn the_rocksdb_and_compaction_keys_read_as_their_flags() {
        let dump = r#"{
          "skip-boot-compaction": true, "db-enable-compression": false, "db-compression-dict-bytes": 16384,
          "db-compression-level": -3, "db-row-cache-percent": 25, "db-bottom-filters": true, "db-block-size": 16,
          "db-max-open-files": -1, "db-read-buffer-size": 1024, "db-threads": 4, "db-write-buffer-size": 128,
          "auto-compaction-min-gap-blocks": 0, "auto-compaction-min-free-bytes": 1073741824
        }"#;
        let got = from_text(dump).unwrap();
        assert!(got.notes.is_empty(), "every one of them is read: {:?}", got.notes);
        assert_eq!(
            got.args,
            [
                "--skip-boot-compaction",
                "--db-enable-compression=false",
                "--db-compression-dict-bytes",
                "16384",
                "--db-compression-level",
                "-3",
                "--db-row-cache-percent",
                "25",
                "--db-bottom-filters",
                "--db-block-size",
                "16",
                "--db-max-open-files",
                "-1",
                "--db-read-buffer-size",
                "1024",
                "--db-threads",
                "4",
                "--db-write-buffer-size",
                "128",
                "--auto-compaction-min-gap-blocks",
                "0",
                "--auto-compaction-min-free-bytes",
                "1073741824",
            ]
        );
        // The C++ dump's defaults: compression on says so, the rest nothing.
        assert_eq!(
            args(r#"{"db-enable-compression": true, "db-bottom-filters": false, "skip-boot-compaction": false}"#),
            ["--db-enable-compression"]
        );
        assert_eq!(args(r#"{"db-enable-compression": "off"}"#), ["--db-enable-compression=false"]);
        assert!(from_text(r#"{"db-block-size": -4}"#).is_err(), "only the signed keys take a negative number");
        assert!(from_text(r#"{"db-enable-compression": "sometimes"}"#).is_err());
    }

    #[test]
    fn a_prune_depth_counts_only_with_prune_on() {
        assert_eq!(args(r#"{"prune": true, "prune-depth": 20000}"#), ["--prune", "--prune-depth", "20000"]);
        assert_eq!(args(r#"{"prune-depth": 20000, "prune": true}"#), ["--prune-depth", "20000", "--prune"]);
        assert!(args(r#"{"prune": false, "prune-depth": 20000}"#).is_empty());
    }

    #[test]
    fn special_values() {
        assert_eq!(args(r#"{"load-checkpoints": "/etc/cp.csv"}"#), ["--load-checkpoints", "/etc/cp.csv"]);
        assert_eq!(args(r#"{"load-checkpoints": ""}"#), ["--no-checkpoints"], "empty is the C++'s none");
        assert!(args(r#"{"load-checkpoints": "default", "daemon-mode": "standard"}"#).is_empty());
        assert!(args(r#"{"transaction-validation-threads": 0}"#).is_empty());
        assert_eq!(args(r#"{"zmq-pub": "tcp://*:17857", "no-zmq": true}"#), ["--zmq-pub", "tcp://*:17857", "--no-zmq"]);
        assert_eq!(args(r#"{"zmq-pub": ""}"#), ["--zmq-pub", ""], "empty is the C++'s off");
        assert!(args(r#"{"zmq-pub": null, "no-zmq": false}"#).is_empty());
        assert_eq!(args(r#"{"p2p-external-port": 27855}"#), ["--p2p-external-port", "27855"]);
        assert_eq!(
            args(r#"{"block-notify": "on-block %s", "notify-during-sync": true}"#),
            ["--block-notify", "on-block %s", "--notify-during-sync"]
        );
        let priority = from_text(r#"{"add-priority-node": ["9.9.9.9:17855"], "add-exclusive-node": []}"#).unwrap();
        assert_eq!(priority.args, ["--add-priority-node", "9.9.9.9:17855"]);
        assert!(priority.notes.is_empty(), "{:?}", priority.notes);
        assert!(from_text(r#"{"daemon-mode": "miner"}"#).is_err());
    }

    #[test]
    fn one_off_actions_in_a_file_are_ignored_with_a_note() {
        let got =
            from_text(r#"{"resync": true, "rewind-to-height": 4200000, "dump-file": "", "import-validate": false}"#)
                .unwrap();
        assert!(got.args.is_empty(), "never turned into flags: {:?}", got.args);
        assert_eq!(
            got.notes,
            [
                "resync: a one-off action taken on the command line only, as in Wrkzd; ignored",
                "rewind-to-height: a one-off action taken on the command line only, as in Wrkzd; ignored",
            ]
        );
    }

    #[test]
    fn the_key_value_form_reads_too() {
        let text = "# an old Wrkzd config\ndata-dir=/srv/wrkz\nhide-my-port=true\nadd-peer=1.1.1.1:17855\n\
                    add-peer = 2.2.2.2:17855\nrpc-bind-port=\"27856\"\n\nno-console=0\n\
                    add-priority-node=3.3.3.3:17855\nadd-priority-node=4.4.4.4:17855\n";
        assert_eq!(
            args(text),
            [
                "--data-dir",
                "/srv/wrkz",
                "--hide-my-port",
                "--add-peer",
                "1.1.1.1:17855",
                "--add-peer",
                "2.2.2.2:17855",
                "--rpc-bind-port",
                "27856",
                "--add-priority-node",
                "3.3.3.3:17855",
                "--add-priority-node",
                "4.4.4.4:17855",
            ]
        );
        assert!(from_text("data-dir /srv\n").is_err(), "a line without = is refused");
    }

    #[test]
    fn wrong_types_unknown_keys_and_bad_documents() {
        assert!(from_text(r#"{"p2p-bind-port": -1}"#).is_err());
        assert!(from_text(r#"{"hide-my-port": "sometimes"}"#).is_err());
        assert!(from_text(r#"{"add-peer": "1.2.3.4:17855"}"#).is_err(), "a list must be an array");
        assert!(from_text(r#"["data-dir"]"#).is_err());
        assert!(from_text(r#"{"data-dir": "#).is_err());
        let typo = from_text(r#"{"rpc-bind-ipp": "0.0.0.0"}"#).unwrap();
        assert!(typo.args.is_empty());
        assert_eq!(typo.notes, ["rpc-bind-ipp: not a setting of this daemon or of Wrkzd, ignored"]);
    }
}
