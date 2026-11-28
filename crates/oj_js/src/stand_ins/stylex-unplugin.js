// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// @stylexjs/unplugin as a vite.config sees it under oj, which compiles StyleX
// natively and never runs the real plugin. Every adapter returns a plugin
// that carries the options it was given; config extraction reads them, and
// oj does the rest of the plugin's work in Rust.
//
// A Start build's plugin host offers more (`globalThis.__ojStylexHost`): the
// Vite adapter then composes the package's own plugin, every hook over its
// shared rule store, with oj's compiler as the `transform`. An app that
// drives the plugin's hooks itself (two instances splitting one sheet by
// file) runs as it does under Vite; an instance a config lists as it is,
// the host hands back to oj's native path.
//
// The loader prepends the package's directory and Node's file helpers, and
// appends the exports, as ESM or CommonJS, with the module's default: the
// `stylex` object for the package entry, the adapter for a subpath such as
// `@stylexjs/unplugin/vite`.

function stylexPlugin(options) {
  const base = { name: "@stylexjs/unplugin", enforce: "pre", __ojStylexOptions: options ?? {} };
  const host = globalThis.__ojStylexHost;
  if (!host) return base;
  const real = realVitePlugin(options);
  if (!real) return base;
  const plugin = {
    ...real,
    ...base,
    transform(code, id) {
      return host.transform(real, base.__ojStylexOptions, code, id);
    },
  };
  host.created(plugin);
  return plugin;
}

// The package's own Vite adapter (lib/vite.js, CommonJS), evaluated from its
// file with a `require` of its own: the loader hands the file's siblings
// (core, consts, the dev middleware) out as they are.
let realAdapter;
function realVitePlugin(options) {
  if (realAdapter === undefined) {
    realAdapter = null;
    try {
      const file = __ojRealpath(`${__ojUnpluginDir}/lib/vite.js`);
      const module = { exports: {} };
      new Function("exports", "require", "module", "__filename", "__dirname", __ojReadFile(file, "utf8"))(
        module.exports,
        __ojCreateRequire(file),
        module,
        file,
        __ojDirname(file),
      );
      const adapter = module.exports.default ?? module.exports;
      if (typeof adapter === "function") realAdapter = adapter;
    } catch (e) {
      globalThis.__ojStylexHost?.failed?.(e);
    }
  }
  if (!realAdapter) return null;
  const plugin = realAdapter(options);
  return plugin && !Array.isArray(plugin) ? plugin : null;
}

const [bun, esbuild, farm, rolldown, rollup, rspack, unloader, vite, webpack] = Array(9).fill(stylexPlugin);
const stylex = { bun, esbuild, farm, rolldown, rollup, rspack, unloader, vite, webpack, raw: stylexPlugin };
