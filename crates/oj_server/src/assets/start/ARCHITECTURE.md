# TanStack Start adapter

This directory is oj's adapter for [TanStack Start](https://tanstack.com/start)
apps. It lets `oj dev` and `oj build` run a TanStack Start app without Vite,
by reproducing the build-time glue the TanStack Vite plugin provides and by
running enough of the app's Vite plugins to keep its conventions working.

The Rust side is small: `crates/oj/src/start_dev.rs` (dev server and prod build
orchestration) and three helpers in `crates/oj_server/src/lib.rs`
(`is_tanstack_start_app`, `write_start_assets`, and the `START_ASSETS` list).
Everything else is the JavaScript assets in this directory, embedded into the
binary with `include_str!` and written into `<app>/.oj-cache/start/` at run
time.

## Detection

`is_tanstack_start_app(root)` returns true when the app has a `src/routes`
directory and `@tanstack/react-start` in its `package.json`. `oj dev` and
`oj build` route such apps to `start_dev` / `start_build` instead of the normal
pipeline.

## The framework seam

TanStack Start is designed to be wired into an app by a bundler. The framework
runtime imports four bare specifiers that the bundler is expected to fill in,
plus one manifest specifier:

- `#tanstack-router-entry` becomes the app's `src/router` (its `getRouter`).
- `#tanstack-start-entry` becomes a start instance (we supply an empty one).
- `#tanstack-start-plugin-adapters` becomes the plugin adapter list.
- `#tanstack-start-server-fn-resolver` becomes the server-function dispatch map.
- `tanstack-start-manifest:v` becomes the client asset manifest.

In dev these are resolved at runtime by `loader.mjs`. In prod they are resolved
at build time by rolldown's `resolve.alias` map. This seam is the reason the prod
server is fully bundled: `createStartHandler` (inside `@tanstack/start-server-core`)
runs `import("#tanstack-router-entry")`, and that import only resolves if the
framework package is bundled so rolldown can apply the alias. See "Known limits".

## Asset files

Shared build helpers:

- `resolve-pkg.mjs` resolves a package under a pnpm-strict layout (a transitive
  dep is not reachable from the app root, so it is resolved through the app's
  direct deps as anchors). Also exports `viteEnvDefine` for `import.meta.env`.
- `rolldown-assets.mjs` holds the rolldown building blocks shared by dev and prod:
  `assetsPlugin` (Vite-style `?url` / `?raw` / `?inline` / bare-asset / css
  imports), `makeVitePlugins` (routes `virtual:` ids, `.mdx`, bare `.svg`, and
  the `.svg?react` query through the plugin container), `nodeBuiltinShims`,
  `pnpmStorePaths`, `workspaceRoot`, and `contentHashEmitter` (a
  content-addressed asset emitter that rewrites css `url()` refs).
- `vite-plugin-bridge.mjs` is the plugin container. See below.
- `glob-transform.mjs` expands `import.meta.glob` into a literal map.
- `cf-server.mjs` is the dev/prod shim for `@cloudflare/vite-plugin/server`.
- `cf-build.mjs` builds the Worker environment for `@cloudflare/vite-plugin`.
- `nitro-build.mjs` detects `nitro/vite` and gives its buildApp hooks a
  Vite-like builder, so Nitro packages oj's client and SSR output.

Dev:

- `loader.mjs` is a Node ESM loader hook (in-thread, synchronous — installed
  via `module.registerHooks`). It resolves the framework aliases, the app's
  `#`-imports and tsconfig `paths`, asset conventions, `virtual:` ids, `.mdx`
  and `.svg`, and provides CJS-to-ESM interop; it compiles TS/JSX with
  rolldown's `transformSync` on the fly. The async Vite plugin container runs
  in a worker (`container-host.mjs`) behind an Atomics-based sync bridge
  (`container-bridge.mjs`), so the hooks stay synchronous and container
  bootstrap overlaps module loading.
- `runner.mjs` is the persistent SSR process. It imports `loader.mjs` and
  registers its hooks, imports the server entry, and answers render requests
  over stdio. A reload message re-imports the entry in place (warm reload).
- `generate.mjs` runs `@tanstack/router-generator` to write `routeTree.gen.ts`.
- `gen-resolver.mjs` scans `src` for `createServerFn` and emits the server-fn
  resolver (`getServerFnById`) with static imports.
- `bundle-client.mjs` bundles the browser client entry with rolldown.
- `live-reload.js` is the dev client that snapshots and restores form state
  across a warm reload.

Prod:

