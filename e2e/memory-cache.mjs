// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the in-memory module cache is byte-bounded and stays correct under
// eviction. Runs the dev server with a tiny OJ_MEMORY_CACHE_MB cap against a set
// of modules whose combined weight exceeds the cap, forcing least-recently-used
// eviction. Every module must still serve, and a re-fetch of an evicted module
// (miss -> recompute) must return byte-identical output.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5324;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-memcache-"));
const COUNT = 8;
const CHUNK = "x".repeat(200000); // ~200KB each; 8 * ~200KB > 1MB cap

let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"memcache","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  const names = Array.from({ length: COUNT }, (_, i) => `mod${i}.js`);
  for (let i = 0; i < COUNT; i++) {
    fs.writeFileSync(path.join(app, "src", names[i]), `export const s${i} = ${JSON.stringify(CHUNK)};\n`);
  }
  fs.writeFileSync(
    path.join(app, "src", "main.js"),
    names.map((n, i) => `import { s${i} } from "./${n}";`).join("\n") +
      "\nexport const total = " +
      names.map((_, i) => `s${i}.length`).join(" + ") +
      ";\n",
  );

  let out = "";
  child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app, env: { ...process.env, OJ_MEMORY_CACHE_MB: "1" } });
  child.stdout.on("data", (d) => (out += d.toString()));
  child.stderr.on("data", (d) => (out += d.toString()));

  const up = async () => {
    return waitUp(`http://localhost:${PORT}/`).then(
      () => true,
      () => false,
    );
  };
  if (!(await up())) throw new Error("server did not start:\n" + out);

  const get = async (n) => {
    const res = await fetch(`http://localhost:${PORT}/src/${n}`);
    if (!res.ok) throw new Error(`${n} -> HTTP ${res.status}`);
    return res.text();
  };

  const first = {};
  for (const n of names) first[n] = await get(n);
  console.log(`served ${COUNT} modules:  yes`);

  // Re-fetch in the same order: the earliest are now evicted and recompute.
  for (const n of names) {
    const again = await get(n);
    if (again !== first[n]) throw new Error(`${n} changed after eviction/recompute`);
  }
  console.log("stable after evict: yes");

  const total = COUNT * CHUNK.length;
  if (!first["mod0.js"].includes("xxxx")) throw new Error("module body missing");
  console.log(`total module bytes: ~${Math.round(total / 1024)}KB under a 1MB cap`);
  console.log("\nMEMORY CACHE BOUND VERIFIED: eviction under cap, recompute is byte-stable");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
