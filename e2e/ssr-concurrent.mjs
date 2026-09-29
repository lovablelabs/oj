// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Generic `oj dev --ssr`: requests reach the module runner concurrently over
// its loopback HTTP server (a loader that fetches the app's own dev server
// mid-render no longer deadlocks behind itself, as with Vite's module runner)
// and action bodies arrive as intact bytes with no size cap.

import { spawn, execSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "ssr-self-fetch");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5237);
const base = `http://localhost:${PORT}`;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const withTimeout = (p, ms, what) =>
  Promise.race([
    p,
    new Promise((_, rej) =>
      setTimeout(() => rej(new Error(`${what} did not answer within ${ms}ms (runner deadlock?)`)), ms),
    ),
  ]);

const srv = spawn(oj, ["dev", app, "--ssr", "src/entry-server.ts", "--port", String(PORT)], {
  stdio: "ignore",
  env: { ...process.env, OJ_E2E_SELF_PORT: String(PORT) },
});
try {
  let home = null;
  await waitUp(`${base}/`, {
    until: async (res) => res.ok && (home = await res.text()) !== undefined,
  }).catch(() => {});
  must(home && home.includes('"page":"home"'), `ssr dev did not come up:\n${home}`);
  must(home.includes("<title>/</title>"), "head() output missing from the document");

  // A render whose loader fetches this very server.
  const self = await withTimeout(fetch(`${base}/self`), 15000, "GET /self");
  const selfHtml = await self.text();
  must(
    self.status === 200 && selfHtml.includes('"inner":{"page":"about"'),
    `self-fetching loader failed:\n${selfHtml}`,
  );
  console.log("ssr-concurrent: a loader fetching its own server during render answers");

  // Several renders in flight at once, all of them self-fetching.
  const many = await withTimeout(
    Promise.all([1, 2, 3, 4].map(() => fetch(`${base}/self`).then((r) => r.status))),
    20000,
    "4 concurrent GET /self",
  );
  must(
    many.every((s) => s === 200),
    `concurrent renders failed: ${many}`,
  );

  // A binary action body: bytes intact, well past any small-body path.
  const blob = randomBytes(3 * 1024 * 1024);
  blob[0] = 0xff;
  blob[blob.length - 1] = 0x00;
  let sum = 0;
  for (const b of blob) sum = (sum + b) % 65521;
  const posted = await fetch(`${base}/echo`, {
    method: "POST",
    headers: { "oj-loader": "1", "content-type": "application/octet-stream" },
    body: blob,
  });
  must(posted.status === 200, `POST /echo returned ${posted.status}`);
  const echo = (await posted.json()).echo;
  must(
    echo && echo.bytes === blob.length && echo.first === 0xff && echo.last === 0x00 && echo.sum === sum,
    `action body did not arrive intact: ${JSON.stringify(echo)}`,
  );
  console.log("ssr-concurrent: 3 MB binary action body arrives intact");
  console.log("\nSSR CONCURRENT PASSED");
} catch (e) {
  console.error("\nSSR CONCURRENT FAILED:", e.message);
  process.exitCode = 1;
} finally {
  srv.kill("SIGKILL");
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
}