- `build.mjs` is the production pipeline: client bundle, manifest, server
  bundle, `server.mjs` (Node) and `worker.mjs` (edge), plugin `generateBundle`
  emission, public copy, and prerender.

Framework entries (synthesized, what the TanStack Vite plugin would inject):

- `server-entry.tsx` wraps `createStartHandler(defaultStreamHandler)` into a
  `{ fetch(request) }` default export.
- `client-entry.tsx` hydrates the document with `<StartClient/>`.
- `start-entry.ts`, `plugin-adapters.ts`, `manifest.ts` fill the remaining
  seam specifiers.
- `fn-stubs.mjs` is a runtime `createIsomorphicFn` that branches on
  `typeof document` (the compiler stub defaults to the server impl otherwise).

## Dev flow (`start_dev`)

1. Write the assets into `.oj-cache/start/`.
2. Generate the route tree and the server-fn resolver.
3. Bundle the browser client entry.
4. Build on top of the normal oj dev server for module and asset serving.
5. Spawn the warm SSR runner.
6. Watch `src/`. On a change: regenerate the route tree only when the route
   file set changes, regenerate the resolver only when a server-fn file
   changes, then rebuild the client and warm-reload the runner concurrently,
   and signal the page to reload.

Document requests are forwarded to the runner's `fetch(Request)`. The runner
builds the request URL from the Host header so the server-function CSRF
same-origin check passes. Module and asset requests fall through to the dev
pipeline. `/@oj-start/` owns the live-reload WebSocket, the client entry, and
`/@oj-start/fs/<abspath>` (asset files served from the workspace root, so
relative `url()` refs inside CSS resolve).

### The request loop

The adapter is a single axum `from_fn` middleware (`start_route`) layered over
the normal oj dev server's router. Every request passes through it; SSR is an
interception and anything the middleware does not claim falls through to the
base dev pipeline (module compilation, asset serving) via `next.run`.

The middleware handles, in order:

1. `/@oj-start/hmr`: upgrade to the live-reload WebSocket. It subscribes to a
   broadcast channel and sends `reload` on every rebuild.
2. `/@oj-start/client-entry.js` and `/@oj-start/live-reload.js`: serve the two
   cached dev scripts (`no-cache`, so a rebuild always wins).
3. `/@oj-start/fs<abspath>`: `serve_fs_asset`. The path after the prefix is the
   file's real absolute path; it is percent-decoded, canonicalized, and rejected
   with 403 if it escapes the workspace root. A Tailwind stylesheet is compiled
   by the CSS host before serving; everything else is streamed as-is with a
   `Content-Type` from its extension.
4. `/_serverFn/...`: a server-function RPC. The whole request (method, headers,
   body, capped at 4MB) is forwarded to the runner, which dispatches it through
   the framework's `handleServerAction`.
5. Otherwise `classify` decides: a GET for an extensionless path or `index.html`
   (and not a proxied prefix) is a `Document`, forwarded to the runner as a GET
   with `document_url` applied (`.../index.html` becomes `.../`); everything else
   is `Pass`, handed to the base dev server.

`forward` is the one path to the runner. Because there is a single runner
process with one stdin/stdout, calls are serialized behind an async mutex and
run in a detached task, so a client that disconnects mid-request still drains
the runner's reply line and the protocol stays in frame. On the way back it
rebuilds the axum response: the status, the headers (minus the framing ones,
`content-length` / `content-encoding` / `transfer-encoding`, since the body is
re-sent verbatim), and the body, with the live-reload client injected before
`</body>` for HTML documents.

### The runner protocol (`runner.mjs`)

`start_dev` and the runner talk over the runner's stdio in newline-delimited
JSON. One JSON object per line goes in on stdin; one JSON object per line comes
back on stdout, so `oj` can pair replies by reading a line per request.

There are two request shapes:

- A render request `{ id, method, url, headers, body }`. The runner builds a
  `Request` (origin from `headers.host`, body only for non-GET/HEAD), awaits the
  entry's `fetch`, and replies `{ id, status, headers, body }`. A handler that
  throws is framed as `{ id, status: 500, body: <error text> }` rather than
  crashing the process.
- A reload command `{ cmd: "reload" }`. The runner bumps the version, re-imports
  the entry (see below), and replies `{ reloaded: true }`, or
  `{ reloaded: false, error }` if the re-import failed.

stdout is the protocol channel, so app code writing to it (via `console.log` or
`process.stdout.write`) would interleave with and corrupt the frames. The runner
captures the real stdout writer for protocol frames on startup, then redirects
`process.stdout.write` to stderr. App logs and the `oj start runner: ready`
banner therefore surface on stderr, keeping stdout pure JSON.

