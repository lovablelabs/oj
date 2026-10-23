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
]);

// Dev-tooling plugins oj cannot host: they drive a full Vite dev server (ws
// error overlay, file watcher, a worker running tsc/eslint) that oj does not
// provide, and they have no effect on the served or built output. Left in, the
// worker they spawn throws asynchronously; skip them outright.
export const OJ_UNSUPPORTED_PLUGIN_NAMES = new Set(["vite-plugin-checker"]);
