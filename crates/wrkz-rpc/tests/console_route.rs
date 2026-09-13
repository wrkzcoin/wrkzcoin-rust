// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `POST /console` (`RpcServer::console`, `RpcServer.cpp:1062-1088`): a route
//! of the IPC socket only, through the middleware of any route with a body,
//! answering 503 until the daemon installs its console and after it clears it.

mod fake;

use fake::FakeNode;
use std::sync::Arc;
use wrkz_rpc::console::{ConsoleSlot, NOT_READY};
use wrkz_rpc::http::{Request, Response};
use wrkz_rpc::ipc::IPC_PEER;
use wrkz_rpc::server::{dispatch, Context, ServerConfig};

fn context(config: ServerConfig) -> Context {
    Context::new(Arc::new(FakeNode::default()), config)
}

fn request(method: &str, body: &str, headers: &[(&str, &str)]) -> Request {
    let mut all = vec![("Content-Type".to_string(), "application/json".to_string())];
    all.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    Request {
        method: method.into(),
        path: "/console".into(),
        query: String::new(),
        version: "HTTP/1.1".into(),
        headers: all,
        body: body.as_bytes().to_vec(),
    }
}

fn post(ctx: &Context, body: &str, peer: &str) -> Response {
    dispatch(ctx, &request("POST", body, &[]), peer)
}

fn text(res: &Response) -> String {
    String::from_utf8(res.body.clone()).unwrap()
}

/// A console that says what it was asked to run.
fn echo(slot: &ConsoleSlot) {
    slot.install(Arc::new(|line: &str| format!("ran: {line}\n")));
}

#[test]
fn over_tcp_the_console_is_not_a_route() {
    let ctx = context(ServerConfig::default());
    echo(&ctx.console);
    for peer in ["127.0.0.1", "::1", "203.0.113.9"] {
        let res = post(&ctx, r#"{"command":"stop"}"#, peer);
        assert_eq!((res.status, res.body.len()), (404, 0), "{peer}");
    }
    let tokened = context(ServerConfig { access_token: "s3cret".into(), ..Default::default() });
    echo(&tokened.console);
    let with_token = request("POST", r#"{"command":"stop"}"#, &[("X-API-Key", "s3cret")]);
    assert_eq!(dispatch(&tokened, &with_token, "127.0.0.1").status, 404, "token or no token");
}

#[test]
fn on_the_socket_a_command_runs_once_the_console_is_installed() {
    let ctx = context(ServerConfig::default());
    let res = post(&ctx, r#"{"command":"status"}"#, IPC_PEER);
    assert_eq!(res.status, 503);
    assert_eq!(text(&res), format!(r#"{{"error":"{NOT_READY}","status":"Failed"}}"#));

    echo(&ctx.console);
    let res = post(&ctx, r#"{"command":"print_bc 1 2"}"#, IPC_PEER);
    assert_eq!(res.status, 200);
    assert_eq!(text(&res), r#"{"output":"ran: print_bc 1 2\n","status":"OK"}"#);
    assert_eq!(res.header("Content-Type"), Some("application/json"));

    ctx.console.clear();
    assert_eq!(post(&ctx, r#"{"command":"status"}"#, IPC_PEER).status, 503, "and again once it is cleared");
}

#[test]
fn a_bad_body_is_refused_as_every_route_refuses_one() {
    let ctx = context(ServerConfig::default());
    let not_json = post(&ctx, "status", IPC_PEER);
    assert_eq!(not_json.status, 400);
    assert!(text(&not_json).contains("Failed to parse request body as JSON"), "{}", text(&not_json));
    let missing = post(&ctx, "{}", IPC_PEER);
    assert_eq!(text(&missing), r#"{"error":"Missing JSON parameter: 'command'","status":"Failed"}"#);
    assert_eq!(missing.status, 400);
    let wrong = post(&ctx, r#"{"command":["stop"]}"#, IPC_PEER);
    assert_eq!(wrong.status, 500);
    assert!(text(&wrong).contains("Internal server error: "), "{}", text(&wrong));
}

#[test]
fn the_socket_is_asked_for_the_token_only_when_the_operator_says_so() {
    let open = context(ServerConfig { access_token: "s3cret".into(), ..Default::default() });
    echo(&open.console);
    assert_eq!(post(&open, r#"{"command":"help"}"#, IPC_PEER).status, 200, "the socket file's mode is the gate");

    let strict = context(ServerConfig { access_token: "s3cret".into(), ipc_require_token: true, ..Default::default() });
    echo(&strict.console);
    assert_eq!(post(&strict, r#"{"command":"help"}"#, IPC_PEER).status, 401);
    let with_token = request("POST", r#"{"command":"help"}"#, &[("X-API-Key", "s3cret")]);
    assert_eq!(dispatch(&strict, &with_token, IPC_PEER).status, 200);

    let no_token = context(ServerConfig { ipc_require_token: true, ..Default::default() });
    echo(&no_token.console);
    assert_eq!(post(&no_token, r#"{"command":"help"}"#, IPC_PEER).status, 200, "no token set, none asked for");
}

#[test]
fn only_post_is_routed() {
    let ctx = context(ServerConfig::default());
    echo(&ctx.console);
    assert_eq!(dispatch(&ctx, &request("GET", "", &[]), IPC_PEER).status, 404);
}

/// The whole way, over a real socket and a real TCP listener.
#[cfg(unix)]
#[test]
fn the_console_answers_over_the_socket_and_not_over_tcp() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let dir = std::env::temp_dir().join(format!("wrkz-console-route-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rpc.sock");
    let _ = std::fs::remove_file(&path);
    let config =
        ServerConfig { bind: "127.0.0.1:0".into(), ipc_path: path.display().to_string(), ..Default::default() };
    let server = wrkz_rpc::server::start(Arc::new(FakeNode::default()), config).expect("starts");
    assert!(server.ipc_path().is_some(), "{:?}", server.ipc_error());

    let body = r#"{"command":"height"}"#;
    let raw = format!(
        "POST /console HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let ask = |mut stream: Box<dyn ReadWrite>| {
        stream.write_all(raw.as_bytes()).unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        answer
    };
    trait ReadWrite: Read + Write {}
    impl<T: Read + Write> ReadWrite for T {}

    let before = ask(Box::new(UnixStream::connect(&path).unwrap()));
    assert!(before.starts_with("HTTP/1.1 503 "), "{before}");
    server.console().install(Arc::new(|line: &str| format!("ran: {line}\n")));
    let answer = ask(Box::new(UnixStream::connect(&path).unwrap()));
    assert!(answer.starts_with("HTTP/1.1 200 "), "{answer}");
    assert!(answer.ends_with(r#"{"output":"ran: height\n","status":"OK"}"#), "{answer}");
    let tcp = ask(Box::new(std::net::TcpStream::connect(server.local_addr()).unwrap()));
    assert!(tcp.starts_with("HTTP/1.1 404 "), "{tcp}");
}