The entry and loader paths default to the assets beside `runner.mjs` but are
overridable with `OJ_RUNNER_ENTRY` and `OJ_RUNNER_LOADER`, which the protocol
test uses to drive the framing against stubs without the loader's rolldown
bootstrap.

### The watcher

A dedicated OS thread runs a `notify` recursive watcher on `src/` and bridges
its events into the tokio runtime. The thread keeps `prev_routes`, the route
file set from the last pass, so it can tell a structural change from a content
edit.

Each pass:

1. Debounce. On the first event it collects the changed paths, then keeps
   draining events on a 50ms timeout, coalescing every path into one batch. This
   collapses an editor's save-storm (or a multi-file change) into a single
   rebuild.
2. Ignore its own output. A batch that only touches the generated
   `routeTree.gen.ts` is skipped, so regenerating the tree cannot trigger
   another pass.
3. Regenerate conditionally, because both generators cost more as the app grows:
   - the route tree only when the route file SET changed (add / remove / rename),
     compared against `prev_routes`; a content edit leaves the tree alone.
   - the server-fn resolver only when a `.ts`/`.tsx` file that contains
     `createServerFn` was added, removed, or edited (or when the routes changed).
4. Rebuild and reload, concurrently: the client entry re-bundles on a blocking
   task while `reload_runner` warm-reloads the SSR process (`{cmd:"reload"}` over
   its stdin, awaiting the ack). Both are awaited together with `tokio::join`.
5. Signal the page. A broadcast on `reload_tx` reaches every `/@oj-start/hmr`
   WebSocket, which tells the live-reload client to snapshot state and reload.

### Warm reload

On a rebuild the runner bumps a version counter and re-imports the server entry
with a `?ojv=<v>` query. The loader appends that query to app files and to
`@tanstack/*` ESM modules so they re-evaluate (resetting framework module-scope
caches such as the route tree), while React and other node_modules stay
unversioned and warm. This keeps a single React instance and re-imports at
about 40ms instead of respawning the process.

## Prod flow (`start_build` and `build.mjs`)

1. Generate the route tree and the server-fn resolver.
2. Client bundle: rolldown, minified, hashed, code-split, with the plugin
   container, `import.meta.glob`, `import.meta.env`, asset loaders, svgr, mdx,
   and pnpm nodePaths.
