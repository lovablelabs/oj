# The embedded JS runtime

oj has always kept a strict boundary: everything hot is native Rust, and the workloads that genuinely need JavaScript ran in `node` sidecar processes spawned from scripts embedded in the binary. This document explains the migration that replaced those sidecars with in-process V8 isolates, embedding the Deno runtime as a Rust library, how the new architecture works, what it costs, and the contracts it preserves.

## Why

- **No `node` required.** TS/JSX, CSS (Tailwind v3/v4, Less, Stylus), Svelte, SSR dev, config extraction, dep pre-bundling, the TanStack Start path, and the plugin host all run inside the oj process. `node` on PATH is no longer a requirement of oj at all.
- **Cheaper on demand.** A sidecar used to cost a node process (tens of MB RSS, ~50-70 ms spawn plus tooling require time). An isolate costs a few MB and boots from a V8 snapshot in milliseconds. Laziness is unchanged: a plain React app that never trips a JS workload creates no isolate, and even V8 platform initialization is deferred to the first engine.
- **Simpler by deletion.** Four ad-hoc IPC protocols are gone: newline-JSON sidecar lines, the config extractor's temp-file result dance, the SSR runner's loopback HTTP + port handshake, and the named-pipe/SharedArrayBuffer/Atomics bridge that existed solely because a separate process cannot call synchronously into oj. The bridge's reconnect, orphan-watchdog, and EPIPE machinery is deleted outright, and the plugin path now works on Windows (the bridge was `mkfifo`-only).

## Architecture

### The engine crate (`oj_js`)

A `JsEngine` is a Send+Sync handle to a dedicated OS thread that owns a `deno_runtime` `MainWorker` (a V8 isolate is `!Send` and never leaves its thread; one isolate per thread, `init_platform` exactly once process-wide). Callers submit jobs over a channel and await the reply. Jobs are `eval` (run an ES module to completion) and `call` (invoke an export with JSON arguments; a returned promise is resolved before replying). A concurrent scheduler interleaves calls so requests do not serialize behind one another, and polls the event loop whenever it is not fully idle, so background work inside an isolate (an HTTP middleware server, Miniflare) keeps running between calls.

Two safety mechanisms bound every job:

