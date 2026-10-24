// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The watcher losing events (an inotify queue overflow) must not lose edits:
// as Zed does, the overflow's rescan diffs the tree against what the watcher
// knew, so a module edited while its event was dropped still hot-updates, and
// a directory created meanwhile is watched again. Linux only, and only with
// a small queue, which makes a burst of writes overflow it:
//   sudo sysctl -w fs.inotify.max_queued_events=1
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 5517;

const queue = (() => {
  try {
    return Number(fs.readFileSync("/proc/sys/fs/inotify/max_queued_events", "utf8"));
  } catch {
    return Infinity;
  }
})();
if (process.platform !== "linux" || queue > 256) {
  console.log("SKIP watch rescan: needs Linux with a small inotify queue");
  console.log("  enable with: sudo sysctl -w fs.inotify.max_queued_events=1");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-watch-rescan-"));
const src = path.join(app, "src");
fs.mkdirSync(src, { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "watch-rescan", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><body><script type="module" src="/src/main.ts"></script></body></html>`,
);
fs.writeFileSync(path.join(src, "main.ts"), `import { v } from "./a";\nconsole.log(v);\n`);
const accept = "if (import.meta.hot) import.meta.hot.accept();\n";
fs.writeFileSync(path.join(src, "a.ts"), `export const v = "v1";\n${accept}`);

let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));

let failed = false;
let ws;
try {
  await waitUp(`http://localhost:${PORT}/src/a.ts`, { proc: srv });
  // In the module graph, as a page's import would put it.
  await (await fetch(`http://localhost:${PORT}/src/main.ts`)).text();
  await (await fetch(`http://localhost:${PORT}/src/a.ts`)).text();
  ws = await new Promise((resolve, reject) => {
    const s = new WebSocket(`ws://localhost:${PORT}/`, ["vite-hmr"]);
    s.addEventListener("open", () => resolve(s), { once: true });
    s.addEventListener("error", (e) => reject(new Error(String(e.message ?? e))), { once: true });
  });
  const messages = [];
  ws.addEventListener("message", (e) => messages.push(String(e.data)));
  const updated = (file) =>
    messages.some((m) => {
      const msg = JSON.parse(m);
      return msg.type === "update" && msg.updates.some((u) => u.path === file);
    });

  // The edit lands inside a burst far past the queue, and so does a new
  // directory: their events are dropped with the rest.
  const burst = path.join(src, "burst");
  fs.mkdirSync(burst);
  for (let i = 0; i < 3000; i++) fs.writeFileSync(path.join(burst, `f${i}.txt`), String(i));
  const born = path.join(src, "born");
  fs.mkdirSync(born);
  fs.writeFileSync(path.join(src, "a.ts"), `export const v = "v2";\n${accept}`);
  for (let i = 3000; i < 6000; i++) fs.writeFileSync(path.join(burst, `f${i}.txt`), String(i));

  await settles(async () => updated("/src/a.ts"), { timeoutMs: 20000 });
  if (!updated("/src/a.ts")) {
    throw new Error(`an edit made during the overflow never hot-updated; log:\n${log.slice(-3000)}`);
  }
  // Else the edit's own event got through and nothing above was tested.
  if (!log.includes("rescanning")) {
    throw new Error(
      `the burst never overflowed the queue (raise the burst or lower the queue); log:\n${log.slice(-2000)}`,
    );
  }
  console.log("watch rescan: an edit made during a queue overflow hot-updated");

  // The directory born mid-burst lost its Create, so its watch came from
  // the rescan's re-arm: an edit there is seen.
  fs.writeFileSync(path.join(born, "late.ts"), `export const late = 1;\n${accept}`);
  await settles(async () => (await fetch(`http://localhost:${PORT}/src/born/late.ts`)).ok, { timeoutMs: 10000 });
  await (await fetch(`http://localhost:${PORT}/src/born/late.ts`)).text();
  fs.writeFileSync(path.join(born, "late.ts"), `export const late = 2;\n${accept}`);
  await settles(async () => updated("/src/born/late.ts"), { timeoutMs: 20000 });
  if (!updated("/src/born/late.ts")) {
    throw new Error(`a directory created during the overflow is not watched; log:\n${log.slice(-3000)}`);
  }
  console.log("watch rescan: a directory created during the overflow is watched");
  console.log("WATCH-RESCAN E2E PASSED");
} catch (err) {
  failed = true;
  console.error(err);
} finally {
  ws?.close();
  srv.kill("SIGTERM");
  await new Promise((r) => (srv.exitCode !== null ? r() : srv.once("exit", r)));
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
