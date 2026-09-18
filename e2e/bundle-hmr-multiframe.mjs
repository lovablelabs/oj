// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `--bundle` HMR on a multi-frame page: the /__ws broadcast reaches every
// frame, so a patch whose boundaries belong to frame A's graph must be
// IGNORED by frame B (whose registry never registered them) instead of
// crashing B with a "module not registered" overlay, and B must still apply
// its own later patch (a skipped foreign patch is not a seq gap).

import { spawn, execSync } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// One bundle, two realms: the parent loads lazy chunk A, the iframe (same
// origin, same bundle) loads lazy chunk B, so each frame's registry only holds
// the chunks it imported (the reported shape). React components, so the
// refresh runtime treats each chunk as a valid boundary. The app lives under
// playground/ so react resolves from its node_modules.
const app = fs.mkdtempSync(path.join(repo, "playground", "mfhmr-"));
fs.mkdirSync(path.join(app, "src"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "mf", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "src", "entry.jsx"),
  `import React from "react";
import { createRoot } from "react-dom/client";
const b = new URL(location.href).searchParams.get("f") === "b";
const Chunk = React.lazy(() => (b ? import("./b.jsx") : import("./a.jsx")));
createRoot(document.getElementById("root")).render(
  <React.Suspense fallback={null}><Chunk /></React.Suspense>
);
if (!b) {
  const fr = document.createElement("iframe");
  fr.src = "/?f=b";
  document.body.appendChild(fr);
}
`,
);
// The component imports a child module; editing the CHILD makes the patch's
// boundary (the component) differ from its changed module; the boundary is
// what a foreign realm never registered.
const comp = (name) =>
  `import React from "react";
import { msg } from "./${name.toLowerCase()}-msg.js";
window.__${name} = msg;
export default function ${name}() { return <div>${name} {msg}</div>; }
`;
const msgMod = (version) => `export const msg = "${version}";\n`;
fs.writeFileSync(path.join(app, "src", "a.jsx"), comp("A"));
fs.writeFileSync(path.join(app, "src", "b.jsx"), comp("B"));
fs.writeFileSync(path.join(app, "src", "a-msg.js"), msgMod("v1"));
fs.writeFileSync(path.join(app, "src", "b-msg.js"), msgMod("v1"));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head></head><body><div id="root"></div><script type="module" src="/src/entry.jsx"></script></body></html>`,
);

const port = 5489;
let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port), "--bundle"], { stdio: "ignore" });
let browser;
try {
  for (let i = 0; i < 150; i++) {
    try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {}
    await sleep(200);
  }
  browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("console", (m) => { if (m.type() === "error" || /patch failed|not registered/.test(m.text())) errors.push(`page: ${m.text()}`); });
  page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
  await page.goto(`http://127.0.0.1:${port}/`, { waitUntil: "networkidle" });
  for (let i = 0; i < 100 && !page.frames().some((f) => f.url().includes("f=b")); i++) await sleep(100);
  const frame = page.frames().find((f) => f.url().includes("f=b"));
  assert.ok(frame, "iframe loaded");
  const waitFor = async (ctx, expr, want, label) => {
    for (let i = 0; i < 100; i++) {
      if ((await ctx.evaluate(expr)) === want) return;
      await sleep(100);
    }
    assert.equal(await ctx.evaluate(expr), want, label);
  };
  await waitFor(page, () => window.__A, "v1", "A booted");
  await waitFor(frame, () => window.__B, "v1", "B booted");
  await sleep(500);

  // Edit A's module: only the top frame's graph contains it.
  fs.writeFileSync(path.join(app, "src", "a-msg.js"), msgMod("v2"));
  for (let i = 0; i < 100 && (await page.evaluate(() => window.__A)) !== "v2"; i++) await sleep(100);
  assert.equal(await page.evaluate(() => window.__A), "v2", "A hot-applied its patch");

  // B must be untouched: no overlay, no error, still v1, not reloaded.
  await sleep(500);
  assert.equal(await frame.evaluate(() => window.__B), "v1", "B ignored A's patch");
  const overlayInB = await frame.evaluate(() => !!document.querySelector("[data-oj-overlay], #oj-error-overlay") || document.body.innerText.includes("patch failed"));
  assert.equal(overlayInB, false, "no overlay in the unrelated frame");

  // B's own edit still applies after skipping A's patch (seq not treated as a gap).
  fs.writeFileSync(path.join(app, "src", "b-msg.js"), msgMod("v2"));
  for (let i = 0; i < 100 && (await frame.evaluate(() => window.__B)) !== "v2"; i++) await sleep(100);
  assert.equal(await frame.evaluate(() => window.__B), "v2", "B applies its own later patch");

  const fatal = errors.filter((e) => /not registered|patch failed/.test(e));
  assert.deepEqual(fatal, [], `no cross-frame patch errors: ${fatal.join(" | ")}`);
  console.log("BUNDLE MULTIFRAME HMR E2E PASSED");
} catch (e) {
  failed = true;
  console.error(e);
} finally {
  if (browser) await browser.close();
  srv.kill();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
