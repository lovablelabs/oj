// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

fn main() {
    // Test binaries load napi addons (the dep optimizer's rolldown), which
    // resolve napi_* symbols against the running executable. Keep all global
    // symbols exported from test executables, as crates/oj_js/build.rs does;
    // the oj binary itself exports exactly deno_napi's list via
    // deno_napi::print_linker_flags in crates/oj/build.rs.
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg-tests=-Wl,-export_dynamic");
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
    println!("cargo:rustc-link-arg-tests=-rdynamic");
}
