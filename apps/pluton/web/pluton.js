// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// Where a browser keeps wallet files: this browser's own IndexedDB, and
// nowhere else. The wallet (WebAssembly) calls these two functions; it owns
// the worker and the messages itself, so this file holds no wallet logic.
//
// A wallet file is encrypted with the user's password before it arrives here,
// so what is stored is useless to anyone who cannot open it.

const DB_NAME = "rust-pluton-wallet";
const STORE = "files";

function openDb() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, 1);
    request.onupgradeneeded = () => request.result.createObjectStore(STORE);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

function toBase64(bytes) {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

// Everything this browser holds, as `{ name: "<base64>" }`, which is what the
// wallet hands to its worker at the start.
export async function loadFiles() {
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, "readonly");
    const store = tx.objectStore(STORE);
    const keys = store.getAllKeys();
    const values = store.getAll();
    tx.oncomplete = () => {
      const files = {};
      keys.result.forEach((key, i) => {
        const value = values.result[i];
        files[key] = toBase64(value instanceof Uint8Array ? value : new Uint8Array(value));
      });
      resolve(files);
    };
    tx.onerror = () => reject(tx.error);
  });
}

// Keep one file for the next visit. Called every time the wallet saves.
export async function storeFile(name, bytes) {
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, "readwrite");
    // Stored as bytes; only the hand-over to the worker is base64.
    tx.objectStore(STORE).put(new Uint8Array(bytes), name);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}
