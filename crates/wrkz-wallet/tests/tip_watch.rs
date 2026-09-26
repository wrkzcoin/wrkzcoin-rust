// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! `tip_watch` against a scripted daemon: the upgrade, the wake on an event,
//! the pong, liveness and pacing, a daemon without `/ws`, and the reconnect
//! after a drop.

use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use wrkz_wallet::sync::SyncStep;
use wrkz_wallet::tip_watch::{TipWatch, LIVE_SYNCED_POLL, SYNCED_POLL};
use wrkz_ws::frame::{encode_frame, Message, MessageReader, Opcode, Role};
use wrkz_ws::handshake;

/// One accepted upgrade: the request's key checked, `101` written.
struct Upgraded {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Upgraded {
    fn send_text(&mut self, text: &str) {
        self.writer.write_all(&encode_frame(Opcode::Text, text.as_bytes(), None)).unwrap();
    }

    fn next(&mut self) -> Message {
        MessageReader::new(Role::Server, 4096).read(&mut self.reader).expect("a frame from the wallet")
    }
}

/// A listener that answers each connection with `status`, and hands every
/// `101` connection to the test.
fn daemon(status: u16) -> (String, Receiver<Upgraded>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            // The request head, as the wallet sent it.
            let mut head = Vec::new();
            loop {
                let mut line = String::new();
                if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line == "\r\n" {
                    break;
                }
                head.push(line);
            }
            if status != 101 {
                let _ = writer.write_all(format!("HTTP/1.1 {status} Nope\r\nContent-Length: 0\r\n\r\n").as_bytes());
                continue;
            }
            assert!(head[0].starts_with("GET /ws HTTP/1.1"), "{head:?}");
            let key = head
                .iter()
                .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
                .map(|k| k.trim().to_string())
                .expect("a key");
            writer.write_all(handshake::response(&key).as_bytes()).unwrap();
            if tx.send(Upgraded { reader, writer }).is_err() {
                return;
            }
        }
    });
    (url, rx)
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_event_wakes_the_wallet_and_a_live_stream_slows_the_poll() {
    let (url, upgrades) = daemon(101);
    let watch = TipWatch::start();
    let never = AtomicBool::new(false);
    watch.follow(Some(&url));
    let mut conn = upgrades.recv_timeout(Duration::from_secs(10)).expect("the wallet connects");

    // The hello wakes it: whatever happened while it was away needs a sync.
    let seen = watch.events();
    conn.send_text("{\"topic\":\"hello\",\"data\":{\"height\":1,\"hash\":\"00\",\"topics\":[]}}");
    assert!(watch.wait(Duration::from_secs(10), seen, &never));
    until("the stream to count as live", || watch.is_live());

    let synced = SyncStep::Synced { height: 1 };
    assert_eq!(watch.pace(&synced, SYNCED_POLL), LIVE_SYNCED_POLL);
    let processing = SyncStep::Idle { backoff: Duration::ZERO };
    assert_eq!(watch.pace(&processing, Duration::ZERO), Duration::ZERO, "only a synced wallet waits longer");

    // A heartbeat keeps it live but does not wake it.
    let seen = watch.events();
    conn.send_text("{\"topic\":\"heartbeat\",\"data\":{}}");
    assert!(!watch.wait(Duration::from_millis(300), seen, &never));
    conn.send_text("{\"topic\":\"hashblock\",\"data\":{\"height\":2,\"hash\":\"aa\"}}");
    assert!(watch.wait(Duration::from_secs(10), seen, &never));

    // A ping is answered, masked, with the same payload.
    conn.writer.write_all(&encode_frame(Opcode::Ping, b"hi", None)).unwrap();
    assert_eq!(conn.next(), Message::Pong(b"hi".to_vec()));
}

#[test]
fn a_dropped_stream_is_reconnected() {
    let (url, upgrades) = daemon(101);
    let watch = TipWatch::start();
    watch.follow(Some(&url));
    let first = upgrades.recv_timeout(Duration::from_secs(10)).expect("the wallet connects");
    drop(first);
    let mut second = upgrades.recv_timeout(Duration::from_secs(10)).expect("and connects again");
    let never = AtomicBool::new(false);
    let seen = watch.events();
    second.send_text("{\"topic\":\"chainswitch\",\"data\":{\"common_root_height\":1,\"hashes\":[]}}");
    assert!(watch.wait(Duration::from_secs(10), seen, &never));
}

#[test]
fn a_daemon_without_the_stream_is_polled_as_before() {
    let (url, _upgrades) = daemon(404);
    let watch = TipWatch::start();
    watch.follow(Some(&url));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!watch.is_live());
    let synced = SyncStep::Synced { height: 1 };
    assert_eq!(watch.pace(&synced, SYNCED_POLL), SYNCED_POLL);
}

#[test]
fn a_stop_does_not_wait_on_a_slow_name_lookup() {
    // `.invalid` never resolves (RFC 6761); with an unreachable or slow DNS
    // server the lookup can take many seconds, and nothing can cut it short.
    let watch = TipWatch::start();
    watch.follow(Some("http://daemon.invalid:17856"));
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    drop(watch);
    assert!(started.elapsed() < Duration::from_secs(5), "closing a wallet waited {:?}", started.elapsed());
}

#[test]
fn changing_daemon_moves_the_stream_and_stopping_is_prompt() {
    let (first_url, first) = daemon(101);
    let (second_url, second) = daemon(101);
    let watch = TipWatch::start();
    watch.follow(Some(&first_url));
    let mut old = first.recv_timeout(Duration::from_secs(10)).expect("the first daemon");
    // Past the handshake — during it there is no stream to close yet.
    old.send_text("{\"topic\":\"heartbeat\",\"data\":{}}");
    until("the stream to count as live", || watch.is_live());
    watch.follow(Some(&second_url));
    // The old connection is closed by the wallet...
    assert!(matches!(old.next(), Message::Close(_)));
    // ...and the new daemon is followed.
    let _new = second.recv_timeout(Duration::from_secs(10)).expect("the second daemon");
    // The IPC forms are not followed at all.
    watch.follow_host("/run/wrkz/rpc.sock", 0, false);
    let started = Instant::now();
    drop(watch);
    assert!(started.elapsed() < Duration::from_secs(5), "a stop is noticed within a read slice");
}