3. Manifest pointing at the hashed client entry.
4. Server bundle: rolldown, minified, code-split (splitting preserves the
   framework's top-level await, which a single-file bundle cannot), fully
   bundled so the framework `#`-imports resolve at build time. Node builtins
   stay external.
5. `generateBundle`: run the app plugins' `generateBundle` hooks so plugins
   that publish files (for example content-assets emitting
   `/__content/<collection>/<file>`) produce their output into `dist/client`.
6. Emit `server.mjs` (Node http), `worker.mjs` (edge fetch handler),
   `cf-server.mjs` and `cf-loader.mjs`, copy `public/`, and prerender any
   routes configured in `oj.config` `build.prerender`.

Client and server builds emit assets through the same `contentHashEmitter`, so
both produce identical `/assets/<hash>` URLs for identical bytes with no shared
manifest. CSS is emitted with its `url()` references rewritten and its
referenced fonts and images emitted alongside. The emitter records the
stylesheet urls, which the manifest lists under the root route's `css` so
`HeadContent` links them in the SSR `<head>` (no flash of unstyled content); the
client bundle no longer injects the link in production.

### The two rolldown passes

The client pass targets the browser: minified, hashed, code-split, with browser
conditions and the node-builtin shims, entered at the hydration `client-entry`.
The server pass targets Node: `platform: node`, node builtins external, fully
bundled so the framework's `#`-import aliases resolve at build time, and
code-split rather than single-file because the framework's server module uses
top-level await that a single-file bundle cannot always
propagate. Both passes run the same plugin container (svgr, mdx, virtuals) and
the same asset plugins; the server container falls back to the client one for
`load`, since some ssr plugins expect cross-environment state Vite would share.

### The prod server entrypoints

The build writes two runtime entrypoints wrapping the same `server-bundle.mjs`.

`server.mjs` is a Node `http` server, for `node dist/server.mjs`. It first
registers `cf-loader.mjs`, a resolve hook that maps the framework's runtime,
variable `import("@cloudflare/vite-plugin/server")` (which escapes the
build-time alias) to the bundled `cf-server.mjs` shim. On a GET it tries the
exact static file and then the prerendered `<path>/index.html` under
`dist/client` before falling back to the handler's `fetch`; a request for
`.../index.html` is folded to `.../` so the router matches the directory route.

`worker.mjs` is the edge Web `fetch` handler for a Cloudflare Worker. It needs
neither the loader nor the shim, because a real deployment gets its bindings
from `cloudflare:workers`. (See Known limits for why a large app's worker can
exceed the size cap.)

## The plugin container (`vite-plugin-bridge.mjs`)

`loadPluginContainer(app, { command, mode, environment })` loads the app's
`vite.config` plugins with `vite.loadConfigFromFile` and exposes `resolveId`,
`load`, `transform`, and `generateBundle` bound to one command and environment.
It is the compatibility layer that makes `virtual:` modules, MDX, svgr, and
content emission work. It applies the same gating Vite applies:

- plugin `apply` ("build" | "serve" | function) versus the current command,
- object-form hooks `{ handler, filter, order }` where `filter.id` must match
  before the handler runs,
- `enforce` order (pre, then normal, then post), first non-null wins,
- `applyToEnvironment` for `generateBundle`.

The container hosts every plugin that owns any of `resolveId`, `load`,
`transform`, or `generateBundle`. Getting the gating right is what keeps a
build-only, id-filtered stub plugin from swallowing every id. The prod server
build passes the client container as a `fallback` for `load`, because some ssr
plugins error expecting cross-environment state that Vite shares between the
client and ssr builds and our separate containers do not.

### svgr and the `?react` query

svgr keys its `load` filter on the import id, so both svg import styles reach
it as an id the plugin recognizes:

- Bare `import Logo from "./logo.svg"`. rolldown resolves the file normally and
  `makeVitePlugins` runs its `.svg` `load` for the resolved path. svgr claims
  it when configured with `exportType: "default"` (or an `include` that matches
  the file); otherwise the id falls back to an asset URL.
- Explicit `import Logo from "./logo.svg?react"`. rolldown's resolver cannot
  find a file named `logo.svg?react`, so `makeVitePlugins` strips the query,
  resolves the real `.svg`, parks it in a `\0oj-svg-react:` virtual id, and calls
  `container.load(path + "?react")` so svgr sees the query it filters on. The
  dev loader mirrors this: it resolves the file and tags the url `?ojsvg=react`,
  then `load()` hands svgr the `path + "?react"` id. Either way the URL fallback
  applies when no plugin claims the id.

The bare `.svg` `load` skips `\0`-prefixed ids so it does not also
claim the `?react`-tagged files, which would drop the query before svgr runs.

## Resolution chain

For app-owned specifiers the dev loader and the rolldown builds resolve in this
order:

1. framework aliases and the CF shim,
2. `virtual:` ids, `.mdx`, and `.svg` (bare or `?react`) through the plugin
   container,
3. Vite asset conventions (`?url`, `?raw`, `?inline`, bare fonts and images),
4. package.json `imports` subpaths (`#shared`, `#modules`) with extension
   probing,
5. tsconfig `paths` (a string-aware JSONC parse that follows `extends`),
6. relative and bare resolution, with `.js` to `.ts` remapping, directory to
   `index`, and file probing,
7. for the dev loader only, CJS-to-ESM interop for node_modules whose named
   exports Node's `cjs-module-lexer` cannot detect (require the module and
   re-export its actual runtime keys; ESM files, including syntax-detected
   dual-package `.js`, are left to Node).

rolldown handles CJS interop, `#`-imports, tsconfig paths, `.js`-to-`.ts`, and
directory resolution natively when bundling, so the prod builds only need the
plugin container, glob, env, assets, and shims.

## Server functions

`createServerFn(...).handler(fn)` compiles to different code on each side, tied
together by one id: `base64url(<app-relative path>#<name>)`, computed identically
everywhere so a browser call can find its handler.

Three transform sites share that id:

- Server (SSR). The dev loader (`rewriteServerFns`) and the prod build's server
  pass rewrite `const NAME = createServerFn(...).handler(FN)` to pass
  `createServerRpc(meta, (opts) => NAME.__executeServer(opts))` as the first
  `.handler` argument and to export `NAME_createServerFn_handler`. The handler
  runs in process during SSR, and the export is importable by the resolver.
- Client. The dev client bundle and the prod build's client pass replace that
  first argument with `createClientRpc(id)`. The runtime treats argument one as
  the extracted function, so the browser makes an HTTP RPC to `/_serverFn/<id>`
  instead of running the handler.
