# Research: `oj build --watch`

Status: **Option A implemented.** `oj build --watch` now watches with `notify`
and re-runs the one-shot build on each debounced change (client, `--ssr` and
lib; TanStack Start still rejects it). Option B (rolldown `experimental`
incremental rebuilds) remains a follow-up. The original assessment follows.
Scope: why `oj build --watch` / `build.watch` was unsupported, and what it took
to add it with Vite 8 parity.

## TL;DR

- `oj build --watch` is deliberately rejected with a clear error, and config
  `build.watch` is warned about and ignored. There is no watch code on the build
  path at all.
- Vite 8 `build --watch` is *not* Vite code we can mirror line-for-line: it
  delegates to the **native rolldown `watch()` API** (`rolldown.watch()` from
  the JS package, backed by the NAPI `BindingWatcher`). That API is **not
  published in the `rolldown` Rust crate** oj depends on — the crate exposes
  only `Bundler::write/generate`, `watch_files()`, and (behind the
  `experimental` feature) `incremental_write/generate` + HMR computation.
- Feasibility is **good but not a drop-in**. Recommended path: replace the
  `bail!` with a watch loop that re-runs the existing one-shot build on
  `notify` events (correct, low risk, slower), then optionally upgrade the hot
  client bundle to rolldown's incremental Rust API for speed. A native watcher
  with true Vite event semantics would mean reimplementing the small
  orchestrator around `notify` + `watch_files()` + incremental builds.

## Implemented (Option A)

- New `crates/oj/src/build_watch.rs`: parses the `build.watch` chokidar subset
  (`ignored`, `usePolling`, `interval`, `buildDelay`), filters events against
  outDir + chokidar's fixed skips + the user globs, debounces a burst into one
  rebuild, and loops forever. It mirrors the dev watcher's top-level-entry watch
  (never recursing into `node_modules`/`.git`/`.oj-cache`).
- `crates/oj/src/build.rs`: `build()` is now a thin wrapper over the old body
  (renamed `build_once`, returning the resolved watch options and outDir); it
  runs one fail-fast build, then hands a `build_once` closure to
  `build_watch::run`. `--ssr` and lib go through the same entry point and are
  covered; a failed rebuild is logged and watching continues.
- `--watch` is accepted, `build.watch` no longer warns, and TanStack Start
  rejects `--watch` with a clear message (it has its own build entry point).
- Tests: unit tests for option parsing/ignore matching; e2e `build-watch.mjs`
  (source/CSS/HTML edits trigger rebuilds, outDir writes do not, SIGTERM stops)
  and updated `build-cli-flags.mjs`.

The watch set is the **whole project root** (minus the ignores), not just the
last bundle's module graph. That is noisier than Vite (an unrelated file under
the root forces a rebuild) but needs no state threaded out of the bundler and
guarantees a new file anywhere can be picked up. Tightening it to
`Bundler::watch_files()` is a possible follow-up.

## 1. Current state (why it was unsupported)

### CLI

`crates/oj/src/main.rs:162-164`:

```rust
/// Rebuild on changes (Vite's -w); not supported by oj yet.
#[arg(short = 'w', long)]
watch: bool,
```

The flag is parsed and threaded into `build::CliOptions.watch`
(`crates/oj/src/main.rs:625`, `crates/oj/src/build.rs:2380`), then the very
first thing `build()` does is reject it (`crates/oj/src/build.rs:2486-2488`):

```rust
if cli.watch {
    bail!("oj build --watch is not supported yet; run `oj build` again after changes, or use `oj dev`");
}
```

Config `build.watch` is accepted in the schema only so the build can warn
(`crates/oj_config/src/schema.rs:429-430`):

```rust
/// Vite's `build.watch`; not supported by oj (warned about, ignored).
pub watch: Option<serde_json::Value>,
```

and `warn_unsupported_build_options` prints the same "not supported" hint
(`crates/oj/src/build.rs:2456-2465`).

### Tests encode the current contract

`e2e/build-cli-flags.mjs:194-198` asserts `--watch` exits non-zero with
`/--watch is not supported/`; `:201-207` asserts `build.watch` warns. Any
implementation must update both.

### Why there is no watch today

There is no watcher on the build path. The only watcher in the workspace lives
in the **dev server** (`crates/oj_server/src/watch.rs`, built on `notify`), and
it is coupled to `ServerState` (`spawn_watcher` is `pub(crate)` and takes an
`Arc<ServerState>`). `build()` is a single one-shot async function
(`crates/oj/src/build.rs:2482-3311`) that:

1. loads and re-adopts the config (possibly twice, for a config-named mode),
2. builds the entry list from `index.html`/`rollupOptions.input`,
3. creates a plugin host and the `vite:css`/asset/worker plugins,
4. runs **one** rolldown `Bundler::write()`,
5. post-processes the bundle: split CSS, `__vitePreload` wrapping, HTML
   rewriting, manifests, `publicDir` copy, size report,
6. closes the plugin host (`closeBundle`).

Nothing in that pipeline is resumable, so there is no "rebuild" step to call.

## 2. How Vite 8 does it

