# oj_rolldown_dev

Fork of [rolldown_dev](https://crates.io/crates/rolldown_dev) **1.3.2**
(source: [rolldown/rolldown](https://github.com/rolldown/rolldown)
`67e2203c`, `crates/rolldown_dev`), maintained for
[oj](https://github.com/lovablelabs/oj) and carrying these changes, in
`src/dev_engine.rs`:

- `DevEngine::compile_lazy_entry` queues its rebuild only when rolldown's own
  lazy compilation is on (`experimental.devMode.lazy`).
- `DevEngine::register_client_without_output` registers a client whose
  session starts with nothing held (see below).
- `DevEngine::forget_client_output` drops the frozen output of a client
  that was registered before oj could tell what it loaded (see below).
- `DevEngine::forget_client_modules` drops modules a client reports it has
  no factory for from its frozen output and its ship map, after a lazy chunk
  left them out as copies it holds (see below).
- `DevEngine::resend_files` has files handled as changed with nothing
  written (see the end).

Upstream queues a rebuild after every lazy compile. With rolldown's lazy
compilation, a module behind an `import()` stays out of the graph until a
page asks for it; the compile merges it in, so the build output no longer
matches the graph and the rebuild brings it in line. oj turns that plugin off
and declares the lazy boundaries with a plugin of its own, whose proxy
modules always import their targets: every module is in the graph from the
first build, and a compile only renders modules that are already there. It
loads no new file and changes no output, so the rebuild it queued produced
the same output again, and the next `import()` or edit waited for it (about
0.3 s to 0.5 s on a 2,100-module app). The record of what the compile sent to
the page and the assets it emitted are handled before that point and are
unchanged.

One case is left to the host. A module the engine's last task could not load
(a syntax error mid-edit) is not in the graph's cache, so a compile made once
the file is fixed, before the engine has handled the fix itself, would load
it and change the graph with no update made for it. oj asks for no compile
while `DevEngine::get_bundle_state` reports `last_build_errored`.

`register_client` freezes into a client's session the modules the engine's
newest output evaluates as it loads, on the premise that the client loaded
that output (upstream notes the gap: "A client that loaded an output older
than the latest rebuild gets the newer map ... the reload fallback covers
that window until the hello carries a build id"). oj knows which pages did
not: one on a bundle restored from its cache or made by an engine before a
restart, and one behind a rebuild or a hot update the editor's gate holds.
For those it calls `register_client_without_output`, which creates the
session with empty maps: every patch and lazy chunk then carries each factory
the client may lack, and the delivery notifications record what it holds, as
for any client. A request can also come before oj can tell what its page
loaded (the page names what it has only once its live-reload client ran): it
is registered with `register_client`, and if the page then turns out to be
behind, `forget_client_output` empties the frozen map of its session and
keeps the record of what was delivered. Only such a session is emptied: one
frozen for a page that had loaded the output stays right when the page falls
behind later, since what a later change adds is not in it. Either map can
still name a module the page never got (a patch it was behind, a module a
change moved into the entry's graph, a session made anew after a change),
and only the page knows what it ran: when a lazy chunk compiled for it refers
to modules it has no factory for, the page asks again naming them, and oj
calls `forget_client_modules`, which drops those modules from a copy of the
frozen map and from the ship map, so the next compile carries them; the page
names in turn what those import and it lacks. Drop the three once
`register_client` takes the build a client loaded.

`oj.patch` is the whole difference from the published 1.3.2 sources
(`diff -ru` of `src/`); everything else under `src/` is verbatim. Not
carried over from the published crate: `examples/` and the dev-dependencies
only it used. `rustfmt.toml` and `LICENSE` are the upstream repository's (MIT).

## Updating and dropping the fork

- On a rolldown bump: replace `src/` with the new release's, apply `oj.patch`
  (`patch -p1 < oj.patch`), update the versions in `Cargo.toml`, and
  regenerate the patch.
- Drop the fork once a rolldown_dev release can skip the rebuild (an option,
  or a check that the compile changed nothing) and can register a client by
  the build it loaded: delete this directory and its
  workspace member entry, point the workspace's `rolldown_dev` dependency
  back at the registry (`rolldown_dev = "=<version>"`), and remove the
  `--exclude oj_rolldown_dev` in the Makefile. The test
  `compiling_a_module_for_an_import_starts_no_build` (crates/oj) says whether
  the upstream release does what the fork did.

Consumers rename it back via the dependency key, so `use rolldown_dev::...`
paths keep working unchanged (the crate is versioned with the oj workspace;
the upstream base stays rolldown_dev 1.3.2):

```toml
rolldown_dev = { package = "oj_rolldown_dev", version = "0.4.17" }
```

Unless you need this change, depend on upstream `rolldown_dev` instead.

`resend_files` queues the update a watcher event for the files would. A
module that calls `import.meta.hot.invalidate()` hands its update to its
importers, which the client runs again from the factories it holds. It holds
none for a module of the output it first loaded (only a patch or a lazy
chunk registers one), so the host asks for those importers here and they
reach the client as any update does. Vite's dev server answers
`vite:invalidate` the same way, with an update for the importers
(server/environment.ts `invalidateModule`).
