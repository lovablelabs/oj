// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// End-to-end: oj serves the @lingui/*/macro runtime identity shim and rewrites
// the three macro entrypoints to it, so an app that imports the (normally
// build-time) lingui macros still loads and renders source strings. Exercises
// every shim export through a real browser, plus the HTTP wiring.

import { spawn, execSync } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const port = 5291;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-lingui-"));
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "lingui-shim-app", version: "1.0.0" }),
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>lingui</title></head><body><div id="root"></div><script type="module" src="/main.js"></script></body></html>`,
);
// Imports all three macro entrypoints. None of the @lingui packages are
// installed: oj must intercept the specifiers by name and serve the shim, so a
// missing/heavy real macro package is never resolved.
fs.writeFileSync(
  path.join(app, "main.js"),
  `import { t, msg, defineMessage, plural, selectOrdinal, select } from "@lingui/core/macro";
import { Trans, Plural, Select, SelectOrdinal, useLingui } from "@lingui/react/macro";
import { t as ut } from "@lingui/macro";
const name = "World";
const bound = useLingui();
const out = {
  t: t\`Hello \${name}\`,
  umbrellaT: ut\`Umbrella \${name}\`,
  msg: msg\`Bye \${name}\`,
  defineMessage: defineMessage\`Def \${name}\`,
  plural2: plural(2, { one: "# item", other: "# items" }),
  plural1: plural(1, { one: "# item", other: "# items" }),
  ordinal: selectOrdinal(1, { one: "#st", other: "#th" }),
  select: select("a", { a: "Apple", other: "Other" }),
  hookT: bound.t\`Hi \${name}\`,
  hookI18n: bound.i18n._("passthrough"),
  trans: Trans({ message: "TransMessage" }),
  transChildren: Trans({ children: "TransChild" }),
  pluralC: Plural({ value: 2, one: "# file", other: "# files" }),
  selectOrdinalC: SelectOrdinal({ value: 1, one: "#st", other: "#th" }),
  selectC: Select({ value: "a", a: "AA", other: "OO" }),
};
window.__OJ_LINGUI = out;
document.getElementById("root").textContent = JSON.stringify(out);
`,
);

const get = async (route) => {
  const res = await fetch(`http://localhost:${port}${route}`);
  return { status: res.status, ctype: res.headers.get("content-type") || "", body: await res.text() };
};

let server;
let stderr = "";
let failed = false;
try {
  server = spawn(oj, ["dev", app, "--port", String(port)], { stdio: ["ignore", "ignore", "pipe"] });
  server.stderr.on("data", (d) => (stderr += d.toString()));
  await waitUp(`http://localhost:${port}/`);

  // --- HTTP wiring ---
  const shim = await get("/@oj/lingui-macro-shim.js");
  assert.equal(shim.status, 200, "shim route serves");
  assert.match(shim.ctype, /javascript/, "shim served as JS module");
  assert.match(shim.body, /export function t\b/, "shim exports t");
  assert.match(shim.body, /export function useLingui\b/, "shim exports useLingui");

  const main = await get("/main.js");
  assert.equal(main.status, 200);
  const shimUrl = "/@oj/lingui-macro-shim.js";
  assert.ok(
    main.body.includes(`"@lingui/core/macro"`) === false,
    "the raw @lingui/core/macro specifier must be rewritten away",
  );
  const rewrites = (main.body.match(/\/@oj\/lingui-macro-shim\.js/g) || []).length;
  assert.ok(rewrites >= 3, `all three macro imports rewritten to the shim (got ${rewrites})`);

  assert.match(stderr, /runtime identity shim/, "oj warns once about the degraded shim");

  // --- real browser end-to-end: every export executes through oj ---
  let chromium;
  try {
    ({ chromium } = createRequire(path.join(here, "x.js"))("playwright"));
  } catch {
    console.log("SKIP browser check: playwright not resolvable");
  }
  if (chromium) {
    const browser = await chromium.launch();
    try {
      const page = await browser.newPage();
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e.message || e)));
      await page.goto(`http://localhost:${port}/`, { waitUntil: "load", timeout: 30000 });
      await page.waitForFunction("window.__OJ_LINGUI !== undefined", { timeout: 15000 });
      const out = await page.evaluate(() => window.__OJ_LINGUI);
      assert.deepEqual(errors, [], `no page errors: ${errors.join("; ")}`);
      assert.equal(out.t, "Hello World");
      assert.equal(out.umbrellaT, "Umbrella World");
      assert.equal(out.msg, "Bye World");
      assert.equal(out.defineMessage, "Def World");
      assert.equal(out.plural2, "2 items");
      assert.equal(out.plural1, "1 item");
      assert.equal(out.ordinal, "1st");
      assert.equal(out.select, "Apple");
      assert.equal(out.hookT, "Hi World");
      assert.equal(out.hookI18n, "passthrough");
      assert.equal(out.trans, "TransMessage");
      assert.equal(out.transChildren, "TransChild");
      assert.equal(out.pluralC, "2 files");
      assert.equal(out.selectOrdinalC, "1st");
      assert.equal(out.selectC, "AA");
      const rootText = await page.$eval("#root", (el) => el.textContent);
      assert.ok(rootText.includes("Hello World"), "shim output rendered into the DOM");
    } finally {
      await browser.close();
    }
  }

  console.log("PASS lingui-macro-shim");
} catch (e) {
  failed = true;
  console.error("FAIL lingui-macro-shim:", e.message);
  if (stderr) console.error("--- oj stderr ---\n" + stderr.slice(0, 2000));
} finally {
  if (server) server.kill();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
