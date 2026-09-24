// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Dev-side hook gating: filtered plugin hooks whose filters cannot claim a
// module skip the isolate RPC, and the gate must never skip a hook that would
// have acted. Shapes covered: a RegExp-filtered transform that matches the
// entry (marker must appear in the served module), one that matches nothing, a
// RegExp-filtered load supplying a real import's content, and a function-form
// resolveId+load virtual module (unfiltered, must keep working).

import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5327;

execSync("cargo build -p oj", { stdio: "inherit" });
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hookgate-dev-"));

let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"hookgate-dev","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/entry.js"></script></body></html>',
  );
  fs.writeFileSync(
    path.join(app, "src", "entry.js"),
    'import { virt } from "virtual:gate-info";\nimport { special } from "./data.special.js";\nconsole.log("gate", virt, special);\n',
  );
  fs.writeFileSync(path.join(app, "src", "data.special.js"), "export const special = \"placeholder\";\n");
  fs.writeFileSync(
    path.join(app, "oj.plugins.mjs"),
    `export default [
  {
    name: "gate-virtual",
    resolveId(id) {
      if (id === "virtual:gate-info") return "\\0gate-info";
    },
    load(id) {
      if (id === "\\0gate-info") return 'export const virt = "virtual-crossed";';
    },
  },
  {
    name: "gate-special-load",
    load: {
      filter: { id: /\\.special\\.js$/ },
      handler() {
        return 'export const special = "special-load-crossed";';
      },
    },
  },
  {
    name: "gate-transform-match",
    transform: {
      filter: { id: /entry\\.js$/ },
      handler(code) {
        return { code: code + '\\nconsole.log("regex-transform-crossed");', map: null };
      },
    },
  },
  {
    name: "gate-transform-never",
    transform: {
      filter: { id: /\\.does-not-exist$/ },
      handler(code) {
        return { code: code + '\\nconsole.log("never-transform-LEAKED");', map: null };
      },
    },
  },
];
`,
  );

  let stderr = "";
  child = spawn(OJ, ["dev", "--port", String(PORT)], {
    cwd: app,
    stdio: ["ignore", "ignore", "pipe"],
    detached: true,
    env: { ...process.env, OJ_DEBUG_HOOK_GATE: "1" },
  });
  child.stderr.on("data", (d) => (stderr += d.toString()));
  const t0 = Date.now();
  for (;;) {
    try {
      const res = await fetch(`http://localhost:${PORT}/`);
      if (res.ok) break;
    } catch {}
    if (Date.now() - t0 > 60000) throw new Error("dev server did not start");
    await new Promise((r) => setTimeout(r, 100));
  }

  const entry = await (await fetch(`http://localhost:${PORT}/src/entry.js`)).text();
  assert.match(entry, /regex-transform-crossed/, "matching filtered transform crossed in dev");
  assert.doesNotMatch(entry, /never-transform-LEAKED/, "non-matching filtered transform did not fire");

  const specialUrl = entry.match(/["']([^"']*data\.special\.js[^"']*)["']/)?.[1];
  assert.ok(specialUrl, "the .special import was rewritten to a servable url");
  const special = await (await fetch(`http://localhost:${PORT}${specialUrl}`)).text();
  assert.match(special, /special-load-crossed/, "filtered load supplied the module in dev");

  // The /@id/ specifier is hex-encoded, so match the route, not the name.
  const virtUrl = entry.match(/["'](\/@id\/[^"']+)["']/)?.[1];
  assert.ok(virtUrl, "the virtual import was rewritten to a servable url");
  const virt = await (await fetch(`http://localhost:${PORT}${virtUrl}`)).text();
  assert.match(virt, /virtual-crossed/, "function-form virtual module still served");

  // The gate must have SKIPPED the transform RPC for the module no filter
  // claims, and never for the entry the filter matches (output alone cannot
  // tell: the host's own per-plugin filters produce identical bytes).
  assert.match(stderr, /hook gate skipped transform for .*data\.special\.js/, "gate skipped the unclaimed module");
  assert.doesNotMatch(stderr, /hook gate skipped transform for .*entry\.js/, "gate never skipped the claimed entry");

  console.log("PLUGIN HOOK GATING DEV VERIFIED");
} catch (e) {
  failed = true;
  console.error(e);
} finally {
  // Group-kill and await exit so neither the process nor the port leaks into
  // the next CI step, then drop the temp app.
  if (child) {
    try {
      process.kill(-child.pid, "SIGKILL");
    } catch {}
    try {
      child.kill("SIGKILL");
    } catch {}
    await new Promise((r) => {
      child.once("exit", r);
      setTimeout(r, 5000);
    });
  }
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
