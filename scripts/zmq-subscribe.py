#!/usr/bin/env python3
# Copyright (c) 2026, The WrkzCoin developers
#
# Please see the included LICENSE file for more information

"""Watch a daemon's ZMQ feed: every block, reorganisation and pool change, one
line each, as the daemon publishes them. It works against the C++ Wrkzd and
against wrkz-node alike, because it is a plain libzmq SUB socket.

    scripts/zmq-subscribe.py                              # tcp://127.0.0.1:17857
    scripts/zmq-subscribe.py --topic hashblock --topic chainswitch
    scripts/zmq-subscribe.py --count 1 --timeout 180 tcp://127.0.0.1:17857

Each line is the topic and its JSON body:

    hashblock {"height":4300123,"hash":"..."}
    chain_main {"height":4300123,"hash":"...","transaction_hashes":["...", ...]}

Topics match by prefix, as ZMQ matches them, so --topic hashblock also brings
hashblock_alt. With no --topic every topic comes through.

It needs pyzmq (`pip install pyzmq`), which carries its own libzmq; that is
also what makes it a fair test of the daemon's publisher, since the daemon
does not use libzmq.

Exit status: 0 once --count messages arrived (or on Ctrl-C), 1 if --timeout
passed with nothing new, 2 without pyzmq.
"""

import argparse
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("endpoint", nargs="?", default="tcp://127.0.0.1:17857")
    parser.add_argument("--topic", action="append", default=[], help="a topic prefix; repeat for more")
    parser.add_argument("--count", type=int, default=0, help="exit after this many messages")
    parser.add_argument("--timeout", type=float, default=0, help="give up after this many quiet seconds")
    args = parser.parse_args()

    try:
        import zmq
    except ImportError:
        print("pyzmq is not installed: pip install pyzmq", file=sys.stderr)
        return 2

    context = zmq.Context()
    socket = context.socket(zmq.SUB)
    socket.setsockopt(zmq.LINGER, 0)
    for topic in args.topic or [""]:
        socket.setsockopt(zmq.SUBSCRIBE, topic.encode())
    socket.connect(args.endpoint)
    poller = zmq.Poller()
    poller.register(socket, zmq.POLLIN)

    received = 0
    try:
        while args.count == 0 or received < args.count:
            timeout_ms = int(args.timeout * 1000) if args.timeout > 0 else None
            if not poller.poll(timeout_ms):
                print(f"nothing from {args.endpoint} in {args.timeout:g} s", file=sys.stderr)
                return 1
            frames = socket.recv_multipart()
            topic = frames[0].decode(errors="replace")
            body = b"".join(frames[1:]).decode(errors="replace")
            print(f"{topic} {body}", flush=True)
            received += 1
    except KeyboardInterrupt:
        pass
    finally:
        socket.close()
        context.term()
    return 0


if __name__ == "__main__":
    sys.exit(main())
