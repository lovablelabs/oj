// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite runs the plugins' resolveId before its own resolver for every import,
// relative ones included. oj resolves natively first; a relative or absolute
// import that a plugin's object-form `resolveId.filter.id` claims now goes to
// the plugins first (the `./icon.svg?react` remap), and when they decline, the
// disk resolver still serves the file.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 6412;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-resolverel-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "resolverel", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "icon.svg"), `<svg xmlns="http://www.w3.org/2000/svg"></svg>\n`);
fs.writeFileSync(path.join(app, "src", "plain.js"), `export const plain = "PLAIN";\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import Icon from "./icon.svg?react";\nimport { plain } from "./plain.js?claimed";\nwindow.__icon = Icon; window.__plain = plain;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import path from "node:path";
export default [{
  name: "svg-react",
  resolveId: {
    filter: { id: [/\\.svg\\?react$/, /\\?claimed$/] },
    handler(source, importer) {
      // Claimed by the filter but declined here: oj's resolver must take over.
      if (source.endsWith("?claimed")) return null;
      return path.resolve(path.dirname(importer), source.replace(/\\?react$/, "")) + "?react-component";
    },
  },
  load: {
    filter: { id: /\\?react-component$/ },
    handler() { return "export default 'REACT_SVG_COMPONENT';"; },
  },
}];\n`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const main = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  const ids = [...main.matchAll(/from\s*"(\/@id\/[^"]+)"/g)].map((m) => m[1]);
  assert.equal(ids.length, 2, `both claimed relative imports route through the plugins:\n${main}`);

  const remapped = await (await fetch(`http://localhost:${PORT}${ids[0]}`)).text();
  assert.match(
    remapped,
    /REACT_SVG_COMPONENT/,
    `the plugin's resolveId remapped ./icon.svg?react and its load served it:\n${remapped}`,
  );

  const declined = await fetch(`http://localhost:${PORT}${ids[1]}`);
  assert.equal(declined.status, 200, "a declined claim still resolves");
  assert.match(await declined.text(), /PLAIN/, "the disk resolver served the file the plugin declined");
  console.log("PLUGIN-RESOLVE-RELATIVE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-RESOLVE-RELATIVE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
