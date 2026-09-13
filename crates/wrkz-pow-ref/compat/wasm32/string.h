// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// wasm32-unknown-unknown has no C library (build.rs). These symbols are
// defined by Rust's compiler-builtins, which every wasm32 Rust binary links.
#pragma once

#include <stddef.h>

void *memcpy(void *restrict dst, const void *restrict src, size_t n);
void *memmove(void *dst, const void *src, size_t n);
void *memset(void *dst, int c, size_t n);
int memcmp(const void *a, const void *b, size_t n);
