// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// The wallet, off the page's thread. Everything slow happens here: syncing,
// building a transaction, and the proof of work when no server is configured.
// A worker may make blocking requests, which is what lets the same wallet code
// that runs on a desktop run in a browser.
//
// The page sends exactly one opening message — the wallet files it holds —
// and then commands. Every reply is a JSON array of events.

import init, { worker_init, worker_command, worker_tick } from "./pkg/rust_pluton_wallet.js";

const ready = init();
let started = false;
let timer = null;

function post(events) {
  if (events && events !== "[]") {
    self.postMessage(events);
  }
}

function fail(what, e) {
  post(JSON.stringify([{ event: "notice", message: what + ": " + e, kind: "error" }]));
}

// One round of syncing, then the wait the wallet asked for.
function loop() {
  let wait = 1000;
  try {
    const result = JSON.parse(worker_tick());
    if (result.events && result.events.length > 0) {
      post(JSON.stringify(result.events));
    }
    // Never busier than every 10 ms, however eager the wallet is.
    wait = Math.max(result.waitMs ?? 250, 10);
  } catch (e) {
    fail("The wallet stopped syncing", e);
    wait = 5000;
  }
  timer = setTimeout(loop, wait);
}

self.onmessage = async (message) => {
  await ready;

  if (!started) {
    // The opening hand-over: `{"files": {name: base64}}`.
    started = true;
    try {
      worker_init(JSON.stringify(JSON.parse(message.data).files ?? {}));
    } catch (e) {
      worker_init("{}");
      fail("The stored wallets could not be read", e);
    }
    loop();
    return;
  }

  try {
    post(worker_command(message.data));
  } catch (e) {
    fail("The wallet failed", e);
  }

  // Sync again right away rather than after the current wait.
  if (timer !== null) {
    clearTimeout(timer);
    timer = setTimeout(loop, 10);
  }
};
