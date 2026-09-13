// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

//! The browser. The window draws on the page; the wallet runs in a Web Worker,
//! where a blocking request is allowed — which is what lets the same wallet
//! code that runs on a desktop run here, syncing and building transactions
//! without ever freezing the page.
//!
//! - The page (`web/pluton.js`) keeps wallet files in IndexedDB, starts the
//!   worker and hands it what it has.
//! - The worker (`web/worker.js`) calls [`worker_init`], [`worker_command`]
//!   and [`worker_tick`], and posts every event back to the page.
//! - The page gives each event to [`WalletWorker`], which is what the window
//!   listens to.
//!
//! Nothing secret crosses that line except the password the user just typed;
//! keys stay inside the worker, and wallet files are encrypted before they are
//! stored.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wrkz_wallet::http::{HttpReply, HttpTransport};

use crate::protocol::{Command, Event, EventSink, WalletHandle};
use crate::service::{Service, Storage};

/// The page's storage (`web/pluton.js`). The wallet owns the worker and the
/// messages; this is only where files are kept between visits.
#[wasm_bindgen(module = "/web/pluton.js")]
extern "C" {
    #[wasm_bindgen(js_name = loadFiles, catch)]
    async fn load_files() -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_name = storeFile, catch)]
    async fn store_file(name: &str, bytes: js_sys::Uint8Array) -> Result<JsValue, JsValue>;
}

////////////////////////
/* THE PAGE'S SIDE    */
////////////////////////

/// The worker, as the window talks to it.
pub struct WalletWorker {
    worker: web_sys::Worker,
    // Kept alive for as long as the worker is: dropping it would silence the
    // wallet.
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

impl WalletWorker {
    /// Start `worker.js`, hand it whatever this browser has stored, and route
    /// everything it says to `sink`. A wallet file it saves is written back to
    /// storage here rather than shown.
    pub fn start(sink: EventSink) -> Rc<dyn WalletHandle> {
        let worker = web_sys::Worker::new("./worker.js").expect("the wallet worker starts");
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            let Some(text) = event.data().as_string() else { return };
            let events = match serde_json::from_str::<Vec<Event>>(&text) {
                Ok(events) => events,
                Err(e) => {
                    web_sys::console::error_1(&format!("the wallet sent something unreadable: {e}").into());
                    return;
                }
            };
            for event in events {
                match event {
                    Event::Persist { name, bytes } => wasm_bindgen_futures::spawn_local(async move {
                        let array = js_sys::Uint8Array::from(bytes.as_slice());
                        if let Err(e) = store_file(&name, array).await {
                            web_sys::console::error_2(&"the wallet file could not be stored".into(), &e);
                        }
                    }),
                    event => sink(event),
                }
            }
        });
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        // The opening hand-over, once the browser has read its storage.
        let opening = worker.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let files = load_files().await.unwrap_or(JsValue::UNDEFINED);
            let files = js_sys::JSON::stringify(&files).map(String::from).unwrap_or_else(|_| "{}".into());
            let _ = opening.post_message(&JsValue::from_str(&format!("{{\"files\":{files}}}")));
        });

        Rc::new(WalletWorker { worker, _on_message: on_message })
    }
}

impl WalletHandle for WalletWorker {
    fn send(&self, command: Command) {
        match serde_json::to_string(&command) {
            Ok(text) => {
                let _ = self.worker.post_message(&JsValue::from_str(&text));
            }
            Err(e) => web_sys::console::error_1(&format!("could not send a command: {e}").into()),
        }
    }
}

////////////////////////
/* THE WORKER'S SIDE  */
////////////////////////

/// HTTP inside a Web Worker: `XMLHttpRequest` in its synchronous form, which
/// only a worker may use. The page's own thread is never blocked by it.
#[derive(Clone, Default)]
pub struct XhrTransport;

