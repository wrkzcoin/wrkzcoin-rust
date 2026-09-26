// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The WebSocket frame reader of `wrkz_ws`, which reads the daemon's `GET /ws`
//! subscribers (the server role) and the daemon's stream in every wallet (the
//! client role), and the answer-head reader the wallet runs on the upgrade.
//!
//! The properties: all of it is total; nothing comes back larger than the cap
//! it was given — above all, no allocation sized from a length the peer
//! declared — and the buffered parser and the stream reader agree on every
//! input, since the wallet uses one and the daemon the other.
#![no_main]
use libfuzzer_sys::fuzz_target;
use wrkz_ws::frame::{parse_frame, read_frame, Assembler, Message, Role};
use wrkz_ws::handshake;

const CAP: usize = 1024;

fuzz_target!(|data: &[u8]| {
    for role in [Role::Server, Role::Client] {
        // The buffered parser against the stream reader, frame by frame.
        let mut rest = data;
        let mut assembler = Assembler::new(CAP);
        loop {
            let parsed = parse_frame(rest, role, assembler.frame_cap());
            let mut stream = rest;
            let read = read_frame(&mut stream, role, assembler.frame_cap());
            match (parsed, read) {
                (Ok(Some((frame, used))), Ok(same)) => {
                    assert_eq!(frame, same, "the two readers disagree on a frame");
                    assert_eq!(used, rest.len() - stream.len(), "the two readers disagree on its length");
                    assert!(used <= rest.len());
                    assert!(frame.payload.len() <= CAP.max(125), "a payload past the cap was accepted");
                    rest = &rest[used..];
                    match assembler.push(frame) {
                        Ok(Some(Message::Text(t))) => assert!(t.len() <= CAP),
                        Ok(Some(Message::Binary(b))) => assert!(b.len() <= CAP),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                // Incomplete: the stream reader runs out of bytes.
                (Ok(None), Err(_)) => break,
                (Err(_), Err(_)) => break,
                (parsed, read) => panic!("the readers disagree: {parsed:?} against {read:?}"),
            }
        }
    }

    // The upgrade answer, as the wallet reads it: bounded, and never past the
    // blank line.
    let mut reader = data;
    if let Ok(head) = handshake::read_response_head(&mut reader, 512) {
        assert!(data.len() - reader.len() <= 512, "the head was read past its cap");
        let _ = handshake::check_response(&head, "dGhlIHNhbXBsZSBub25jZQ==");
    }
});
