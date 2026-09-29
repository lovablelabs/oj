// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Regression: a plugin `transform` can append an import to a virtual it serves
// from in-memory state populated during that transform (wyw-in-js keeps each
// module's extracted CSS in a `cssLookup` its `load` hook serves). oj persists
// the transformed module code across restarts; if it served that cached code on
// a warm start WITHOUT re-running the transform, the plugin's map would be empty
// and the virtual import would 404, breaking the module. oj must re-transform a
// cached module that imports such a virtual. This test asserts the virtual still
// serves after a warm restart (cache kept), and that a plain module (no virtual)
// keeps working too.

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
const port = 5314;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-warmvirt-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "warmvirt", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/main.tsx"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "main.tsx"), `export const app = "hi";\n`);
// A wyw-style plugin: transform records the extracted CSS for the module in an
// in-memory map keyed by an absolute virtual `.css` path, appends an import of
// it, and serves it from resolveId/load. The map is intentionally per-process
// (a fresh dev server starts empty) — exactly the state a warm start loses.
fs.writeFileSync(
  path.join(app, "vite.config.js"),
  `const cssStore = {};
export default {
  plugins: [
    {
      name: "stateful-virtual-css",
      enforce: "pre",
      resolveId(id) {
        const clean = id.split("?", 1)[0];
        return clean in cssStore ? clean : null;
      },
      load(id) {
        const clean = id.split("?", 1)[0];
        return clean in cssStore ? cssStore[clean] : null;
      },
      transform(code, id) {
        const clean = id.split("?", 1)[0];
        if (!clean.endsWith("/main.tsx")) return null;
        const virt = clean.replace(/\\.tsx$/, ".stateful.css");
        cssStore[virt] = ".injected { color: rebeccapurple }";
        return { code: code + "\\nimport " + JSON.stringify(virt) + ";\\n", map: null };
      },
    },
  ],
};
`,
);

const get = async (route) => {
  const res = await fetch(`http://localhost:${port}${route}`);
  return { status: res.status, ctype: res.headers.get("content-type") || "", body: await res.text() };
};

function startOj() {
  const proc = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
  return proc;
}
async function stopOj(proc) {
  try {
    execSync(`pkill -P ${proc.pid}`);
  } catch {}
  try {
    proc.kill("SIGKILL");
  } catch {}
  try {
    execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs -r kill -9`);
  } catch {}
  await sleep(500);
}

// Pull the virtual's URL out of the transformed main.tsx (an absolute .stateful.css path).
function virtUrlFrom(mainBody) {
  const m = mainBody.match(/import\s+"([^"]*\.stateful\.css)"/);
  assert.ok(m, `main.tsx must import the injected virtual css:\n${mainBody.slice(0, 400)}`);
  return m[1];
}

let failed = false;
let proc;
try {
  // --- COLD: transform runs, virtual is populated and served ---
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  proc = startOj();
  await waitUp(`http://localhost:${port}/`);
  const coldMain = await get("/main.tsx");
  assert.equal(coldMain.status, 200);
  const virtUrl = virtUrlFrom(coldMain.body);
  const coldCss = await get(virtUrl);
  assert.equal(coldCss.status, 200, "cold: virtual css serves");
  assert.match(coldCss.ctype, /javascript/, "cold: virtual css wrapped as a JS module");
  assert.match(coldCss.body, /rebeccapurple/, "cold: extracted CSS present");
  await stopOj(proc);

  // --- WARM: cache kept, fresh process (plugin map empty). Re-transform must
  // repopulate the virtual so it still serves instead of 404ing. ---
  assert.ok(fs.existsSync(path.join(app, ".oj-cache")), "cache persisted for the warm run");
  proc = startOj();
  await waitUp(`http://localhost:${port}/`);
  const warmMain = await get("/main.tsx");
  assert.equal(warmMain.status, 200);
  const warmVirt = virtUrlFrom(warmMain.body);
  const warmCss = await get(warmVirt);
  assert.equal(warmCss.status, 200, "WARM REGRESSION: virtual css must still serve (was 404 before the fix)");
  assert.match(warmCss.body, /rebeccapurple/, "warm: extracted CSS present");

  console.log("PASS warm-transform-virtual");
} catch (e) {
  failed = true;
  console.error("FAIL warm-transform-virtual:", e.message);
} finally {
  if (proc) await stopOj(proc);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
