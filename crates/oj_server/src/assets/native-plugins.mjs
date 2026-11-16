// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The app plugins oj does not run in its plugin host, by name: the host
// leaves them out, and config extraction counts what is left, so an app whose
// every plugin is one of these boots no plugin host at all.

export const OJ_NATIVE_PLUGIN_NAMES = new Set([
  "vite:react-babel",
  "vite:react-refresh",
  "vite:react-swc",
  "vite:react-swc:resolve-runtime",
  // rolldown-vite's @vitejs/plugin-react does its client Fast Refresh wrapping by
  // returning the rolldown builtin `builtin:vite-react-refresh-wrapper` from this
  // plugin's `applyToEnvironment`. oj already reimplements React refresh natively
  // (its own preamble + refresh runtime), so this is redundant; left active the
  // builtin's transform runs and throws "Missing field `moduleType`" (oj drives
  // transform hooks with Vite's { ssr } options, not the builtin's rolldown
  // { moduleType } extra args), which 500s the client entry and stops the browser
  // from ever hydrating. Skipping the parent here removes it before
  // applyToEnvironment can introduce the builtin.
  "vite:react:refresh-wrapper",
  // @vitejs/plugin-react 6 and plugin-react-swc 4: `config-post` only turns
  // oxc's JSX refresh off when Fast Refresh is skipped, and the bundled-dev
  // preamble hooks (`refresh-fbm`, `refresh-bundled-dev`) inject only under
  // `experimental.bundledDev`, which oj does not run. The virtual preamble
  // (`@vitejs/plugin-react/preamble`, `@vitejs/plugin-react-swc/preamble`) is
  // oj's own refresh preamble: the module resolver maps both ids to it.
  "vite:react:config-post",
  "vite:react-refresh-fbm",
  "vite:react-swc:refresh-bundled-dev",
  "vite:react-virtual-preamble",
  // @stylexjs/unplugin: oj compiles StyleX natively (oj_stylex) with the
  // options extraction records, and serves the plugin's dev endpoints itself.
  "@stylexjs/unplugin",
  // An editor's HMR gate (/__hmr_gate, /__hmr_flush): oj gates natively, and
  // config extraction turns that gate on when the app has this plugin.
  // Hosted as well, it would gate behind oj's own gate.
  "hmr-gate",
  // @vitejs/plugin-basic-ssl: the config sees oj's stand-in (oj_js
  // stand_ins/basic-ssl.js), which records the options; oj makes the
  // plugin's certificate itself (oj_server basic_ssl.rs) and the extractor
  // carries the options over as `basicSsl`.
  "vite:basic-ssl",
]);

// Dev-tooling plugins oj cannot host: they drive a full Vite dev server (ws
// error overlay, file watcher, a worker running tsc/eslint) that oj does not
// provide, and they have no effect on the served or built output. Left in, the
// worker they spawn throws asynchronously; skip them outright.
export const OJ_UNSUPPORTED_PLUGIN_NAMES = new Set(["vite-plugin-checker"]);

// Start's router plugin: the code splitter pair it adds per environment
// (client and server, sharing one route list) and the router generator, whose
// run fills that list (`routesByFile`, the files the route tree imports).
export function isRouteSplitting(name) {
  return typeof name === "string" && /^tanstack-router:code-splitter:|^tanstack:router-generator$/.test(name);
}

// Framework plugins oj reimplements natively (vite:*, TanStack's): a Start
// host leaves them out, and config extraction counts their config hooks as
// oj's work. A name that is not a string (`name: null` on a disabled
// factory) is never one of them.
export function isReimplemented(name) {
  if (typeof name !== "string") return false;
  // Start's import protection has no oj counterpart: it runs as Start ships it.
  if (name.startsWith("tanstack-start-core:import-protection")) return false;
  // Nor have Start's route code splitters (one per environment), nor the
  // router generator whose run gives them the routes to split.
  if (isRouteSplitting(name)) return false;
  return name.startsWith("vite:") || /^tanstack[-:]/.test(name);
}

// Start's manifest plugins: one captures the client bundle's chunks in its
// generateBundle, the other answers the server bundle's
// `tanstack-start-manifest:v` with the routes' preloads and scripts built
// from them. A build runs them as Start ships them, so the manifest is the
// installed Start's own and a plugin's transform sees the module under
// Vite's id; dev keeps oj's manifest (theirs is a stub there).
export function isStartManifest(name) {
  return typeof name === "string" && name.startsWith("tanstack-start:start-manifest");
}

// Vite's asyncFlatten over arraify, falsy entries dropped: plugin options may
// nest arrays and promises, and a promise may resolve to an array of plugins
// (`Promise.resolve([tanstackStart()])`), so each round awaits what the last
// one produced until no thenable is left. A thenable has a callable `then`
// (Promise.all's own test): a plugin object carrying some other `then` field
// is a value, not a reason to loop forever.
export async function flattenPlugins(value) {
  let list = Array.isArray(value) ? value : [value];
  do {
    list = (await Promise.all(list)).flat(Infinity);
  } while (list.some((v) => v && typeof v.then === "function"));
  return list.filter(Boolean);
}
