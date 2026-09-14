# oj

[OJ's logo](/assets/OJ-spill.png)

0. If you like the idea and project please consider sponsor, working on it in my free time.
1. This is a research project, use on your own risk.
2. It was created out of frustration with agents running vite build when I was working on rioterm-js repository. Each build process was carrying 2gb.
3. Working on it mostly to fix my own problems.

OJ is a Rust-native build tool for React apps.

It optimizes for memory and cold start, where running many builds (CI, agents, multi-tenant) under Vite gets expensive. OJ is meant to run real production React apps without changes to their source.

This project is alpha, so expect bugs. For real.

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
cargo install oj                                # install the `oj` CLI
oj dev                                           # dev server for the current app on :5199
oj build                                         # production build into ./dist
```

## Benchmarks

Generated fanout-10 React component trees, measured save-to-paint with Playwright against Vite 8.2.1 (Rolldown-based) on an M-series Mac, in both its default dev mode (vite) and its experimental bundled dev mode (vite-fbm). p50/p95 over 5 cold+warm restart cycles and 10 HMR edits.

**1,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **311/315ms** | **240/243ms** | **41/42ms** | **56/57ms** | **43MB** |
| oj (unbundled) | 434/582ms | 352/354ms | 170/173ms | 56/59ms | 43MB |
| vite | 701/734ms | 653/659ms | 172/176ms | 55/73ms | 434MB |
| vite-fbm | 326/344ms | 334/338ms | 49/52ms | 55/58ms | 361MB |

**5,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **750/783ms** | **588/604ms** | **132/133ms** | **59/61ms** | **75MB** |
| oj (unbundled) | 1281/1402ms | 1058/1080ms | 744/839ms | 60/63ms | 65MB |
| vite | 2649/2656ms | 2424/2532ms | 758/807ms | 56/147ms | 949MB |
| vite-fbm | 927/970ms | 965/973ms | 174/177ms | 59/80ms | 976MB |

**10,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **1315/1333ms** | **1129/1442ms** | **232/237ms** | **67/72ms** | **115MB** |
| oj (unbundled) | 2492/2543ms | 2068/2303ms | 1523/1687ms | 67/176ms | 94MB |
| vite | 5693/6085ms | 5304/6289ms | 1781/1820ms | 59/64ms | 1552MB |
| vite-fbm | 1528/2225ms | 1482/2176ms | 302/414ms | 59/65ms | 1751MB |

Bundle-mode oj wins cold start, warm start, and reload against Vite's default dev at every size (4-8x at 10k), and now also beats Vite's experimental bundled dev on all three at every size (cold, warm, and reload). HMR is a wash across all four (within ~10ms). oj's decisive, consistent win is memory: 43-115MB against Vite's 361MB-1.75GB, an 8-15x gap that widens with app size.

Production builds (`oj build` vs `vite build`) land at parity: same engine (Rolldown), byte-identical output sizes.

## Reference reading

- [oxc_transformer/examples/transformer.rs](https://github.com/oxc-project/oxc/blob/main/crates/oxc_transformer/examples/transformer.rs): the pipeline oj's compiler is based on
- [oxc_transformer/src/jsx](https://github.com/oxc-project/oxc/tree/main/crates/oxc_transformer/src/jsx): JSX + ReactRefresh transform internals
- [vitejs/vite-plugin-react](https://github.com/vitejs/vite-plugin-react): the Fast Refresh glue semantics oj replicates
- [vite/packages/vite/src/node/server](https://github.com/vitejs/vite/tree/main/packages/vite/src/node/server): HMR propagation, `import.meta.hot` protocol
- [rolldown/rolldown](https://github.com/rolldown/rolldown): plugin hook filters, the prod linker oj embeds

## Self note

```sh
cargo run -p oj -- dev                            # dev server for ./playground on :5199
cargo run -p oj -- dev --bundle                   # registry-runtime bundle mode
cargo run -p oj -- dev --ssr src/entry-server.tsx # streaming SSR + hydration
cargo run -p oj -- build playground               # production build into playground/dist
cargo test --workspace                            # rust unit tests
node --test e2e/unit/*.test.mjs                   # js unit tests (adapter helpers)
node e2e/run.mjs                                  # browser e2e suite (add --bundle for bundle mode)
node e2e/ssr-dev.mjs                              # SSR dev e2e, e2e/ssr-prod.mjs for the built server
node e2e/start.mjs                                # tanstack start integration (see e2e/fixtures/start-app)
node e2e/dep-optimize.mjs                         # dependency pre-bundle + cjs interop integration
node e2e/assets.mjs                               # asset url imports + new URL(import.meta.url)
node e2e/dynamic-import.mjs                       # dynamic import with variables (glob switch)
node e2e/wasm.mjs                                 # wasm ?init instantiation (dev + build)
node e2e/query-assets.mjs                         # ?url/?raw/?inline/?init in bundle mode
node e2e/rolldown-options.mjs                     # build.rollupOptions filenames + external
node e2e/preprocessors.mjs                        # less + stylus css (installs both, then dev+build)
node e2e/assets-inline.mjs                        # assetsInlineLimit: small assets become data uris
node e2e/config-proxy.mjs                         # adopt vite.config server.proxy + ignored-config warnings
node e2e/proxy-regex-context.mjs                  # server.proxy "^..." regex contexts (path + query), like Vite
node e2e/config-function.mjs                      # oj.config function form ({ command, mode }) => config
node e2e/build-target-raw-inline.mjs              # build.target downleveling + ?raw/?inline in build
node e2e/manual-chunks.mjs                        # rollupOptions output.manualChunks vendor splitting
node e2e/svgr.mjs                                 # svg as react component (?react), installs react
node e2e/worker-modes.mjs                         # ?worker in dev, bundle, and production build
node e2e/html-entry.mjs                           # relative index.html script entry (src="src/x")
node e2e/svelte.mjs                               # svelte 5 components in dev, bundle, and build
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

The suite is organized by failure mode rather than by module -- adversarial
input, boundary shapes, contention, injected faults, and properties that hold
for every input -- and `docs/development/testing.md` describes the layers, the
fuzz targets, and the behaviours that are deliberate boundaries rather than
gaps.
