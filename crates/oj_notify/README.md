# oj_notify

Fork of [notify](https://crates.io/crates/notify) **9.0.0-rc.6**
(source: [notify-rs/notify](https://github.com/notify-rs/notify), `notify/`),
maintained for [oj](https://github.com/lovablelabs/oj) and carrying one
unreleased upstream fix:

- `src/fsevent.rs` is taken from notify-rs/notify
  [`636af549`](https://github.com/notify-rs/notify/commit/636af5497b1e15240c14653bc149cba936bc3fc1)
  ("fix(fsevent): preserve pending events across watch changes",
  [#1033](https://github.com/notify-rs/notify/pull/1033)): when a watch
  restarts the FSEvents stream, events queued on the old stream are no
  longer dropped.

Between rc.6 and that commit this is the only change to the notify crate's
sources; everything else is verbatim upstream rc.6. Upstream's CC0-1.0
license is kept (see LICENSE-CC0). The fork goes away once a notify release
carries the fix.

Unless you specifically need this fix before it is released, depend on
upstream `notify` instead.

Consumers rename it back via the dependency key, so `use notify::...` paths
keep working unchanged (the crate is versioned with the oj workspace; the
upstream base stays notify 9.0.0-rc.6):

```toml
notify = { package = "oj_notify", version = "0.2" }
```