Vite 8's `build --watch` is implemented in `buildEnvironment`
(vite `dist/node/chunks/node.js`):

```js
if (options.watch) {
  logger.info(`\nwatching for file changes...`);
  // resolveChokidarOptions merges rolldownOptions.watch.chokidar and
  // options.watch.chokidar, adds ignoreInitial/ignorePermissionErrors and
  // ignores .git/node_modules/test-results/cacheDir/(outDir when emptyOutDir)
  const { watch } = await import("rolldown");
  const watcher = watch({
    ...rolldownOptions,
    watch: { ...rolldownOptions.watch, ...options.watch,
             watcher: convertToWatcherOptions(resolvedChokidarOptions) },
  });
  watcher.on("event", (event) => {
    if (event.code === "BUNDLE_START") { logger.info(`\nbuild started...`); }
    else if (event.code === "BUNDLE_END") { event.result.close(); logger.info(`built in ${event.duration}ms.`); }
    else if (event.code === "ERROR") { /* enhanceRollupError + logger.error */ }
  });
  return watcher;
}
```

Key facts:

- It is **per environment**. `createBuilder(...).buildApp()` calls
  `builder.build(environment)` for each configured environment; watch mode
  returns one `RolldownWatcher` per environment. The CLI action finishes, and
  the live watchers keep the Node event loop alive.
- `rolldown.watch()` is the Rollup-compatible JS API. It is implemented by the
  native `BindingWatcher` (`rolldown/dist/shared/watch-*.mjs`), emitting
  `BUNDLE_START` / `BUNDLE_END` / `ERROR` bundle events plus `change`,
  `restart`, `close`.
- Options honoured: Rollup `build.watch` (`chokidar`, `buildDelay`,
  `skipWrite`, `clearScreen`, `include`/`exclude`, ...). Vite additionally
  merges `rolldownOptions.watch` and its own `build.watch`, and maps chokidar's
  `usePolling`/`interval` to rolldown watcher options
  (`convertToWatcherOptions`).
- Output is written by the watcher on each successful bundle. Vite does **not**
  re-run HTML post-processing outside the rolldown plugins; `vite:build-html`
  is a plugin inside the same watched bundle.

## 3. What rolldown offers the Rust side (the real gap)

`rolldown = "=1.2.5"` (no `experimental` feature; `Cargo.toml:29`). Inspecting
the crate:

| Capability | Rust availability |
| --- | --- |
| `Bundler::write/generate/scan/close` | yes (`impl_bundler_build.rs`) |
| `Bundler::watch_files() -> &Arc<FxDashSet<ArcStr>>` | yes (`impl_bundler_getter.rs`) |
| `incremental_write/generate(ScanMode)` | **only `#[cfg(feature = "experimental")]`** (`impl_bundler_incremental_build.rs`) |
| HMR change computation (`compute_hmr_update_for_file_changes`, `compile_lazy_entry`) | **only `experimental`** (`impl_bundler_hmr.rs`) |
| `Watcher` / `BindingWatcher` / `watch()` | **not present**; lives in the NAPI binding / JS package |

So the exact engine Vite drives (native watcher + per-bundle event stream) is
not reachable from oj's Rust build today. Anything native means either:

- enabling `rolldown/experimental` and writing our own `notify`-driven loop
  over `watch_files()` + `incremental_write(ScanMode::Partial(changed))`, or
- driving `rolldown.watch()` through the existing Node sidecar (the plugin
  host already runs JS), which duplicates config serialization and still does
  not cover oj's custom emit pipeline (HTML rewriting, split CSS, manifests).

## 4. Options for oj

### Option A — Re-run the one-shot build on every change (recommended first step)

Keep `build()` as-is and wrap it:

```
oj build --watch:
  build() once                      # initial bundle
  loop:
    wait for a debounced notify batch over the watch set
    build() again (full)
```

- **Pros:** minimal new surface; reuses every existing code path (config
  reload, plugin host, HTML/CSS/manifest emit); matches Vite's observable
  semantics closely enough (each rebuild is a full, correct bundle). Handles
  config changes for free by reloading config inside `build()`.
- **Cons:** slower than Vite's incremental watcher on large apps; plugin host
  is rebuilt each cycle. Watch set must be derived after the first build
  (source files in the module graph; `rolldown` gives `watch_files()` per
  bundler, but the one-shot path currently drops the `Bundler`).
- **Effort:** small-to-moderate. Main work is a `notify` wrapper + a watch-set
  source + CLI/plumbing + tests.

### Option B — Incremental rebuild via rolldown `experimental` (fast path)

Persist the `Bundler`s across cycles, collect `watch_files()`, and on a change
call `incremental_write(ScanMode::Partial(changed))`.

- **Pros:** close to Vite's speed; rolldown keeps the module graph.
- **Cons:** requires enabling an explicitly experimental feature; the build has
  several bundlers per app (client, SSR, server fns, inline workers) and
  oj's post-processing/emit state must be made rebuildable; larger risk.
- **Effort:** large; should follow A.

### Option C — Drive JS `rolldown.watch()` from the sidecar

