# oj_deno_runtime

Fork of [deno_runtime](https://crates.io/crates/deno_runtime) **0.267.0**
(source: [denoland/deno](https://github.com/denoland/deno), `runtime/`),
maintained for [oj](https://github.com/lovablelabs/oj).

The **only** change is in the manifest: the `deno_napi` dependency resolves to
[`oj_deno_napi`](https://crates.io/crates/oj_deno_napi) (upstream deno_napi
0.190.0 plus oj's finalizer-registry performance patch, Vec -> BTreeMap).
Every source file is verbatim upstream deno_runtime 0.267.0, and upstream's
MIT license and copyright are kept unchanged (see LICENSE.md).

Unless you specifically need that patch, depend on upstream `deno_runtime`
instead.

Consumers rename it back via the dependency key, so `use deno_runtime::...`
paths keep working unchanged (the crate is versioned with the oj workspace;
the upstream base stays deno_runtime 0.267.0):

```toml
deno_runtime = { package = "oj_deno_runtime", version = "0.2.0" }
```

Upstream's README follows.

---

# `deno_runtime` crate

[![crates](https://img.shields.io/crates/v/deno_runtime.svg)](https://crates.io/crates/deno_runtime)
[![docs](https://docs.rs/deno_runtime/badge.svg)](https://docs.rs/deno_runtime)

This is a slim version of the Deno CLI which removes typescript integration and
various tooling (like lint and doc). Basically only JavaScript execution with
Deno's operating system bindings (ops).

## Stability

This crate is built using battle-tested modules that were originally in the
`deno` crate, however the API of this crate is subject to rapid and breaking
changes.

## `MainWorker`

The main API of this crate is `MainWorker`. `MainWorker` is a structure
encapsulating `deno_core::JsRuntime` with a set of ops used to implement `Deno`
namespace.

When creating a `MainWorker` implementors must call `MainWorker::bootstrap` to
prepare JS runtime for use.

`MainWorker` is highly configurable and allows to customize many of the
runtime's properties:

- module loading implementation
- error formatting
- support for source maps
- support for V8 inspector and Chrome Devtools debugger
- HTTP client user agent, CA certificate
- random number generator seed

## `Worker` Web API

`deno_runtime` comes with support for `Worker` Web API. The `Worker` API is
implemented using `WebWorker` structure.

When creating a new instance of `MainWorker` implementors must provide a
callback function that is used when creating a new instance of `Worker`.

All `WebWorker` instances are descendents of `MainWorker` which is responsible
for setting up communication with child worker. Each `WebWorker` spawns a new OS
thread that is dedicated solely to that worker.
