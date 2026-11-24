# oj_rolldown_fs_watcher

Fork of [rolldown_fs_watcher](https://crates.io/crates/rolldown_fs_watcher)
**1.3.2** (source: [rolldown/rolldown](https://github.com/rolldown/rolldown)
`67e2203c`, `crates/rolldown_fs_watcher`), maintained for
[oj](https://github.com/lovablelabs/oj). Its sources under `src/` are
verbatim upstream; the one change is in `Cargo.toml`: the event debouncer is
[`oj_notify_debouncer_full`](../oj_notify_debouncer_full), oj's fork of
`rolldown-notify-debouncer-full` 0.8.1, in place of that crate. The fork's
library keeps upstream's name (`notify_debouncer_full`), so the `use` paths
are unchanged.

Why: the debouncer's thread woke once a tick, a quarter of the debounce
delay, whether or not an event was queued. At rolldown's default 10 ms delay
that is 400 wakeups a second from an idle dev server, the whole of its idle
CPU. The fork's thread sleeps until the watcher queues an event. See the
fork's README for the change.

oj depends on this crate wherever upstream code names `rolldown_fs_watcher`
(oj itself and `oj_rolldown_dev`), renamed back through the dependency key:

```toml
rolldown_fs_watcher = { package = "oj_rolldown_fs_watcher", version = "0.4" }
```

- On a rolldown bump: replace `src/` with the new release's and update the
  `=1.x.y` versions in `Cargo.toml`; nothing else to carry.
- Drop the fork once rolldown's debouncer sleeps while idle (upstream
  rolldown_fs_watcher then needs no change).
- Not carried over from the published crate: its README (the rolldown
  repository's). `rustfmt.toml` and `LICENSE` are the upstream repository's
  (MIT).
