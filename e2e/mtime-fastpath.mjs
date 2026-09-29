// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the stat-first request fast-path: a warm re-request of an unchanged
// module reuses the cached key (skipping read+hash) and stays correct, while an
// edit (mtime + size change) invalidates it so the next request serves the new
// content. Guards against the fast-path ever serving stale bytes after a save.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { settles, waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5325;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-mtime-"));
const mod = path.join(app, "src", "mod.js");

let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"mtime","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/mod.js"></script></body></html>',
  );
  fs.writeFileSync(mod, 'export const marker = "VALUE_A";\n');

  child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app });
  let out = "";
  child.stdout.on("data", (d) => (out += d.toString()));
  child.stderr.on("data", (d) => (out += d.toString()));

  const get = async () => (await fetch(`http://localhost:${PORT}/src/mod.js`)).text();
  const up = async () => {
    return waitUp(`http://localhost:${PORT}/`).then(() => true, () => false);
  };
  if (!(await up())) throw new Error("server did not start:\n" + out);

  const a1 = await get();
  const a2 = await get();
  if (!a1.includes("VALUE_A") || !a2.includes("VALUE_A")) throw new Error("initial content wrong");
  if (a1 !== a2) throw new Error("warm re-request differs from first");
  console.log("warm re-request:    stable");

  fs.writeFileSync(mod, 'export const marker = "VALUE_B_LONGER";\nexport const extra = 1;\n');

  const updated = await settles(async () => {
    const b = await get();
    return b.includes("VALUE_B_LONGER") && !b.includes("VALUE_A");
  });
  if (!updated) throw new Error("stale content served after edit (fast-path did not invalidate)");
  console.log("edit invalidation:  fresh content served");
  console.log("\nMTIME FAST-PATH VERIFIED: warm reuse is correct, edits are never stale");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
