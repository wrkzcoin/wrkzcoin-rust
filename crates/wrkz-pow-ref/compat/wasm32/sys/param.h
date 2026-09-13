// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// wasm32-unknown-unknown has no C library (build.rs). common/int-util.h reads
// only the byte-order macros from here; WebAssembly is little-endian.
#pragma once

#define LITTLE_ENDIAN 1234
#define BIG_ENDIAN 4321
#define BYTE_ORDER LITTLE_ENDIAN
