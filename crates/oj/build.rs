// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

fn main() {
    // Export the Node-API symbols from the oj binary so `.node` native addons
    // (napi) loaded by the embedded JS engine can resolve them against this
    // executable. On macOS this uses -exported_symbols_list, which also hides
    // every symbol not on deno_napi's list; that is the intended usage (the
    // deno CLI links the same way) and fine for a CLI binary.
    deno_napi::print_linker_flags("oj");
}
