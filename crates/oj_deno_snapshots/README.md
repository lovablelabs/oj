# oj_deno_snapshots

Fork of [`deno_snapshots`](https://crates.io/crates/deno_snapshots) 0.74.0
(from `denoland/deno`, `cli/snapshot`) maintained for
[oj](https://github.com/lovablelabs/oj). `lib.rs` and `shared.rs` are verbatim
upstream. The one change is in `build.rs`: when `OJ_SNAPSHOT_ARCHIVE` points
at a prebuilt bundle (this build script's own outputs, harvested per target by
`tools/gen-snapshot-pin.sh`), the V8 snapshot is taken from the bundle instead
of being created at build time.

Why: V8 snapshot creation is not run-deterministic
([lovablelabs/oj#211](https://github.com/lovablelabs/oj/issues/211)), which
breaks bit-reproducible builds. Pinning the blob per target takes the
nondeterministic step out of the build; a manifest check turns a stale pin
(after a deno_runtime / deno_core / rusty_v8 bump) into a readable build
error. Without the env var this crate behaves exactly like upstream.

Use upstream `deno_snapshots` unless you need this.