- a hard V8 heap cap (near-heap-limit callback terminates the job and raises the limit so V8 unwinds instead of aborting the process), and
- a wall-clock deadline enforced by one long-lived watchdog thread per engine. Deadlines catch all three hang shapes we found the hard way: a busy loop (terminated by the watchdog), a never-settling promise (the scheduler's park is bounded by the earliest pending deadline), and a busy loop entered after the call's first await (the guard stays armed for the call's whole life). A job that dies to either limit fails alone; the isolate is un-poisoned and survives. Calls without a deadline may park forever by design (SSR semantics).

### Node compatibility

Module resolution is byonm ("bring your own node_modules"): bare specifiers resolve from the app's installed `node_modules` with Node semantics, CommonJS is translated on load, and `node:` builtins come from the CLI snapshot. Native `.node` addons work because the oj binary exports the Node-API symbol set (`deno_napi::print_linker_flags` in build.rs); `@tailwindcss/oxide`, `lightningcss`, and rolldown's binding are exercised by permanent canary tests, and esbuild's Go-binary child protocol also works in-engine. Packaging rule: never fully `strip` the binary; that removes the exported napi symbols. `strip -x` is safe.

### The ModuleHost seam

An engine can delegate module resolution and loading to a Rust host (`ModuleHost`): the host future runs on the main tokio runtime while the isolate thread parks on a plain channel (never `block_on`, so no runtime-in-runtime deadlocks). The SSR and Start runners are ModuleHost implementations over oj's own resolver and transform pipeline. Invalidation uses version-stamped specifiers (`file://...?v=N`) whose versions propagate up the importer graph: re-linking reaches fresh instances exactly along the invalidated chain while untouched subtrees keep their cached instances and state. Stale instances accumulate in the module map; after a watermark the engine is respawned transparently (snapshot boot makes this cheap).

### The plugin host

The plugin host (the one component running arbitrary user plugin code) also runs in-process, with its container/environment/middleware logic unchanged; only the stdio protocol layer was replaced. Pushes travel over an installed `__oj_post` function; hook calls are engine jobs whose return value is the reply, so plugin `console.log` can never be parsed as protocol (the old control-token framing existed for exactly that and is now unnecessary by construction); the reverse `this.resolve`/`getModuleInfo` RPC is a synchronous callback into oj's resolver and compiler. The health model is preserved verbatim: the init gate (a call before init writes nothing and waits with per-call windows, no fail-fast latch), the stall monitor, and the `host_gone` latch. **`buildStart` settles before any `load`/`transform`/`resolveId` runs**, matching Vite's ordering contract, guarded by a concurrent-first-loads regression test.

Failure semantics changed in one deliberate way: a synchronously wedged plugin hook now fails that one call at `OJ_PLUGIN_TIMEOUT` instead of killing the whole host (an upgrade; V8's `terminate_execution` can interrupt JS where a process kill was the old tool). Only a native-code block escalates to abandoning the engine (the isolate thread may leak; documented trade). A napi crash in a plugin takes the process down, which is exactly Vite's own failure model; plugin `process.env` writes and `chdir` are contained per host, and plugin stdout reaches stderr instead of being swallowed.

`@cloudflare/vite-plugin` works unchanged: Miniflare boots inside the engine, and workerd remains the child binary it always was.

### One-shot children and the rolldown lesson

Start's route-tree codegen, server-fn resolver, client rebundles, and prerender run as **one-shot child processes of the oj binary itself** (a hidden `start-script` subcommand with a fresh engine that exits when done). This is not an aesthetic choice: rolldown's binding retains native memory per `build()` invocation at process scope. We measured it under plain node too: flat JS heap under forced GC, linearly growing RSS, and V8 isolate teardown does not release it; only process exit does. The old node-spawn world was memory-flat purely because every rebundle was a throwaway process, so the fix is the same lifecycle minus node. On a large app each rebundle child peaks at parent-scale RSS for ~15 seconds and exits; the parent stays pinned. An upstream issue with the repro is being filed. A memory regression e2e (`e2e/start-memory.mjs`) asserts bounded growth across repeated rebundles.

A second, harder reason arrived with the bug hunt over production apps: **a napi-rs 3.9-era native addon cannot be registered again once every runtime that loaded it has been torn down** — its process-global state dangles and the next `napi_register_module_v1` segfaults with no output. This is not an oj defect to route around politely: plain Node dies the same way when a second `worker_thread` requires such an addon after the first worker exited, and vite 8.0.16 pins exactly such a binding (rolldown 1.0.3). Vite itself never trips this because config load and build share one process-wide registration through Node's module cache; oj's per-job isolates cannot share one, so the remaining one-shot in-process engines — config extraction and the dep optimizer's pre-bundle (`run_engine_job`) — now run in an `oj engine-job` child (payload on stdin, outcome envelope in a `--result` file, so job prints cannot corrupt the channel). One registration per process, and a crashing addon takes down a child whose death reads as "failed to load config", not the whole command. `oj_deno_napi` also warns on stderr when an addon is re-initialized after all its envs died, so any future variant of this class is a diagnosable line instead of a silent SIGSEGV. Guarded by `e2e/native-addon-reregister.mjs`.

byonm has one deliberate exception: rolldown itself. A Start app on a vite that predates rolldown (vite <= 7) resolves no rolldown from anywhere in its tree, so the Start bundles would fail outright. The nix package vendors the pinned rolldown next to the binary (`OJ_VENDORED_ROLLDOWN`, embedded at compile time and part of the runtime closure), and the Start bundle scripts prefer it at their own import sites (`importOjRolldown` in `resolve-pkg.mjs`): the Start bundles are oj's own code, written and tested against that version, and old app pins carry bindings that cannot survive re-registration (napi-rs < 3.10). The preference never reaches the app's own module-graph resolution — an app that imports `rolldown` itself keeps the copy its lockfile pins. The app's rolldown remains the fallback for oj builds that vendor nothing, `OJ_VENDORED_ROLLDOWN=` (set but empty) opts out of a build's vendor entirely, and the vendor path salts the Start bundle cache key so a vendor change never restores a bundle another rolldown built.

The same lesson covers the app's own Vite: on a runner-backed config the plugin host builds real DevEnvironments in-process, and a cold deps cache would make Vite run its dep optimizer (a rolldown build) inside oj. Before the host boots, oj pre-seeds those per-environment caches in a one-shot child (`optimize-env.mjs`): the child resolves the app's config with the app's own Vite, exactly as the host does, and drives Vite's own optimizer, so the committed metadata is hash-identical to what the in-host check expects and the host loads it instead of building. The cold/warm gate never imitates Vite's hashes; Rust only detects change since the last seed (config-extraction freshness, a lockfile stamp, a digest of each seeded metadata file), and every uncertain case runs the child, which performs Vite's exact check and exits. Honest limits: a mid-session re-optimization (a lockfile edit while serving, a discovery-mode environment registering new deps) still runs in-host, and `optimizeDeps.force` re-optimizes in-host regardless of any seed; intercepting those would mean owning Vite's optimizer scheduling. `OJ_NO_DEPS_PRESEED=1` opts out; `OJ_PRESEED_TIMEOUT` bounds the child (default 300s).

## Binary size and the release profile

V8 and the Deno runtime are statically linked: the stripped release binary goes from ~30 MB to ~98 MB. The release profile uses most of Deno's own recipe (`opt-level = "z"`, fat LTO) with `opt-level = 3` package overrides on the hot path: oj's own crates, oxc, rolldown, lightningcss, tokio/hyper, deno_core, v8, and notably the leaf `oj` bin crate itself, which under fat LTO drives the merged codegen (without that one override, warm latency and HMR regressed ~20%/11%). Benchmarked on a 1000-component app: cold start, warm request latency, and HMR save-to-paint are flat versus the old profile; prod build +4.6%; release builds take about a minute longer wall-clock from the fat-LTO link.

`panic = "abort"` is deliberately **not** set (it would save another ~10 MB): the plugin-host RPC boundary relies on `catch_unwind` so a panicking hook fails one call instead of aborting a long-lived dev server.

Deeper size reductions are blocked upstream and tracked as issues to file: `deno_runtime` has no feature gates for unused extensions (webgpu/ffi/kv/cron and friends, ~6 MB), and `deno_node` hard-depends on the swc stack for Node TS type-stripping (~4 MB). The ICU data (10 MB) lives inside the prebuilt rusty_v8 static library.

## Measured on a large real app

On an ~18k-module production app with 50+ plugins (Start bundled dev mode), full process-tree RSS versus oj 0.1.26:

| | old oj (node sidecars) | new oj |
|---|---|---|
| at ready | 5.46 GB, 6 processes | **4.57 GB, 2 processes** |
| warmed steady state | 5.57 GB | **5.12 GB** |
| across 6 HMR edits | flat | **flat (+2 MB avg/edit)** |

The six processes were oj plus the plugin host, the Start runner, two CSS sidecars, and workerd; the two are oj and workerd. During a rebundle a transient child briefly doubles tree RSS before exiting. At this scale the dominant memory is the plugin ecosystem's own working set, which weighs the same in or out of process; the per-sidecar savings are proportionally larger on small apps, where a sidecar's baseline was a meaningful fraction of the total.

## Behavior changes

- oj builds on the targets rusty_v8 ships prebuilt static libraries for (README "Supported targets"). The nix build feeds the prebuilt archive via `RUSTY_V8_ARCHIVE`; note the archive name encodes the crate's feature set (the `_simdutf` variant).
- SSR dev documents are delivered buffered rather than progressively streamed for now (an op-based streaming channel is the known follow-up); `import.meta.url` in SSR modules carries a `?v=N` query.
- Config/plugin code that mutates `process.env` stays isolated per engine (`Deno.env` writes are process-real, so every engine shadows it); extraction results no longer travel over stdout or temp files, so a config that prints cannot corrupt them.
- Request bodies to the Start handler are buffered with a cap (`OJ_START_MAX_BODY`, default 128 MiB, 413 past it) and a read error fails the request instead of delivering an empty body.
- The dep optimizer has a timeout (`OJ_OPTIMIZE_TIMEOUT`, default 120 s); it previously waited forever. Deadlines generally arm when a job starts executing, not when it is queued.
- Dependency-change tracking stamps Vite's `.env` file family deterministically (named `node:fs` import bindings are not observable under the embedded runtime; `require` and default-import consumers still are).
- Switching between a node-era and engine-era binary triggers one ssr dep re-optimize: Vite's lockfile hash folds in a patches-dir mtime, node reports sub-millisecond mtimes and the embedded runtime reports integer milliseconds, so the hash flips once and self-heals.
- Incidental fixes that fell out: `import.meta.glob` with `..` segments no longer expands empty; duplicate `define` keys no longer disable all replacements; a route-tree regeneration race; NODE_ENV leaking from build scripts; pnpm-layout Tailwind resolution everywhere; TypeScript PostCSS configs load.

## Testing

Every migrated subsystem keeps its e2e suites green (SSR incl. stack mapping and concurrency, the CSS/Svelte battery, dep-optimize, config extraction, Start dev+prod, the Cloudflare pair under real workerd) plus the full playground battery in both modes, and gains unit tests for the new seams: engine limits and all three deadline hang shapes, ModuleHost invalidation semantics, the extraction contract, napi and child-process probes, buildStart ordering under concurrent first calls, and bounded rebundle memory. One CI note: a plugin-host test wedges a hook natively via `execSync("sleep 60")`; sandboxes that deny nested shell spawns fail it spuriously.
