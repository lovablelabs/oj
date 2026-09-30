// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A dependency's package.json `browser` OBJECT can remap the package's own
// RELATIVE files for the client (`"./lib/platform/node.js":
// "./lib/platform/browser.js"`): the usual way a package ships separate Node and
// browser implementations behind one entry. Vite applies that map to relative
// imports inside the package; the dev server must not short-circuit `./x.js` to
// the file on disk, or the Node implementation (and the builtins it imports)
// reaches the browser. App-source relative imports keep their on-disk fast path.
// Run with a built target/debug/oj (or OJ_BIN).
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-browser-field-"));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};

write("package.json", JSON.stringify({ name: "browser-field-app", version: "1.0.0", type: "module" }));
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
write(
  "src/main.js",
  `import { platform } from "browser-lib";\nimport { local } from "./local.js";\nconsole.log(platform, local);\n`,
);
write("src/local.js", `export const local = "app-source";\n`);

// browser-lib: ESM, entered through `main`, whose index imports a relative file
// the `browser` object swaps. The Node file imports a builtin, as real Node
// implementations do, so a wrong pick would also fail in a browser.
write(
  "node_modules/browser-lib/package.json",
  JSON.stringify({
    name: "browser-lib",
    version: "1.0.0",
    type: "module",
    main: "index.js",
    browser: { "./lib/platform/node.js": "./lib/platform/browser.js" },
  }),
);
write("node_modules/browser-lib/index.js", `export { platform } from "./lib/platform/node.js";\n`);
write("node_modules/browser-lib/lib/platform/node.js", `import "events";\nexport const platform = "node";\n`);
write("node_modules/browser-lib/lib/platform/browser.js", `export const platform = "browser";\n`);

const port = 5231;
const server = spawn(oj, ["dev", app, "--port", String(port), "--host=127.0.0.1"], {
  stdio: ["ignore", "pipe", "pipe"],
});
let log = "";
server.stdout.on("data", (d) => (log += d));
server.stderr.on("data", (d) => (log += d));
let failed = false;
try {
  const base = `http://127.0.0.1:${port}`;
  await waitUp(`${base}/`);

  const entry = await (await fetch(`${base}/node_modules/browser-lib/index.js`)).text();
  assert.match(
    entry,
    /\/node_modules\/browser-lib\/lib\/platform\/browser\.js/,
    `the browser map was not applied to the dependency's relative import:\n${entry}`,
  );
  assert.doesNotMatch(entry, /platform\/node\.js/, `the Node file is still imported:\n${entry}`);

  // App source is unaffected: its relative import still resolves to the file.
  const main = await (await fetch(`${base}/src/main.js`)).text();
  assert.match(main, /"\/src\/local\.js"/, `app-source relative import changed:\n${main}`);
  console.log("dep-browser-field-relative: ok");
} catch (err) {
  failed = true;
  console.error(log.slice(-4000));
  console.error("dep-browser-field-relative FAILED:", err && err.stack ? err.stack : err);
} finally {
  server.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
