# oj_notify_debouncer_full

Fork of [rolldown-notify-debouncer-full](https://crates.io/crates/rolldown-notify-debouncer-full)
**0.8.1**, rolldown's publication of
[notify-debouncer-full](https://github.com/notify-rs/notify) (`notify-debouncer-full/`)
for its `rolldown-notify` fork of notify, maintained for
[oj](https://github.com/lovablelabs/oj) and carrying one change, in `src/lib.rs`
and `src/event_queues.rs`:

- The debouncer's thread sleeps while no event is queued. It woke once a
  tick, a quarter of the debounce delay by default, to look for an event old
  enough to report, whether or not one was queued: at rolldown's 10 ms
  delay, 400 wakeups a second from an idle dev server, the whole of its idle
  CPU. The thread now waits on a condition variable the watcher's callback
  signals when it queues something into an empty debouncer (and `stop`
  signals too, under the same lock, so a stop is never missed); the tick
  runs only while something is queued, as before. The watcher is created
  before the thread, so a watcher that cannot be made leaves no thread.

This is upstream's own fix, notify-rs/notify
[#933](https://github.com/notify-rs/notify/pull/933) ("use condvar for no
idle CPU usage"), released in notify-debouncer-full 0.8.0 with notify 9.0.0
(2026-11-21); rolldown's 0.8.1 publication predates it. The port keeps
upstream's names (`SharedDebounceData`, `changed`, `is_idle`) so that a
rolldown publication built on upstream 0.8.0 or later reads the same.
`is_idle` has no term for `rename_event` (upstream's
`pending_rename_destinations` does not exist in 0.8.1): a rename `From` kept
there for its `To` is also in the queues.

`oj.patch` is the whole difference from the published 0.8.1 sources
(`diff -ru` of `src/`); everything else under `src/` and `test_cases/` is
verbatim. Upstream's tests are kept (their dev-dependencies with them), and
two tests of the thread's wait are added at the end of `src/lib.rs`. Not
carried over from the published crate: its README and CHANGELOG (notify's).
Upstream's licenses (MIT OR Apache-2.0) are kept.

The library keeps upstream's name, `notify_debouncer_full`, so the sources
of [`oj_rolldown_fs_watcher`](../oj_rolldown_fs_watcher), which depend on it
in place of `rolldown-notify-debouncer-full`, are unchanged. Nothing else in
oj uses it.

- On a rolldown bump: if the new `rolldown-notify-debouncer-full` waits on a
  condition variable (`grep Condvar src/lib.rs`), drop this fork and
  `oj_rolldown_fs_watcher` with it. Otherwise replace `src/` with the new
  release's, apply `oj.patch` (`patch -p1 < oj.patch`), update the versions
  in `Cargo.toml`, and regenerate the patch.
- The crate is excluded from `make lint` with the other forks (upstream's
  sources, upstream's lints).
