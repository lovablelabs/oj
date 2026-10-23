# Vendored Vite build-time helpers

These files are byte-exact copies, kept out of oxfmt (`.oxfmtrc.json`): the
minifier's output depends on its input text, and oj's build output is meant to
be byte-identical to Vite's.

- `detect-script-rel.js`, `preload.js`, `is-css-preload-url.js`,
  `preload-once.js`: the `Function.prototype.toString()` text of the functions
  Vite 8.3.5 builds its `\0vite/preload-helper.js` module from
  (`getPreloadCode` in `vite/dist/node/chunks/node.js`). MIT, Copyright (c)
  2019-present, VoidZero Inc. and Vite contributors.
- `module-preload-polyfill.js`: rolldown 1.2.15's
  `crates/rolldown_plugin_vite_module_preload_polyfill/src/module-preload-polyfill.js`,
  the polyfill Vite 8 builds with. MIT, Copyright (c) 2024-present VoidZero
  Inc. & Contributors.

To update, re-extract them from the Vite release (and its rolldown) oj tracks.