- **Pros:** closest to Vite's engine and event semantics.
- **Cons:** would have to serialize the whole rolldown option graph plus JS
  plugins, build per environment, and then still emit oj's hashed HTML/CSS and
  manifests outside the watcher — effectively two sources of truth for output.
  Does not obviously reduce total work.
- **Verdict:** not recommended.

### Recommended shape

Phase 1 (ship semantics): Option A, with a `notify`-based loop in a new
`crates/oj/src/build_watch.rs` (or in `oj_server` as a reusable primitive).
Expose Vite-compatible `build.watch` options minimally: at first accept `{}` /
`true`; then `include`/`exclude`, `ignored` (chokidar-compatible subset),
`usePolling`/`interval`, and `buildDelay`/debounce. Watch the set of files the
last build actually consumed (from `Bundler::watch_files()` where available,
plus config/`.env`/tsconfig/PostCSS/Tailwind/HTML inputs).

Phase 2 (performance, optional): Option B for the client bundle only; keep
full rebuilds for SSR/server-fn/link-CSS steps.

## 5. Design details and hazards

- **Initial build then watch.** Vite builds once, then watches; oj should do the
  same and print the same `watching for file changes...` line.
- **Emptying `outDir`.** `prepare_out_dir` empties `outDir` at the start of
  `build()` (under Vite's containment rules). On every rebuild this would wipe
  the previous output before writing the new one, briefly leaving a broken
  `dist/`; it also risks delete/recreate churn. Decide: keep the same rule
  Vite uses (it does empty on each environment build on first build only —
  `emptyOutDir` in Vite is a build-start concern) and skip re-emptying on
  rebuilds, or make it explicit.
- **Self-triggering loops.** Ignore `outDir`, `node_modules`, `.git`, cache
  dir, and the generated route tree (Start), exactly as Vite's
  `resolveChokidarOptions` list does. Be careful with atomic-save tricks
  (rename + write) and with macOS FSEvents per-path restarts (see the dev
  watcher's notes in `crates/oj_server/src/watch.rs`).
- **Debounce/coalesce.** One editor save can produce several events; coalesce
  with a short debounce (`buildDelay`, default a few ms) and drop events that
  arrive while a rebuild runs, then run one more if anything landed.
- **Config and plugin-host lifecycle.** `build()` already reloads config and
  creates/closes the plugin host per call. Re-running `build()` gets config
  changes for free. If Phase 2 persists bundlers/plugin hosts, `configResolved`
  / `configureServer` / `closeBundle` lifecycles must be re-thought; the dev
  watcher already restarts on config/`.env` because those change server
  identity.
- **Multiple entry points and outputs.** `build()` covers client-only apps;
  `build_ssr_app` (SSR), `build_library` (lib), and
  `start_dev::start_build` (Start) are separate top-level paths. Vite watches
  each environment independently; oj should at minimum reject or explicitly
  handle `--watch` for `--ssr` and lib with a clear message until covered.
- **Exit codes and signals.** Watch mode runs until SIGINT/SIGTERM; errors
  should be logged and the watcher kept alive (Vite's behavior), not exit.
  Ctrl-C must close watchers/plugin hosts cleanly.
- **stdout contract.** Today `build()` prints `... build: <dir> in <duration>`
  plus the size report and chunk-size warning. Watch mode should not reprint
  the whole report in a way that breaks scripts; follow Vite's
  `build started...` / `built in Nms.` lines but keep them on stderr or behind
  the existing reporter, and never fail the loop on a single bad rebuild.
- **`write: false` / `skipWrite`.** Already unsupported in config; watch should
  keep rejecting/normalizing consistently.

## 6. Test plan

Unit (`crates/oj/src/build.rs` / new module):

- watch-set assembly from `Bundler::watch_files()` + config/html inputs;
- chokidar-option normalization and ignored-path matching;
- debounce/coalesce logic with synthetic event batches;
- "rebuild requested while running" schedules exactly one follow-up.

E2E (`e2e/build-cli-flags.mjs` and a new `e2e/build-watch.mjs`):

- update the existing `--watch` assertions: the flag is no longer a hard error;
- start `oj build --watch` on a fixture, wait for the initial `dist/`, edit a
  source file, assert `dist/` is rewritten with the change (hash and content);
- edit an HTML file and assert the page is re-emitted;
- assert editing `outDir` output does **not** trigger a rebuild;
- edit the vite config and assert the rebuild picks it up (full-reload
  semantics);
- assert SIGINT exits 0 and leaves no child processes (reuse the dev-server
  teardown checks);
- config `build.watch: {}` no longer warns.

## 7. Recommendation

Ship **Option A** first: a `notify`-based watch loop that re-runs the existing
`build()` pipeline, with Vite-compatible `build.watch` option parsing and the
ignore/debounce rules above. It delivers correct Vite-equivalent output
ordering and all of oj's emit behavior with small, low-risk changes to
`build.rs`/`main.rs`, and it makes the existing "not supported" error and e2e
expectations obsolete. Treat **Option B** (rolldown `experimental` incremental
rebuilds) as a follow-up performance project, not a prerequisite.
