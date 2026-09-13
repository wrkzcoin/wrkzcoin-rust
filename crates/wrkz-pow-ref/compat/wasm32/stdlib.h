// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// wasm32-unknown-unknown has no C library (build.rs). malloc and free are
// defined in Rust over the global allocator (src/lib.rs, `wasm_libc`); the
// ring-signature code of cn_shim.c is their only caller.
#pragma once

#include <stddef.h>

void *malloc(size_t n);
void free(void *p);
