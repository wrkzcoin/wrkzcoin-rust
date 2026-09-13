// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `nlohmann::json::dump(4)`, which is what every `ApiDispatcher` handler
//! writes (`res.set_content(j.dump(4) + "\n", "application/json")`).
//!
//! [`wrkz_rpc::json::Json::to_string`] is the compact `dump()` the daemon uses.
//! The wallet API pretty prints instead, so a client diffing two responses by
//! eye sees the same bytes the C++ produced. Keys come out in ascending byte
//! order in both, because `nlohmann`'s default object is a `std::map`.
//!
//! The one rule worth naming: an empty object is `{}` and an empty array is
//! `[]`, on one line, with no newline inside — `nlohmann` special cases both.

use std::fmt::Write as _;

use wrkz_rpc::json::Json;

/// One indentation step. `dump(4)`.
const INDENT: usize = 4;

/// Render `value` the way `nlohmann::json::dump(4)` does, with the trailing
/// newline `ApiDispatcher` appends.
pub fn body(value: &Json) -> String {
    let mut out = String::new();
    write_value(value, 0, &mut out);
    out.push('\n');
    out
}

/// Render `value` as `dump(4)` with no trailing newline.
pub fn dump(value: &Json) -> String {
    let mut out = String::new();
    write_value(value, 0, &mut out);
    out
}

fn write_value(value: &Json, depth: usize, out: &mut String) {
    match value {
        Json::Object(items) if items.is_empty() => out.push_str("{}"),
        Json::Array(items) if items.is_empty() => out.push_str("[]"),
        Json::Object(items) => {
            let mut order: Vec<&(String, Json)> = items.iter().collect();
            order.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push_str("{\n");
            for (i, (key, item)) in order.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                pad(depth + 1, out);
                write_string(key, out);
                out.push_str(": ");
                write_value(item, depth + 1, out);
            }
            out.push('\n');
            pad(depth, out);
            out.push('}');
        }
        Json::Array(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                pad(depth + 1, out);
                write_value(item, depth + 1, out);
            }
            out.push('\n');
            pad(depth, out);
            out.push(']');
        }
        // Scalars print exactly as the compact writer prints them.
        other => out.push_str(&other.to_string()),
    }
}

fn pad(depth: usize, out: &mut String) {
    for _ in 0..depth * INDENT {
        out.push(' ');
    }
}

/// `nlohmann`'s string escaping, which is what
/// [`wrkz_rpc::json::Json::to_string`] already implements for a bare string.
fn write_string(s: &str, out: &mut String) {
    let _ = write!(out, "{}", Json::Str(s.to_string()).to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrkz_rpc::json::Obj;

    #[test]
    fn an_object_is_indented_four_spaces_with_sorted_keys() {
        let mut o = Obj::new();
        o.set("unlocked", 100u64).set("locked", 0u64);
        assert_eq!(body(&o.build()), "{\n    \"locked\": 0,\n    \"unlocked\": 100\n}\n");
    }

    #[test]
    fn empty_containers_stay_on_one_line() {
        let mut o = Obj::new();
        o.set("transactions", Json::Array(Vec::new()));
        assert_eq!(body(&o.build()), "{\n    \"transactions\": []\n}\n");
        assert_eq!(dump(&Obj::new().build()), "{}");
    }

    #[test]
    fn nested_arrays_of_objects_indent_per_level() {
        let mut inner = Obj::new();
        inner.set("address", "Wrkz").set("amount", 5u64);
        let mut outer = Obj::new();
        outer.set("transfers", Json::Array(vec![inner.build()]));
        let expected = [
            "{",
            "    \"transfers\": [",
            "        {",
            "            \"address\": \"Wrkz\",",
            "            \"amount\": 5",
            "        }",
            "    ]",
            "}",
            "",
        ]
        .join("\n");
        assert_eq!(body(&outer.build()), expected);
    }

    #[test]
    fn a_top_level_array_is_valid_too() {
        // `GET /balances` prints a bare array (`ApiDispatcher.cpp:1885`).
        let mut e = Obj::new();
        e.set("address", "Wrkz").set("locked", 0u64).set("unlocked", 1u64);
        let array = Json::Array(vec![e.build()]);
        assert!(body(&array).starts_with("[\n    {\n        \"address\""));
    }
}
