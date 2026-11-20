# oj_rolldown_dev

Fork of [rolldown_dev](https://crates.io/crates/rolldown_dev) **1.3.2**
(source: [rolldown/rolldown](https://github.com/rolldown/rolldown)
`67e2203c`, `crates/rolldown_dev`), maintained for
[oj](https://github.com/lovablelabs/oj) and carrying one change, in
`src/dev_engine.rs`:

- `DevEngine::compile_lazy_entry` queues its rebuild only when rolldown's own
  lazy compilation is on (`experimental.devMode.lazy`), or when the engine's
  last task errored.

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

The rebuild stays after a task that errored. A module that task could not
load (a syntax error mid-edit) is not in the graph's cache, so a compile made
once the file is fixed, before the engine has handled the fix itself, loads
it: that compile does change the graph, and the rebuild also ends the errored
state before the fix's own update is computed.

`oj.patch` is the whole difference from the published 1.3.2 sources
(`diff -ru` of `src/`); everything else under `src/` is verbatim. Not
carried over from the published crate: `examples/` and the dev-dependencies
only it used. `rustfmt.toml` and `LICENSE` are the upstream repository's (MIT).

## Updating and dropping the fork

- On a rolldown bump: replace `src/` with the new release's, apply `oj.patch`
  (`patch -p1 < oj.patch`), update the versions in `Cargo.toml`, and
  regenerate the patch.
- Drop the fork once a rolldown_dev release can skip the rebuild (an option,
  or a check that the compile changed nothing): delete this directory and its
  workspace member entry, point the workspace's `rolldown_dev` dependency
  back at the registry (`rolldown_dev = "=<version>"`), and remove the
  `--exclude oj_rolldown_dev` in the Makefile. The test
  `compiling_a_module_for_an_import_starts_no_build` (crates/oj) says whether
  the upstream release does what the fork did.

Consumers rename it back via the dependency key, so `use rolldown_dev::...`
paths keep working unchanged (the crate is versioned with the oj workspace;
the upstream base stays rolldown_dev 1.3.2):

```toml
rolldown_dev = { package = "oj_rolldown_dev", version = "0.4" }
```

Unless you need this change, depend on upstream `rolldown_dev` instead.
