// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite's js-sourcemap security regression: a dependency shipping a sourcemap
// whose `sources` climb out of the package (../../../secret) must never get
// the target file's CONTENTS inlined into anything the dev server serves
// (sourcesContent of the served module's map, or a served .map file). The
// path may survive as a string; the bytes behind it must not.

import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5498;

const SECRET = "TOP-SECRET-a12ff342-DO-NOT-SERVE";
const parent = fs.mkdtempSync(path.join(os.tmpdir(), "oj-map-leak-"));
const app = path.join(parent, "app");
const cleanup = () => fs.rmSync(parent, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
fs.writeFileSync(path.join(parent, "secret.txt"), `${SECRET}\n`);
const w = (rel, s) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), s);
};
w("package.json", JSON.stringify({ name: "map-leak", version: "1.0.0", type: "module" }));
w("index.html", `<!doctype html><html><body><script type="module" src="/src/main.js"></script></body></html>`);
w("src/main.js", 'import { evil } from "evil-dep";\nwindow.__evil = evil;\n');
w("node_modules/evil-dep/package.json", JSON.stringify({ name: "evil-dep", version: "1.0.0", main: "index.js", type: "module" }));
w("node_modules/evil-dep/index.js", 'export const evil = "ok";\n//# sourceMappingURL=index.js.map\n');
// sources escape the package AND the app root; sourcesContent is null so any
// server that "helpfully" fills it in must read ../../../secret.txt to do so.
w(
  "node_modules/evil-dep/index.js.map",
  JSON.stringify({
    version: 3,
    file: "index.js",
    sources: ["../../../secret.txt"],
    sourcesContent: null,
    names: [],
    mappings: "AAAA",
  }),
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "ignore", "pipe"], detached: true });
let log = "";
srv.stderr.on("data", (d) => (log += d));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const get = async (u) => {
  const res = await fetch(`http://localhost:${PORT}${u}`);
  return { status: res.status, text: await res.text() };
};
// Every inline base64 sourcemap in a body, decoded.
const inlineMaps = (text) =>
  [...text.matchAll(/sourceMappingURL=data:application\/json[^,]*;base64,([A-Za-z0-9+/=]+)/g)].map((m) =>
    Buffer.from(m[1], "base64").toString("utf8"),
  );
try {
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`http://localhost:${PORT}/`)).ok) break;
    } catch {}
    if (srv.exitCode !== null) throw new Error(`oj exited early\n${log.slice(-3000)}`);
    await sleep(200);
  }
  const main = await get("/src/main.js");
  if (main.status !== 200) throw new Error(`main.js returned ${main.status}\n${log.slice(-2000)}`);
  // The dep's served URL, however oj spelled the rewrite.
  const depUrl = main.text.match(/["']([^"']*evil-dep[^"']*)["']/)?.[1];
  if (!depUrl) throw new Error(`could not find the rewritten evil-dep URL in:\n${main.text}`);
  const dep = await get(depUrl.startsWith("/") ? depUrl : `/${depUrl}`);
  if (dep.status !== 200) throw new Error(`dep module returned ${dep.status} for ${depUrl}`);

  const suspects = [dep.text, ...inlineMaps(dep.text)];
  // A separately served .map beside the module, if the server exposes one.
  for (const cand of [`${depUrl}.map`, depUrl.replace(/\.js(\?|$)/, ".js.map$1")]) {
    try {
      const m = await get(cand.startsWith("/") ? cand : `/${cand}`);
      if (m.status === 200) suspects.push(m.text);
    } catch {}
  }
  for (const s of suspects) {
    if (s.includes(SECRET)) {
      throw new Error(`the dep's malicious sourcemap leaked file contents into a served response:\n${s.slice(0, 800)}`);
    }
  }
  console.log(`sourcemap-source-leak: ${suspects.length} served payload(s) checked, no traversal leak`);
} catch (e) {
  failed = true;
  console.error("SOURCEMAP SOURCE LEAK FAILED:", e.message ?? e);
} finally {
  try {
    process.kill(-srv.pid, "SIGKILL");
  } catch {
    try {
      srv.kill("SIGKILL");
    } catch {}
  }
  cleanup();
}
process.exit(failed ? 1 : 0);
