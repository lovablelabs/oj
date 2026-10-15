// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Loads the app's vite.config the way Node's Vite does. Vite branches on
// `process.versions.deno` in loadConfigFromFile: under Deno (oj's runtime) it
// treats every config as ESM and imports the bundled config from
// `<config>.timestamp-<ms>-<hash>.mjs` beside the source, not from
// node_modules/.vite-temp (vitejs/vite#18823, for Deno-native `npm:` imports).
// That dropped a short-lived file into the app root on every config load, and
// Start loads the config again in every client rebundle. oj only runs Node
// projects' configs, so the Deno branch is switched off for these calls.

let depth = 0;
let deno;

/** Runs `load` (a Vite call that reads the config file) as Node's Vite would.
 *  Overlapping calls share one window; the last to finish restores. */
export async function asNode(load) {
  if (depth++ === 0) {
    deno = Object.getOwnPropertyDescriptor(process.versions, "deno");
    if (deno) delete process.versions.deno;
  }
  try {
    return await load();
  } finally {
    if (--depth === 0 && deno) Object.defineProperty(process.versions, "deno", deno);
  }
}