impl HttpTransport for XhrTransport {
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        api_key: Option<&str>,
        timeout: Duration,
    ) -> Option<HttpReply> {
        let xhr = web_sys::XmlHttpRequest::new().ok()?;
        // `false`: synchronous. Allowed in a worker, forbidden on the page.
        xhr.open_with_async(method, url, false).ok()?;
        xhr.set_timeout(timeout.as_millis().min(u128::from(u32::MAX)) as u32);
        if body.is_some() {
            xhr.set_request_header("Content-Type", "application/json").ok()?;
        }
        if let Some(key) = api_key {
            xhr.set_request_header("X-API-KEY", key).ok()?;
        }
        // A refused connection, a TLS failure or a timeout all throw here, and
        // all mean the same thing to the caller: no answer.
        xhr.send_with_opt_str(body).ok()?;
        let status = xhr.status().ok()?;
        let body = xhr.response_text().ok().flatten().unwrap_or_default();
        Some(HttpReply { status, body })
    }
}

/// The wallet files the page is holding for us. Saving updates this copy and
/// the wallet emits the bytes; the page writes them to IndexedDB.
#[derive(Default)]
pub struct WebStorage {
    files: HashMap<String, Vec<u8>>,
}

impl Storage for WebStorage {
    fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.files.keys().filter(|n| !n.ends_with(".json")).cloned().collect();
        names.sort();
        names
    }

    fn load(&self, name: &str) -> Result<Vec<u8>, String> {
        self.files.get(name).cloned().ok_or_else(|| format!("'{name}' is not in this browser's storage"))
    }

    fn save(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.files.insert(name.to_string(), bytes.to_vec());
        Ok(())
    }

    fn exists(&self, name: &str) -> bool {
        self.files.contains_key(name)
    }
}

thread_local! {
    /// The worker runs one wallet, on its own thread, so this is the whole of
    /// its state.
    static SERVICE: RefCell<Option<Service<XhrTransport, WebStorage>>> = const { RefCell::new(None) };
}

/// Start the wallet with the files the page has kept, as
/// `{"name": "<base64 of the file>"}`.
#[wasm_bindgen]
pub fn worker_init(files_json: &str) {
    console_error_panic_hook::set_once();
    let mut storage = WebStorage::default();
    if let Ok(files) = serde_json::from_str::<HashMap<String, String>>(files_json) {
        for (name, base64) in files {
            match decode_base64(&base64) {
                Some(bytes) => {
                    let _ = storage.save(&name, &bytes);
                }
                None => web_sys::console::error_1(&format!("'{name}' in storage is not readable").into()),
            }
        }
    }
    // `true`: this is a browser, so a send pays the fee that skips the proof of
    // work unless a server is configured (the search is impractical here).
    let service = Service::new(XhrTransport, XhrTransport, storage, true);
    SERVICE.with(|cell| *cell.borrow_mut() = Some(service));
}

/// Answer one command; returns the events as JSON.
#[wasm_bindgen]
pub fn worker_command(command_json: &str) -> String {
    let command = match serde_json::from_str::<Command>(command_json) {
        Ok(command) => command,
        Err(e) => {
            return events_json(&[crate::protocol::Event::Notice {
                message: format!("The wallet did not understand a request: {e}"),
                kind: crate::protocol::NoticeKind::Error,
            }])
        }
    };
    SERVICE.with(|cell| match cell.borrow_mut().as_mut() {
        Some(service) => events_json(&service.handle(command)),
        None => events_json(&[]),
    })
}

/// One round of syncing; returns `{"events": [...], "waitMs": n}`.
#[wasm_bindgen]
pub fn worker_tick() -> String {
    SERVICE.with(|cell| match cell.borrow_mut().as_mut() {
        Some(service) => {
            let (events, wait) = service.tick();
            serde_json::json!({ "events": events, "waitMs": wait.as_millis() as u64 }).to_string()
        }
        None => serde_json::json!({ "events": [], "waitMs": 250 }).to_string(),
    })
}

fn events_json(events: &[Event]) -> String {
    serde_json::to_string(events).unwrap_or_else(|_| "[]".into())
}

/// The page stores file bytes as base64; `atob` is the browser's own decoder,
/// so nothing here has to be a second implementation of it.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let binary = js_sys::global().unchecked_into::<web_sys::DedicatedWorkerGlobalScope>().atob(text).ok()?;
    Some(binary.chars().map(|c| c as u32 as u8).collect())
}
