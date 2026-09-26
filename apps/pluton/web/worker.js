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

import init, { worker_init, worker_command, worker_tick, worker_poke } from "./pkg/rust_pluton_wallet.js";

const ready = init();
let started = false;
let timer = null;

// The node's event stream (GET /ws). It only wakes the wallet: every balance
// still comes from the ordinary sync, so a node without the stream (every C++
// node) is simply polled, as before.
const WAKE_TOPICS = new Set(["hello", "hashblock", "chain_main", "chainswitch", "txpool_add", "txpool_del"]);
// Nothing heard for this long and the stream counts as gone: two and a half
// of the node's thirty-second heartbeats.
const LIVE_WINDOW_MS = 75000;
// A node that never accepts the upgrade is left alone for this long.
const UNSUPPORTED_RETRY_MS = 600000;
let socket = null;
let socketUrl = null;
let lastFrame = 0;
let retryAt = 0;
let backoff = 1000;
let failedOpens = 0;

function streamLive() {
  return socket !== null && Date.now() - lastFrame < LIVE_WINDOW_MS;
}

function tickSoon() {
  if (timer !== null) {
    clearTimeout(timer);
    timer = setTimeout(loop, 10);
  }
}

function openStream(url) {
  let ws;
  try {
    ws = new WebSocket(url);
  } catch (e) {
    retryAt = Date.now() + backoff;
    return;
  }
  let opened = false;
  socket = ws;
  ws.onopen = () => {
    opened = true;
    failedOpens = 0;
    backoff = 1000;
  };
  ws.onmessage = (message) => {
    lastFrame = Date.now();
    let topic = null;
    try {
      topic = JSON.parse(message.data).topic;
    } catch (e) {
      return;
    }
    if (WAKE_TOPICS.has(topic)) {
      try {
        worker_poke();
      } catch (e) {
        // Not initialised yet; the next tick asks anyway.
      }
      tickSoon();
    }
  };
  ws.onclose = () => {
    if (socket !== ws) {
      return;
    }
    socket = null;
    lastFrame = 0;
    // A browser does not say why an upgrade failed; a node that never
    // accepted one is most likely one without the stream.
    failedOpens = opened ? 0 : failedOpens + 1;
    retryAt = Date.now() + (failedOpens >= 3 ? UNSUPPORTED_RETRY_MS : backoff);
    backoff = Math.min(backoff * 2, 60000);
  };
}

// Follow the stream the wallet names, or none.
function followStream(url) {
  if (url !== socketUrl) {
    if (socket !== null) {
      const old = socket;
      socket = null;
      old.close();
    }
    socketUrl = url;
    lastFrame = 0;
    retryAt = 0;
    backoff = 1000;
    failedOpens = 0;
  }
  if (socketUrl && socket === null && Date.now() >= retryAt) {
    openStream(socketUrl);
  }
}

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
    const result = JSON.parse(worker_tick(streamLive()));
    if (result.events && result.events.length > 0) {
      post(JSON.stringify(result.events));
    }
    followStream(result.streamUrl ?? null);
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
