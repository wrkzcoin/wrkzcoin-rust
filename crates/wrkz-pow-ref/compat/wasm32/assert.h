// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// wasm32-unknown-unknown has no C library (build.rs). The vendored C is built
// with NDEBUG, so assert() is always the empty form; static_assert is C11's
// keyword, as <assert.h> defines it.
#pragma once

#define assert(e) ((void)0)
#define static_assert _Static_assert
