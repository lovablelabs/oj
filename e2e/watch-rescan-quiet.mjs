// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A rescan must not raise the next one. With notify 8, the inotify watch
// subscribed to opens and `watch` walks on the thread that drains the queue,
// so re-arming a large tree after an overflow queued an open event per
// directory with nothing reading them: the queue overflowed again, the next
// rescan re-armed again, and the server rescanned forever with no file
// changing. Now the watch subscribes to changes only and only new
// directories are re-armed; at a queue of one, any open the rescan's own
// walk queued would overflow it again. Many directories, one overflow, then
// quiet: the rescans stop. Linux only, with a small queue:
//   sudo sysctl -w fs.inotify.max_queued_events=1
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 5518;

const queue = (() => {
  try {
    return Number(fs.readFileSync("/proc/sys/fs/inotify/max_queued_events", "utf8"));
  } catch {
    return Infinity;
  }
})();
if (process.platform !== "linux" || queue > 256) {
  console.log("SKIP watch rescan quiet: needs Linux with a small inotify queue");
  console.log("  enable with: sudo sysctl -w fs.inotify.max_queued_events=1");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-watch-rescan-quiet-"));
const src = path.join(app, "src");
// Several open events per directory on a re-arm: far past the queue.
for (let i = 0; i < 3000; i++) {
  fs.mkdirSync(path.join(src, `d${i}`), { recursive: true });
  fs.writeFileSync(path.join(src, `d${i}`, "x.ts"), "export {};\n");
}
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "watch-rescan-quiet", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><body><script type="module" src="/src/main.ts"></script></body></html>`,
);
const accept = "if (import.meta.hot) import.meta.hot.accept();\n";
fs.writeFileSync(path.join(src, "main.ts"), `export const v = "v1";\n${accept}`);

let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));
const rescans = () => (log.match(/rescanning/g) ?? []).length;

let failed = false;
let ws;
try {
  await waitUp(`http://localhost:${PORT}/src/main.ts`, { proc: srv });
  await (await fetch(`http://localhost:${PORT}/src/main.ts`)).text();
  ws = await new Promise((resolve, reject) => {
    const s = new WebSocket(`ws://localhost:${PORT}/`, ["vite-hmr"]);
    s.addEventListener("open", () => resolve(s), { once: true });
    s.addEventListener("error", (e) => reject(new Error(String(e.message ?? e))), { once: true });
  });
  const messages = [];
  ws.addEventListener("message", (e) => messages.push(String(e.data)));

  for (let i = 0; i < 20000; i++) fs.writeFileSync(path.join(src, "d0", `b${i}.txt`), "x");
  await settles(async () => rescans() > 0, { timeoutMs: 20000 });
  if (rescans() === 0) {
    throw new Error(`the burst never overflowed the queue (lower the queue); log:\n${log.slice(-2000)}`);
  }
  // Settled: nothing writes from here on, so nothing may rescan.
  await sleep(3000);
  const settled = rescans();
  await sleep(5000);
  if (rescans() !== settled) {
    throw new Error(
      `rescans kept going with no file changing (${settled} then ${rescans()}); log tail:\n${log.slice(-1500)}`,
    );
  }
  console.log(`watch rescan quiet: ${settled} rescan(s), then none while nothing changed`);

  // And the watcher still works.
  fs.writeFileSync(path.join(src, "main.ts"), `export const v = "v2";\n${accept}`);
  const updated = () =>
    messages.some((m) => {
      const msg = JSON.parse(m);
      return msg.type === "update" && msg.updates.some((u) => u.path === "/src/main.ts");
    });
  await settles(async () => updated(), { timeoutMs: 20000 });
  if (!updated()) throw new Error(`an edit after the rescans never hot-updated; log:\n${log.slice(-2000)}`);
  console.log("watch rescan quiet: an edit afterwards hot-updated");
  console.log("WATCH-RESCAN-QUIET E2E PASSED");
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
