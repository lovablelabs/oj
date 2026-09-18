// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The [build-dependencies] entry on deno_core (with the "v8" feature) matters
// even though this script is trivial: resolver v2 unifies build-dependency
// features separately from normal dependencies, and this keeps the host-side
// deno_core build in the workspace dep train compiled with the same features
// as the target-side one.

fn main() {
    // Test binaries load napi addons (see the ignored napi test), which
    // resolve napi_* symbols against the running executable. Keep all global
    // symbols exported from test executables; the oj binary itself exports
    // exactly deno_napi's list via deno_napi::print_linker_flags in
    // crates/oj/build.rs.
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg-tests=-Wl,-export_dynamic");
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
    println!("cargo:rustc-link-arg-tests=-rdynamic");
}
