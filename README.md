<!-- LOGO -->
<h1>
<p align="center">
  <img src="/assets/OJ-spill.png" alt="OJ logo" width="256">
  <br>OJ
</p>
</h1>

OJ is a Rust-native build tool for React apps.

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

## Quickstart

Install the CLI from crates.io, then run `oj` in any app:

```sh
cargo install oj --locked                       # install the `oj` CLI
oj dev                                           # dev server for the current app on :5199
oj build                                         # production build into ./dist
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

Generated fanout-10 React component trees, measured save-to-paint with Playwright against Vite 8.2.1 (Rolldown-based) on an M-series Mac, in both its default dev mode (vite) and its experimental bundled dev mode (vite-fbm). p50/p95 over 5 cold+warm restart cycles and 10 HMR edits.

Memory is measured over the whole process tree, and reported two ways: resident set (tree RSS) and macOS physical footprint, which excludes clean and reclaimable resident pages and so bounds retained memory. Forced-GC RSS is not a column because it cannot be measured symmetrically (Node exposes an external GC handle through its inspector; embedded V8 does not) — for reference, forcing GC before measuring lowered vite's tree RSS by roughly a third in our runs, still well above its footprint. Methodology corrections owe to [#202](https://github.com/lovablelabs/oj/issues/202).

**1,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | tree RSS | footprint |
|---|---|---|---|---|---|---|
| **oj** | **661/756ms** | **433/460ms** | **232/238ms** | **55/59ms** | **139MB** | **96MB** |
| vite | 769/780ms | 724/728ms | 229/232ms | 54/92ms | 405MB | 325MB |
| vite-fbm | 341/344ms | 340/342ms | 54/56ms | 55/59ms | 357MB | 288MB |

**5,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | tree RSS | footprint |
|---|---|---|---|---|---|---|
| **oj** | **1711/1744ms** | **1477/1509ms** | **1037/1056ms** | **59/62ms** | **187MB** | **145MB** |
| vite | 2966/3396ms | 2793/3020ms | 1002/1099ms | 55/60ms | 903MB | 699MB |
| vite-fbm | 959/973ms | 954/969ms | 167/172ms | 57/61ms | 974MB | 770MB |

**10,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | tree RSS | footprint |
|---|---|---|---|---|---|---|
| **oj** | **4168/4487ms** | **3898/4075ms** | **2934/3115ms** | **59/184ms** | **231MB** | **190MB** |
| vite | 6983/7413ms | 6479/7037ms | 2400/2454ms | 83/266ms | 1465MB | 1126MB |
| vite-fbm | 1593/2130ms | 1585/1705ms | 307/329ms | 60/69ms | 1761MB | 1229MB |

oj wins cold and warm start against Vite's default dev at every size (~1.7x at 10k). HMR is a wash across all three. oj's decisive, consistent win is memory: 96-190MB footprint against Vite's 288MB-1.2GB, roughly 3x at 1k growing to 6x at 10k, measured over whole process trees.

Production builds (`oj build` vs `vite build`) land at parity: same engine (Rolldown), byte-identical output sizes.

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
node e2e/start.mjs                                # tanstack start integration (see e2e/fixtures/start-app)
node e2e/dep-optimize.mjs                         # dependency pre-bundle + cjs interop integration
node e2e/assets.mjs                               # asset url imports + new URL(import.meta.url)
node e2e/dynamic-import.mjs                       # dynamic import with variables (glob switch)
node e2e/wasm.mjs                                 # wasm ?init instantiation (dev + build)
node e2e/query-assets.mjs                         # ?url/?raw/?inline/?init asset queries
node e2e/rolldown-options.mjs                     # build.rollupOptions filenames + external
node e2e/preprocessors.mjs                        # less + stylus css (installs both, then dev+build)
node e2e/assets-inline.mjs                        # assetsInlineLimit: small assets become data uris
node e2e/config-proxy.mjs                         # adopt vite.config server.proxy + ignored-config warnings
node e2e/proxy-regex-context.mjs                  # server.proxy "^..." regex contexts (path + query), like Vite
node e2e/config-function.mjs                      # oj.config function form ({ command, mode }) => config
node e2e/build-target-raw-inline.mjs              # build.target downleveling + ?raw/?inline in build
node e2e/manual-chunks.mjs                        # rollupOptions output.manualChunks vendor splitting
node e2e/svgr.mjs                                 # svg as react component (?react), installs react
node e2e/worker-modes.mjs                         # ?worker in dev and production build
node e2e/html-entry.mjs                           # relative index.html script entry (src="src/x")
node e2e/svelte.mjs                               # svelte 5 components in dev and build
node e2e/build-mode.mjs                           # build --mode (import.meta.env.MODE + .env.<mode>)
node e2e/hmr-protocol.mjs                         # hmr client derives wss (behind https proxy)
node e2e/hmr-overlay-recovery.mjs                 # startup compile error: buffered overlay, reload on first update
node e2e/hmr-json-update.mjs                      # editing an imported .json hot-updates its boundary (?t= stamp)
node e2e/hmr-unresolved-import.mjs                # missing ./import fails with an overlay; creating the file recovers
node e2e/host-binding.mjs                         # dev/preview --host + server.host bind all interfaces
node e2e/plugin-ws.mjs                             # plugin server.ws custom events (send/on) round-trip
node e2e/plugin-ws-execute.mjs                     # post -> ws broadcast -> client reply -> collect (bridge execute)
node e2e/plugin-middleware.mjs                     # configureServer post body forwarding + transformIndexHtml
node e2e/hmr-gate.mjs                              # hmr gate holds updates until POST /__hmr_flush
node e2e/config-flag.mjs                           # oj dev --config <path> loads an override config
node e2e/config-wrapper.mjs                        # vite.config that calls an external defineConfig wrapper
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