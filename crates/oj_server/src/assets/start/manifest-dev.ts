// SPDX-License-Identifier: MIT

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

function cssUrls(): string[] {
  try {
    return JSON.parse(readFileSync(join(HERE, "css-urls.json"), "utf8"));
  } catch {
    return [];
  }
}

// The manifest contract changed across start-server-core versions, so this
// serves the union. 1.168+ reads per-route `scripts`/`css` and emits the
// client entry script tag from `scripts`; 1.167 and earlier read a top-level
// `clientEntry` (buildClientEntryScriptTag emits `import(<clientEntry>)`, so
// leaving it out ships `import(undefined)`) and per-route `assets`, ignoring
// `scripts`/`css` entirely. Each version copies only the fields it knows, so
// the extras never double-inject.
export const tsrStartManifest = () => ({
  routes: {
    __root__: {
      preloads: ["/@oj-start/client-entry.js"],
      css: cssUrls(),
      scripts: [{ attrs: { type: "module", async: true, src: "/@oj-start/client-entry.js" } }],
      assets: cssUrls().map((href) => ({
        tag: "link",
        attrs: { rel: "stylesheet", href },
      })),
    },
  },
  clientEntry: "/@oj-start/client-entry.js",
});
