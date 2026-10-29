// The tsc driver (Go's internal/execute): `tsc` and `tsc -b` over a `tsc::System`. The `tsrs` binary
// (crates/tsrs_cli) runs it over the OS file system; crates/tsrs_wasm runs it over a host file system.

pub mod build;
pub mod execute;
pub mod sys;
pub mod tsc;
pub mod watcher;
pub mod watchmanager;
mod watchmem;
