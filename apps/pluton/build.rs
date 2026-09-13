// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// Compiles the Slint user interface into the crate.

fn main() {
    slint_build::compile("ui/app.slint").expect("the Slint interface compiles");
}
