<!-- LOGO -->
<h1>
<p align="center">
  <img src="/assets/OJ-spill.png" alt="OJ logo" width="256">
  <br>OJ
</p>
</h1>

Experimental next generation frontend tooling.

It optimizes for memory and cold start, where running many builds (CI, agents, multi-tenant) under Vite gets expensive. OJ is meant to run real production React apps without changes to their source.

## Server rendering

There is an SSR mode: `oj dev --ssr src/entry-server.tsx` in dev, and `oj build --ssr` for production. It streams the HTML out with `renderToReadableStream` instead of buffering, and the client hydrates through
the normal dev pipeline, so Fast Refresh and HMR keep working over a server-rendered page.

In dev the server modules run in a small persistent Node process (a module runner using `vm.SourceTextModule`) that re-evaluates only what changed, instead of rebuilding a bundle per request.

## Plugins

Vite/Rollup-style plugins run through a persistent Node plugin host. Drop an `oj.plugins.mjs` at the app root that default-exports a plugin array, or let oj read an app's `vite.config.{ts,js,mjs}` and pick up its `plugins` array directly.

A TypeScript config (including one that imports local `.ts` files) is loaded via Vite's own config loader when Vite is installed, or bundled with the app's esbuild otherwise. From a `vite.config` oj also adopts the app's `base`, `server.port`/`host`, `define`, and `resolve.alias` for any field its own config
leaves unset, alias entries resolve alongside tsconfig `paths`, in both `oj dev` and `oj build`.

## TanStack Start

Point oj at a TanStack Start app and it runs the framework directly, no source changes. It detects Start from the app itself:

```sh
oj dev web                                       # dev server for the ./web Start app
oj build web                                     # production build
```

Dev serves file-based routes with streaming SSR and client hydration, keeps Fast Refresh working, runs server functions in the module runner, and applies the app's `vite.config` plugins (React and the app's own). `oj build` emits a Node `server.mjs`, a Cloudflare `worker.mjs` (`nodejs_compat`), content-hashed client assets, and prerendered routes. oj's own docs site (`www/`) is a TanStack Start app built by oj and served from a Cloudflare Worker.

## Type checking

`oj typecheck` checks an app the way `tsc -b --noEmit` does, on [tsrs](https://github.com/oxc-project/tsrs) (the Rust port of the TypeScript 7 compiler, built into oj). It prints tsc's `file(line,col): error TSn: message` lines and exits 0 when clean, 2 on type errors and 1 when it cannot check.

```sh
oj dev web --typecheck                           # dev server that keeps a checker warm for ./web
oj typecheck web                                 # asks that server; checks in its own process when none runs
oj typecheck web --changed src/App.tsx           # names a file just written, so the answer covers it
oj typecheck web --json                          # the report as JSON
oj typecheck -p web/tsconfig.json --noEmit      # tsc's own command line, output and exit codes
```

With `--project` (`-p`) or `--build` (`-b`), `oj typecheck` takes the arguments `tsc` (or `tsgo`) takes and prints and exits as it does, so a build that calls tsgo can call `oj typecheck` in its place.

How it works:

- `oj dev --typecheck` starts a checker in a child `oj` process beside the dev server, so a crash or hang in it never takes the server down; the next check starts a new one. It opens the projects `tsc -b` would build from the root `tsconfig.json` once the server has served its first page and been quiet for a second (sooner when a check is requested), so it never competes with that page, and the dev server's watcher forwards every change to it. Once changes settle it checks again, so a request usually finds a current report.
- The server writes its address (app root, pid and port) to `.oj-cache/v1/dev-server.json`. `oj typecheck <dir>` reads it and posts to the server's `/@oj/typecheck`, which answers only local callers (not browsers or the preview proxy). Files named with `--changed` are re-read before the answer, so it never waits on the watcher. When no live server is advertised for that root, `oj typecheck` checks in its own process.
- After an edit, only the files it affects are checked again, as `tsc --watch` does: each project keeps an incremental program whose per-file results carry over to the next one. An edit that leaves a file's exported types unchanged re-checks only that file. An edit that changes them also re-checks every file that imports it, directly or not, which can be most of an app for a widely shared module.
- That needs each file's declaration signature, which the checker records in the background once it has heard nothing (no change, no check) for a second after a check; a burst of edits records once, after it. A check that arrives while it records stops it before the next file. It leaves out the files whose declaration emit is slow: the TanStack Router route tree (`tsr.config.json`'s `generatedRouteTree`, `src/routeTree.gen.ts` by default), whose one declaration types every route, and any file it once took a second or more to emit. A change to one of those re-checks the files that import it.
- An edit to a tsconfig, `package.json` or a lockfile opens the projects again from scratch.

