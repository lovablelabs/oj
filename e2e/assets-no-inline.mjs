// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// `?no-inline` (Vite's asset plugin): the asset's URL, never a data URL, even
// under assetsInlineLimit; in dev it behaves like `?url`. Run with a built
// target/debug/oj.
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-no-inline-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "no-inline", type: "module" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import inlined from "./small.png?url";\nimport kept from "./small.png?no-inline";\nwindow.__INLINED = inlined;\nwindow.__KEPT = kept;\n`,
);
// Well under the 4096 byte default inline limit.
fs.writeFileSync(path.join(app, "src", "small.png"), Buffer.alloc(64, 7));

let failed = false;
function check(label, ok, detail) {
  if (!ok) {
    failed = true;
    console.error(`FAIL ${label}: ${detail}`);
  } else {
    console.log(`ok   ${label}`);
  }
}

// Dev: `?no-inline` is a URL module.
{
  const port = 6306;
  const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
  try {
    const up = await waitUp(`http://localhost:${port}/`).then(() => true, () => false);
    check("dev server starts", up, "no response on 6306");
    if (up) {
      const main = await (await fetch(`http://localhost:${port}/src/main.js`)).text();
      check("dev: the ?no-inline import is kept as an asset import", /small\.png\?no-inline/.test(main), main);
      const mod = await (await fetch(`http://localhost:${port}/src/small.png?no-inline`)).text();
      check("dev: ?no-inline module exports the url", /export default "\/src\/small\.png"/.test(mod), mod);
    }
  } finally {
    srv.kill();
  }
}

// Build: `?url` inlines the small asset, `?no-inline` emits it as a file.
{
  const r = spawnSync(oj, ["build", app], { cwd: repo, encoding: "utf8" });
  check("build succeeds", r.status === 0, `${r.stdout}\n${r.stderr}`);
  if (r.status === 0) {
    const assets = path.join(app, "dist", "assets");
    const files = fs.readdirSync(assets);
    const js = files.filter((f) => f.endsWith(".js")).map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("\n");
    check("build: ?url inlines under the limit", /data:image\/png;base64,/.test(js), js.slice(-300));
    check("build: ?no-inline emits a hashed file", files.some((f) => /^small-[^.]+\.png$/.test(f)), files.join(", "));
    check("build: ?no-inline resolves to the file url", /small-[^"'`\/]+\.png/.test(js), js.slice(-300));
  }
}

fs.rmSync(app, { recursive: true, force: true });
if (failed) process.exit(1);
console.log("ASSETS-NO-INLINE E2E PASSED");
