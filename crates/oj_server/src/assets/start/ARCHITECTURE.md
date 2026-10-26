# TanStack Start adapter

This directory is oj's adapter for [TanStack Start](https://tanstack.com/start)
apps. It lets `oj dev` and `oj build` run a TanStack Start app without Vite,
by reproducing the build-time glue the TanStack Vite plugin provides and by
running enough of the app's Vite plugins to keep its conventions working.

The Rust side does the bundling: `crates/oj/src/start_bundle/` builds the
client, server and Cloudflare Worker bundles and the dev client bundle with
its hot updates on the rolldown crates oj links, and
`crates/oj/src/start_dev.rs` orchestrates dev and `oj build`. The app's Vite
plugins run in plugin hosts behind a rolldown plugin. The JavaScript files in
this directory are the framework entries oj synthesizes, the route generator
driver, and the dev and prod runtime shims, embedded into the binary with
`include_str!` and written into `<app>/.oj-cache/start/` at run time
(`write_start_assets` and the `START_ASSETS` list in
`crates/oj_server/src/lib.rs`).

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

In dev these are resolved by the in-process Start module host
(`crates/oj/src/start_host.rs`, the embedded JS engine's `ModuleHost`) for
SSR and by the client bundle's aliases for the browser. In prod they are
rolldown aliases of the client and server bundles. This seam is the reason the prod
server is fully bundled: `createStartHandler` (inside `@tanstack/start-server-core`)
runs `import("#tanstack-router-entry")`, and that import only resolves if the
framework package is bundled so rolldown can apply the alias. See "Known limits".

## Where things live

The bundles (`crates/oj/src/start_bundle/`):

- `mod.rs`: what a Start bundle is configured from (`StartConfig`), the
  framework aliases, the client and server defines, conditions and resolver
  options, and `write_server_fn_resolver`, which scans `src/` for
  `createServerFn` and writes the server-fn resolver (`getServerFnById`) with
  static imports.
- `host.rs` / `hosts.rs`: the app's Vite plugins inside a bundle. A plugin
  host started in bundle mode (the TanStack plugins oj reimplements left
  out), driven from rolldown's hooks with Vite's order around its core
  resolver; a plugin's `this.resolve` ends in the bundle's own resolution
  through a reverse RPC. A build shares one host across its environments; a
  worker bundle gets a plugin container of its own on that host. The dev
  client bundle runs in a container of the dev server's own host, its client
  environment (one buildStart, configureServer run once).
- `alias.rs`: the app's `resolve.alias`, ahead of every plugin.
- `assets.rs`: Vite's asset, stylesheet and worker conventions (`?raw`, `?url`,
  `?inline`, `?worker`, bare asset imports, CSS and CSS modules) and the
  content-hash emitter the client, server and Worker builds share.
- `code.rs`: `new URL(..., import.meta.url)`, `import.meta.glob` and the
  `createServerFn` split (the transforms themselves live in
  `oj_compiler::meta_glob` and `oj_compiler::server_fns`).
- `shims.rs`: Node builtins in a browser bundle, as Vite stubs them.
- `chunks.rs`: the app's `codeSplitting` / `chunkOptimization` policy, with
  function-valued groups called back in the plugin host in batches.
- `errors.rs`: Vite's unresolved-import error and its ErrorPayload fields for
  the overlay.
- `dev.rs` / `hot.rs` / `refresh.rs`: the dev client bundle, its hot updates
  on `rolldown_dev`'s `DevEngine`, and React Fast Refresh.
- `prod.rs` / `cloudflare.rs`: `oj build`, and the Worker environment's
  bundle when the app uses `@cloudflare/vite-plugin`.

The files in this directory:

