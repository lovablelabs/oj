# oj_deno_napi

Fork of [deno_napi](https://crates.io/crates/deno_napi) **0.190.0**
(source: [denoland/deno](https://github.com/denoland/deno), `ext/napi`),
maintained for [oj](https://github.com/lovablelabs/oj) and carrying oj's
performance patch:

- `RefTracker::pending` (the napi finalizer registry) is keyed by finalizer id
  in a `BTreeMap` instead of being a `Vec`, so deregistering a reference from a
  GC weak callback is O(log n) instead of a linear scan + shift. With a large
  addon working set the Vec form is quadratic across a GC cycle under heavy
  napi ref counts.

Everything else is verbatim upstream deno_napi 0.190.0, plus a drain-order
unit test for the patched registry. Upstream's MIT license and copyright are
kept unchanged (see LICENSE.md).

Unless you specifically need this patch, depend on upstream `deno_napi`
instead.

Consumers rename it back via the dependency key, so `use deno_napi::...`
paths keep working unchanged (the crate is versioned with the oj workspace;
the upstream base stays deno_napi 0.190.0):

```toml
deno_napi = { package = "oj_deno_napi", version = "0.2.0" }
```
