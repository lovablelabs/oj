# SolidJS support (fork: david1gp/oj, branch `solid-support`)

Status: **plain Vite+Solid works; Solid Start works in dev and prod per the
dedicated fixture (dev + prod gates pass). Homepage dev functional
acceptance PASSED on original code with existing cache (no manual clear),
and homepage production passes (all 9 routes + robots,
theme/language/hard-reload). Visual review PASS on desktop/mobile
screenshots: styled hero/services and dark theme visibly verified with
zero browser console/page errors. No version bump needed for the fork.**

## What works

- Plain Vite + Solid: `oj dev` with `solid-refresh` HMR (hot-swap, no reload);
  `oj build` + preview render. Proven by `e2e/solid.mjs`.
- Solid Start dev + prod: `node e2e/solid-start.mjs` passes both modes with
  no checks skipped — raw SSR body + head ordering, browser
  hydration/reactivity, SPA navigation without reload, loader-error
  (`/boom`) and default render-error (`/render-boom`) boundaries, plus
  route-file and imported-component edits verified on raw SSR and in the
  browser with restore.
- Homepage dev (original code, existing cache, no manual clear): functional
  acceptance PASSED — all 9 routes true SSR + route titles in raw HTML with
  correct browser title, Tailwind retained, theme/language SPA switching
  with no reload, zero browser warnings/errors, imported Hero edit shows
  fresh raw SSR + browser and restores pristine.
- Homepage production: `oj build` emits `server.mjs` + client assets; the
  built server renders all 9 routes + robots with Vite-order titles,
  content, clean hydration, theme/language switching, and hard-reload.
- Cross-framework checks stay green: `e2e/start.mjs`, `e2e/solid.mjs`,
  `e2e/svelte.mjs`.

## Implemented architecture

- `StartFramework` detection (`start_framework()`, `start_package()` /
  `router_target()` / entry filenames); `main.rs` dispatches dev + build;
  `start_dev` / `start_build` keep React wrappers (`*_with_framework`).
- Framework-threaded Start pipeline: `framework_aliases`,
  `rewrite_server_fns`, `StartHost`, `StartEngine`, `script_env` /
  `start_script_env` (`OJ_START_PKG`, `OJ_ROUTER_TARGET`, `OJ_CLIENT_ENTRY`,
  `OJ_SERVER_ENTRY_FILE`), per-framework codegen and bundle cache keys
  (`route-tree-solid`, framework-salted bundle store).
- `assets/start/`: `client-entry-solid.tsx`, `server-entry-solid.tsx`,
  `start-pkg.mjs`, `solid-transform.mjs` (babel-preset-solid dom/ssr +
  hydratable, rolldown JSX preserve, `solid`-first conditions), `build.mjs`,
  `bundle-client.mjs`, `cf-build.mjs`, `generate.mjs` (`target`, footer,
  fallbacks); `vite-plugin-bridge.mjs` reads them.
- Query-preserving plugin identity: full queried ids including
  `?tsr-split=component` through plugin load/transform; clean paths for
  filesystem/compiler; explicit asset-query handling.
- Client/server transform parity: dev client bundling runs TanStack router
  reference/split/shared transforms (plus route-tree client plugin and
  generator lifecycle) before Solid compilation, with Solid's transform
  ordered after route splitting so SSR lazy wrappers and client components
  share hydration topology.
- Fresh SSR after edits: `StartEngine::reload` and `revalidate` (when modules
  were dropped) respawn the isolate under the engine write lock, so the
  fresh router graph is re-imported without framework-package changes.
- Cache correctness: persistent Start client bundle keys include embedded
  builder/bridge/transform inputs (content-based invalidation, no cache
  deletion); client split chunks and HTML import the same emitted
  `client-entry.js` by enforced output naming (no double Solid execution).
- Resolver correctness: `solid`-first export conditions preserved through
  the merged Vite config (pinned by
  `merge_keeps_solid_first_resolve_conditions`); `vite:alias` fallback in
  the plugin `resolveId` chain; dep-aware imports in plugin-loaded source
  (single prebundled Solid instance, working HMR registry).

## Error semantics

- Handled (framework-caught) route errors render the Solid Router error
  boundary with meaningful fallback markup; the framework may return HTTP
  200 — oj preserves that status and never imposes 500.
- Uncaught Start handler / body-read errors return HTTP 500 via the existing
  Rust forwarding path.
- Healthy-route gates require real body elements after stripping script
  payloads; the default global render-error boundary (fragment without a
  document shell) is asserted separately on its fallback markup.

## How to verify

Fork install (no version bump, not a release build):

```sh
cargo install --git https://github.com/david1gp/oj --branch solid-support oj
```

```sh
cargo build -p oj
# fixture deps (bun.lock only):
(cd e2e/fixtures/solid-start-app && bun install)
# CI equivalent (no bun on runners):
npm install --prefix e2e/fixtures/solid-start-app
# dedicated Solid Start fixture, full run (dev + prod, nothing skipped):
node e2e/solid-start.mjs
node e2e/solid-start.mjs --dev
node e2e/solid-start.mjs --prod
# targeted cross-framework checks:
node e2e/solid.mjs
node e2e/start.mjs
node e2e/svelte.mjs
# homepage production (passes: all 9 routes + robots, theme/language/hard-reload):
(cd ~/personal/homepage && ../oj/target/debug/oj build . --out /tmp/oj-solid-dist)
(cd /tmp/oj-solid-dist && PORT=5200 node server.mjs &)
curl -s http://localhost:5200/ | head -c 300
# homepage dev (passes on original code, existing cache, no manual clear):
(cd ~/personal/homepage && ../oj/target/debug/oj dev --port 5199 . &)
curl -s http://localhost:5199/ | head -c 300
```