- `generate.mjs` runs `@tanstack/router-generator` to write `routeTree.gen.ts`,
  importing it through `resolve-pkg.mjs` (which reaches a transitive dep under
  a pnpm-strict layout through the app's direct deps as anchors). It runs in a
  one-shot `oj start-script` child per regeneration.
- `prerender.mjs` renders the `build.prerender` routes against the built
  server bundle.
- `cf-server.mjs` is the dev/prod shim for `@cloudflare/vite-plugin/server`,
  `cf-workers.mjs` the dev stub for `cloudflare:workers`, and
  `cf-server-worker.mjs` the Worker bundle's `getCloudflareContext()`.
- `hmr-runtime.js` is the dev client's hot-update runtime; `live-reload.js` is
  the dev client that snapshots and restores form state across a warm reload.
- `vite-globs.mjs` loads Vite's own glob libraries (picomatch, tinyglobby)
  from the app's vite, or the copies oj ships in `glob-libs.mjs`, for the
  plugin host's hook filters and glob matchers.

Dev SSR:

- The SSR runner is IN-PROCESS: an embedded JS engine (`oj_js`, deno_core with
  Node compat over the app's node_modules) whose module loading is
  `crates/oj/src/start_host.rs`. The host resolves the framework aliases, the
  app's `#`-imports and tsconfig `paths` (via the Vite-style Rust resolver),
  asset conventions, `virtual:` ids, `.mdx` and `.svg`, and serves modules
  through the dev server's SSR transform pipeline (plugin transforms + oxc
  compile with `define` / `import.meta.env` / `import.meta.glob`); CommonJS
  interop and node_modules loading are the engine's byonm semantics. The JS
  half is `crates/oj/src/assets/start-bootstrap.mjs` (the `fetch(Request)`
  protocol only). The user's Vite plugins run in the ssr environment's
  container on the dev server's plugin host, on the same plugin instances
  as the client's.

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
2. Generate the route tree (a one-shot child) and the server-fn resolver.
3. Bundle the browser client entry in process; the session keeps one plugin
   container for it on the dev server's host (one buildStart, plugin state
   across rebundles) and, once a
   page connects, one `DevEngine` that turns each edit into a hot update.
4. Build on top of the normal oj dev server for module and asset serving.
5. Boot the in-process SSR engine (`StartEngine`) on the built app's
   `SsrBridge`, plus a loopback shim for the plugin middleware's
   `x-oj-forward-to` pipe.
6. Watch `src/`. On a change: regenerate the route tree only when the route
   file set changes, regenerate the resolver only when a server-fn file
   changes, then rebuild the client and warm-reload the engine concurrently,
   and signal the page to reload.

