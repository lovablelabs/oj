// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// @stylexjs/unplugin as a vite.config sees it under oj, which compiles StyleX
// natively and never runs the real plugin. Every adapter returns a plugin
// that carries the options it was given; config extraction reads them, and
// oj does the rest of the plugin's work in Rust. The loader appends the
// exports, as ESM or CommonJS, with the module's default: the `stylex` object
// for the package entry, the adapter for a subpath such as
// `@stylexjs/unplugin/vite`.

function stylexPlugin(options) {
  return { name: "@stylexjs/unplugin", enforce: "pre", __ojStylexOptions: options ?? {} };
}

const [bun, esbuild, farm, rolldown, rollup, rspack, unloader, vite, webpack] = Array(9).fill(stylexPlugin);
const stylex = { bun, esbuild, farm, rolldown, rollup, rspack, unloader, vite, webpack, raw: stylexPlugin };
