// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The HTTP/1.1 request parser of `wrkz_rpc::http`, which is hand-written and
//! is the first thing an unauthenticated client reaches.
//!
//! The property is that it is total and that it respects its own limits: no
//! panic, and nothing larger than `HttpLimits` allows comes back accepted —
//! in particular no allocation sized from a `Content-Length` the client
//! declared.
#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::BufReader;
use wrkz_rpc::http::{self, HttpLimits};

fuzz_target!(|data: &[u8]| {
    // Deliberately small, so a corpus entry of a few hundred bytes can reach
    // the limit paths that the 2 MiB default never would.
    let limits = HttpLimits { max_request_line: 512, max_header_line: 512, max_headers: 16, max_body: 4096 };
    let mut reader = BufReader::new(data);
    let Ok(request) = http::read_request(&mut reader, &limits) else { return };
    assert!(request.body.len() <= limits.max_body, "body past the cap was accepted");
    assert!(request.headers.len() <= limits.max_headers, "more headers than the cap were accepted");
    assert!(!request.path.contains('?'), "the query string must be split off the path");
    // The accessors a handler uses must not panic on anything that parsed.
    let _ = request.header("content-type");
    let _ = request.wants_keep_alive();
    let _ = http::accepts_gzip(&request);
});