Document requests are engine calls into the bootstrap's `handle`, which awaits
the entry's `fetch(Request)`. The request URL is built from the Host header so
the server-function CSRF same-origin check passes. Module and asset requests fall through to the dev
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
   body) reaches the engine, which dispatches it through the framework's
   `handleServerAction`; with a plugin middleware present it goes through the
   middleware first (`x-oj-forward-to` names the engine's loopback shim).
5. Otherwise `classify` decides: a GET for an extensionless path or `index.html`
   (and not a proxied prefix) is a `Document`, handed to the engine as a GET
   with `document_url` applied (`.../index.html` becomes `.../`); everything else
   is `Pass`, handed to the base dev server.

`forward` is the one path to the engine: an `engine.handle(request)` call into
the bootstrap module. Requests run concurrently on the one isolate (the
engine's call scheduler interleaves pending promises with the event loop). The
transport is buffered — headers, body bytes and set-cookie lists cross the
JSON boundary whole; the bootstrap reads a streamed `Response` to completion,
so deferred TanStack content is present — and the live-reload client is
injected after the final chunk for HTML documents.

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
4. Rebuild and reload: an edit the client's `DevEngine` accepts reaches the
   page as a hot update; anything else re-bundles the client. The engine
   reload afterwards drops changed modules (and their transitive importers)
   from the version graph and force-bumps the entry.
5. Signal the page. A broadcast on `reload_tx` reaches every `/@oj-start/hmr`
   WebSocket, which tells the live-reload client to snapshot state and reload.

### Warm reload

Every host-served module id carries a version (`?v=<n>`), tracked with its
mtime and importers in a graph (the same scheme as the plain SSR runner's
host). A reload bumps the changed ids and everything that transitively imports
them, plus the entry, so the next request re-links exactly the invalidated
chain; React and other node_modules stay external, unversioned and warm.
Orphaned instances leak in the isolate, so after enough of them the engine is
respawned behind a write lock (a cheap snapshot boot).

## Prod flow (`start_build` and `start_bundle::prod`)

1. Generate the route tree and the server-fn resolver.
2. Client bundle: minified, hashed, code-split, the app's chunk policy, with
   the client plugin host, `import.meta.glob`, `import.meta.env`, the asset
   conventions and the node-builtin shims, entered at `client-entry`.
3. Manifest pointing at the hashed client entry and its stylesheets.
4. Server bundle: `platform: node`, node builtins external, fully bundled so
   the framework's `#`-import aliases resolve at build time, code-split
   because the framework's server module uses top-level await that a
   single-file bundle cannot always carry. With `@cloudflare/vite-plugin` the
   Worker environment's bundle replaces it.
5. Each bundle runs its plugin host's `renderChunk`, `generateBundle` and
   `writeBundle` over the real output, so plugins that publish files (for
   example content-assets emitting `/__content/<collection>/<file>`) or write
   a deploy config (the Cloudflare plugin's `wrangler.json`) work, then its
   `closeBundle`, before the next environment builds.
6. Emit `server.mjs` (Node http), `worker.mjs` (edge fetch handler),
   `cf-server.mjs` and `cf-loader.mjs`, copy `public/`, and prerender any
   `build.prerender` routes.

The bundles emit assets through one content-hash emitter, so they produce
identical `/assets/<hash>` URLs for identical bytes. CSS is emitted with its
`url()` references rewritten and its referenced fonts and images emitted
alongside. The emitter records the stylesheet URLs, which the manifest lists
under the root route's `css` so `HeadContent` links them in the SSR `<head>`
(no flash of unstyled content).

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

## The plugin hosts

A Start bundle reaches the app's Vite plugins through
`start_bundle::host::StartHostPlugin`, one per bundle, over a plugin host
started for a Vite environment (`client`, `ssr`, or the Worker's). The host
resolves the app's config with the app's own Vite, minus the TanStack plugins
oj reimplements, and applies Vite's gating: `apply` against the command,
`applyToEnvironment`, object-form hooks `{ handler, filter, order }`, and
`enforce` order. Around rolldown's own resolver, resolveId runs in Vite's two
phases (the `pre` hooks first, the rest on what rolldown left), each answer
kept whole, `external` included.

The live config stays in the host, so function-valued options (a chunk
group's `name` / `test`, an `external` function) are handles the bundle calls
back, never copies.

`oj build` follows Vite's builder:

- One host serves every environment, as TanStack Start's
  `builder.sharedPlugins` asks: once the client is written the host switches
  to the server environment (`switchEnvironment`) on the same plugin
  instances, with its own plugin set (`applyToEnvironment`), buildStart,
  module graph, emitted files and closeBundle.
- `config`, `configEnvironment`, `configResolved` and `buildApp` run on
  Vite's minimal plugin context (`this.meta` with the Vite and rolldown
  versions, no `this.environment`), `configEnvironment` plugin by plugin.
- The `buildApp` hooks run in hook order, with oj's environment builds (the
  `buildEnvironments` reverse RPC) standing in for TanStack Start's config
  builder just before the first post-order hook. A throwing hook fails the
  build; an environment a plugin builds itself (Nitro's) stops it with an
  error, since oj does not run it.
- moduleParsed comes from rolldown (every module, node_modules included,
  with the resolved imports), its code lowered to JS as vite:oxc would.
- Off the client `build.emitAssets` is false by default, and at vite:asset's
  place in the sorted `generateBundle` hooks (after the `order: "pre"` hooks
  and the enforce:"pre" plugins' unordered ones) the bundle's assets and the
  files emitted so far are dropped.
- A `?worker` entry is bundled with `worker.plugins` alone, as Vite's
  `bundleWorkerEntry` does: a fresh plugin set per worker bundle (the option
  is a function, called on the app's loaded config), `configResolved` with
  `isWorker`, the whole bundle lifecycle, and what its plugins emit shipped
  with the client. The set runs in a container of the app's own host
  (`openWorkerContainer`), in the realm of the app's plugins, so the two share
  module state as they do in Vite's one process; the container has its own
  plugin list, resolved config, emitted files and module graph, and its
  `this.resolve` ends in the worker's bundle. The bundle takes
  `worker.rolldownOptions` (not the app's `build.rolldownOptions`) and
  `worker.format`, iife by default: a classic script whose `import.meta.url`
  is `self.location.href`, started without `type: "module"`; "es" splits
  its chunks and starts as a module. The app's plugins never see a worker's
  modules.

### svgr and the `?react` query

svgr keys its `load` filter on the import id, so both svg import styles reach
it through the host as the id it recognizes, `logo.svg` and `logo.svg?react`.
A `.svg` no plugin componentized falls back to its asset URL.

## Resolution chain

For app-owned specifiers the dev SSR loader resolves in this order:

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

The bundles get the same conventions from rolldown's resolver (CJS interop,
`#`-imports, tsconfig paths, `.js`-to-`.ts`, directory resolution) plus the
plugins above.

## Server functions

`createServerFn(...).handler(fn)` compiles to different code on each side, tied
together by one id: `base64url(<app-relative path>#<name>)`, computed identically
everywhere so a browser call can find its handler.

Three transform sites share that id:

- Server (SSR). The dev module host (`start_host.rs`) and the prod server
  bundle rewrite (both through `oj_compiler::server_fns::rewrite_server`) `const NAME = createServerFn(...).handler(FN)` to pass
  `createServerRpc(meta, (opts) => NAME.__executeServer(opts))` as the first
  `.handler` argument and to export `NAME_createServerFn_handler`. The handler
  runs in process during SSR, and the export is importable by the resolver.
- Client. The dev client bundle and the prod client bundle replace that
  first argument with `createClientRpc(id)` (`rewrite_client`). The runtime treats argument one as
  the extracted function, so the browser makes an HTTP RPC to `/_serverFn/<id>`
  instead of running the handler.
- The resolver. `write_server_fn_resolver` scans `src/` for the same declarations and
  writes a manifest mapping each id to its module's exported handler. Each
  module loads on the first call through a literal relative `import()`, which
  the dev loader hook resolves and the prod server bundle bundles, as Start's
  own build resolver does. A static import would run every `*.functions.ts`
  inside the framework's import of the resolver, and a middleware that imports
  `@tanstack/react-start/server` closes a cycle there.
  It exports `getServerFnById(id)` behind the `#tanstack-start-server-fn-resolver`
  alias.

An RPC hits `/_serverFn/<id>`; the framework's `handleServerAction` looks the id
up through the resolver, runs the handler, and returns the result. In dev the
request loop hands `/_serverFn/` requests whole (method, headers, body) to the
engine; in prod the server bundle handles them directly. Both build the request
origin from the Host header so the framework's same-origin CSRF check passes.

## Testing

The adapter is covered at three levels.

Rust unit tests live beside the code they exercise:

- `oj_server/src/lib.rs` (`adapter_tests`): `is_tanstack_start_app` detection,
  `locate` preferring the app root then the publicDir, and the SPA-navigation
  rule.
- `oj/src/start_dev.rs`: `document_url`, `percent_decode`, `asset_mime`,
  `needs_css_compile`, `workspace_root`, `app_uses_tailwind`, and `classify`.
- `oj/src/start_host.rs`: asset/style classification, intent-query
  splitting, noExternal matching, CJS detection and the framework alias table.
- `oj/src/start_bundle/`: each plugin's rules beside it, and `tests.rs`, which
  drives the asset, alias, shim, glob and server-fn plugins through a real
  rolldown bundle over temp files.
- `oj_compiler`: `server_fns` (the split and the base64url ids) and
  `meta_glob` (`import.meta.glob` and `new URL(..., import.meta.url)`).

JS unit tests are `node --test e2e/unit/*.test.mjs`: `resolve-pkg` covers the
pnpm-anchor resolver, `cf-vars` the Cloudflare shim's JSONC / toml / dotenv
var parsers.

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
- Live Cloudflare KV / D1 / R2 / service bindings are not emulated; the
  `cf-server.mjs` shim provides env vars (wrangler `vars` plus `.dev.vars`) and
  an ASSETS binding backed by `dist/client`.