- The resolver. `gen-resolver.mjs` scans `src/` for the same declarations and
  writes a manifest mapping each id to its module's exported handler, using
  static relative imports so it resolves in both the dev loader hook and the
  bundled prod server (a runtime `import` of a `.ts` would fail in the bundle).
  It exports `getServerFnById(id)` behind the `#tanstack-start-server-fn-resolver`
  alias.

An RPC hits `/_serverFn/<id>`; the framework's `handleServerAction` looks the id
up through the resolver, runs the handler, and returns the result. In dev the
request loop forwards `/_serverFn/` requests whole (method, headers, body) to the
runner; in prod the server bundle handles them directly. Both build the request
origin from the Host header so the framework's same-origin CSRF check passes.

## Testing

The adapter is covered at three levels.

Rust unit tests live beside the code they exercise:

- `oj_server/src/lib.rs` (`adapter_tests`): `is_tanstack_start_app` detection,
  `locate` preferring the app root then the publicDir, and the SPA-navigation
  rule.
- `oj_server/src/plugins.rs`: `parse_vite_values` / `merge_vite_values` (the
  vite.config values oj adopts, including publicDir), asserting config never
  overrides an explicit oj setting.
- `oj/src/start_dev.rs`: `document_url`, `percent_decode`, `asset_mime`,
  `needs_css_compile`, `workspace_root`, `app_uses_tailwind`, and `classify`.

JS unit tests are `node --test e2e/unit/*.test.mjs` (rolldown for the harness
comes from the fixture; the suite skips cleanly when it is not installed):

- `glob-transform` expands `import.meta.glob`; `rolldown-assets` covers the
  content-hash emitter, css `url()` rewriting, the Tailwind compile hook,
  `workspaceRoot`, and `pnpmStorePaths`; `resolve-pkg` covers `viteEnvDefine`
  and the pnpm-anchor resolver.
- `plugin-gating` covers the plugin-container gating (`apply`, id filters,
  enforce order, hook shapes); `cf-vars` covers the Cloudflare shim's
  JSONC / toml / dotenv var parsers.
- `loader-util` covers the SSR loader's pure helpers: extension probing, CJS
  detection and the CJS-to-ESM facade, JSONC parsing, the server-fn rewrite,
  single-`*` alias matching, and the package `imports` / tsconfig-chain parsers.
- `asset-routing` is a build-level harness: it drives `assetsPlugin`,
  `makeVitePlugins`, and `nodeBuiltinShims` through a real rolldown bundle over
  temp files with a stub plugin container, asserting the `?url` / `?raw` /
  `?inline` / bare-asset / css routing, the `virtual:` / `.mdx` / `.svg` (bare
  and `?react`) container routing, and the node-builtin shims.
- `runner-protocol` spawns the real `runner.mjs` (entry and loader pointed at
  stubs) and drives the stdio protocol: render round-trips, the Host-derived
  origin, warm reload, 500 error framing, and the stdout-diversion guard.

The integration test is `node e2e/start.mjs`. It runs oj against a self-contained
Start app (`e2e/fixtures/start-app`) in dev and prod and asserts a server-rendered
`/` wires together routing, server functions, `import.meta.glob`, `?raw` / `?url`,
svgr (bare and `?react`), MDX, Tailwind, a plugin virtual module, tsconfig paths
and package `imports` aliases, a CommonJS dep, the Cloudflare context, and a
non-default publicDir. The fixture's dependencies are not vendored; the test
skips when they are not installed (`cd e2e/fixtures/start-app && npm install`).

## Known limits

- The prod server is fully bundled and code-split because of the framework
  `#`-import seam and its top-level await. On a large app this produces many
  chunks. Externalizing node_modules to reduce this fails at runtime, because
  the framework's runtime `import("#tanstack-router-entry")` then resolves
  package-locally against the framework package and is not defined there.
- `worker.mjs` re-exports the bundled server, which for a large app exceeds
  Cloudflare Workers' size limit. A real edge deploy needs the Cloudflare
  plugin's route-split worker build.
- `generateBundle` hooks that read the output `bundle` (for example
  chunk-import-map or chunk-keyed i18n emissions) receive an empty bundle, so
  those specific emissions are skipped. Asset emissions from a committed
  manifest (content-assets) work.
- Live Cloudflare KV / D1 / R2 / service bindings are not emulated; the
  `cf-server.mjs` shim provides env vars (wrangler `vars` plus `.dev.vars`) and
  an ASSETS binding backed by `dist/client`.