## Quickstart

Install the CLI from crates.io, then run `oj` in any app:

```sh
cargo install oj --locked                       # install the `oj` CLI
oj dev                                           # dev server for the current app on :5199
oj build                                         # production build into ./dist
```

Or with [Homebrew](https://formulae.brew.sh/formula/lovable-oj):

```sh
brew install lovable-oj
```

Or with Nix:

```sh
nix run github:lovablelabs/oj -- dev
```

### Supported targets

oj embeds V8, so it builds on the targets rusty_v8 ships prebuilt static
libs for: Linux glibc x64/arm64, Linux musl x64/arm64, macOS x64/arm64, and
Windows MSVC x64/arm64.

## Benchmarks

Generated fanout-10 React component trees, measured save-to-paint with Playwright against Vite 8.3.5 (Rolldown-based) on an Apple M5 Max, in both its default dev mode (vite) and its experimental bundled dev mode (vite-fbm). p50/p95 over 5 cold+warm restart cycles and 10 HMR edits. Measured with oj 0.2.46.

Memory is measured over the whole process tree, and reported two ways: resident set (tree RSS) and macOS physical footprint, which excludes clean and reclaimable resident pages and so bounds retained memory. Forced-GC RSS is not a column because it cannot be measured symmetrically (Node exposes an external GC handle through its inspector; embedded V8 does not) — for reference, forcing GC before measuring lowered vite's tree RSS by roughly a third in our runs, still well above its footprint. Methodology corrections owe to [#202](https://github.com/lovablelabs/oj/issues/202).

**1,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | tree RSS | footprint |
|---|---|---|---|---|---|---|
| **oj** | **466/558ms** | **321/327ms** | **166/168ms** | **57/59ms** | **180MB** | **136MB** |
| vite | 641/648ms | 600/606ms | 167/167ms | 38/76ms | 398MB | 338MB |
| vite-fbm | 317/356ms | 317/323ms | 47/48ms | 56/59ms | 357MB | 306MB |

**5,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | tree RSS | footprint |
|---|---|---|---|---|---|---|
| **oj** | **1280/1451ms** | **1024/1038ms** | **722/726ms** | **61/62ms** | **398MB** | **146MB** |
| vite | 2312/2321ms | 2133/2153ms | 710/713ms | 156/158ms | 848MB | 731MB |
| vite-fbm | 989/995ms | 998/1000ms | 177/179ms | 60/67ms | 978MB | 931MB |

**10,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | tree RSS | footprint |
|---|---|---|---|---|---|---|
| **oj** | **2380/2395ms** | **1929/1942ms** | **1431/1452ms** | **66/175ms** | **684MB** | **201MB** |
| vite | 4554/4559ms | 4163/4185ms | 1383/1407ms | 152/157ms | 1414MB | 1331MB |
| vite-fbm | 1379/1381ms | 1373/1378ms | 266/273ms | 67/72ms | 1740MB | 1741MB |

Against Vite's default dev, oj starts faster at every size: cold 1.4x at 1k to 1.9x at 10k, warm 1.9x to 2.2x. Reload is level with it. Vite's experimental bundled dev starts faster than oj and reloads far faster, because it serves one bundle instead of thousands of modules. HMR matches bundled dev at every size, and beats default dev from 5k up (default dev is faster at 1k). Memory is oj's largest and most consistent win: a 136-201MB footprint against Vite's 306MB-1.7GB, from 2.5x less than default dev at 1k to 6.6x less at 10k, measured over whole process trees.

Production builds (`oj build` vs `vite build`) produce the same output on the 10,000-component app. `oj build` is currently slower there: 697ms against Vite's 476ms with oj's cache cleared before each build, 523ms against 479ms primed (median of 7 interleaved runs).

## Reference reading

- [oxc_transformer/examples/transformer.rs](https://github.com/oxc-project/oxc/blob/main/crates/oxc_transformer/examples/transformer.rs): the pipeline oj's compiler is based on
- [oxc_transformer/src/jsx](https://github.com/oxc-project/oxc/tree/main/crates/oxc_transformer/src/jsx): JSX + ReactRefresh transform internals
- [vitejs/vite-plugin-react](https://github.com/vitejs/vite-plugin-react): the Fast Refresh glue semantics oj replicates
- [vite/packages/vite/src/node/server](https://github.com/vitejs/vite/tree/main/packages/vite/src/node/server): HMR propagation, `import.meta.hot` protocol
- [rolldown/rolldown](https://github.com/rolldown/rolldown): plugin hook filters, the prod linker oj embeds

## Misc

```sh
cargo run -p oj -- dev                            # dev server for ./playground on :5199
cargo run -p oj -- dev --ssr src/entry-server.tsx # streaming SSR + hydration
cargo run -p oj -- build playground               # production build into playground/dist
cargo test --workspace                            # rust unit tests
node --test e2e/unit/*.test.mjs                   # js unit tests (adapter helpers)
node e2e/run.mjs                                  # browser e2e suite
node e2e/ssr-dev.mjs                              # SSR dev e2e, e2e/ssr-prod.mjs for the built server
cargo test -p oj --test start                     # tanstack start integration (see e2e/fixtures/start-app)
node e2e/dep-optimize.mjs                         # dependency pre-bundle + cjs interop integration
cargo test -p oj --bin oj plugin_api_tests       # build plugin API, rollupOptions, ssr, css and assetsDir options (in-process)
cargo test -p oj_server --test dev                # dev server config, css, plugin hooks, ws and HMR sockets (in-process)
node e2e/config-proxy.mjs                         # adopt vite.config server.proxy + ignored-config warnings
node e2e/proxy-regex-context.mjs                  # server.proxy "^..." regex contexts (path + query), like Vite
node e2e/hmr-overlay-recovery.mjs                 # startup compile error: buffered overlay, reload on first update
node e2e/hmr-json-update.mjs                      # editing an imported .json hot-updates its boundary (?t= stamp)
node e2e/hmr-unresolved-import.mjs                # missing ./import fails with an overlay; creating the file recovers
node e2e/host-binding.mjs                         # dev/preview --host + server.host bind all interfaces
node e2e/awkward-paths.mjs                         # percent-encoded filenames served, traversal contained
node bench/generate.mjs 1000                      # generate a benchmark app (then npm i inside it)
node bench/run.mjs 1000                           # p50/p95 benchmark vs vite
node bench/card.mjs                               # render bench/card.html to oj-benchmarks.png
```

## Testing

```sh
cargo test --workspace                            # unit + integration suites
node e2e/run.mjs                                  # the end-to-end suite
```

The suite is organized by failure mode rather than by module ~ adversarial
input, boundary shapes, contention, injected faults, and properties that hold
for every input ~ and `docs/development/testing.md` describes the layers, the
fuzz targets, and the behaviours that are deliberate boundaries rather than
gaps.


## License

MIT, see [LICENSE](LICENSE).

OJ was created by [Raphael Amorim](https://rapha.land/introducing-oj/) and has since migrated to [Lovable](https://lovable.dev), which now maintains the project.