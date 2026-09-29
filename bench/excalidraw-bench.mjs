// SPDX-License-Identifier: MIT
// Ad-hoc benchmark: oj vs vite on the real Excalidraw app (excalidraw-oj fork).
// Measures cold/warm time-to-canvas via Playwright and settled process-tree RSS.
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { chromium } from "playwright";

const APP_ROOT = "/Users/rapha/Documents/a/excalidraw-oj";
const APP = "excalidraw-app";
const OJ_BIN = "/Users/rapha/Documents/a/oj/target/release/oj";
const ITERS = parseInt(process.env.ITERS ?? "3", 10);

const pct = (xs, p) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.ceil((p / 100) * s.length) - 1)];
};
const fmt = (xs) => `${pct(xs, 50)}/${pct(xs, 95)}ms`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const TOOLS = {
  oj: {
    port: 5401,
    spawn: () => spawn(OJ_BIN, ["dev", APP, "--port", "5401"], { cwd: APP_ROOT, stdio: "ignore" }),
    clearCache: () => fs.rmSync(path.join(APP_ROOT, APP, ".oj-cache"), { recursive: true, force: true }),
  },
  vite: {
    port: 5402,
    spawn: () =>
      spawn(process.execPath, [path.join(APP_ROOT, "node_modules/vite/bin/vite.js"), "--port", "5402"], {
        cwd: path.join(APP_ROOT, APP),
        stdio: "ignore",
      }),
    clearCache: () => {
      for (const d of ["node_modules/.vite", `${APP}/node_modules/.vite`])
        fs.rmSync(path.join(APP_ROOT, d), { recursive: true, force: true });
    },
  },
};

// Sum RSS (MB) of a process and all its descendants.
function treeRssMb(root) {
  try {
    const all = execSync("ps -eo pid,ppid,rss").toString().trim().split("\n").slice(1);
    const kids = new Map();
    const rss = new Map();
    for (const line of all) {
      const [pid, ppid, r] = line.trim().split(/\s+/).map(Number);
      rss.set(pid, r);
      if (!kids.has(ppid)) kids.set(ppid, []);
      kids.get(ppid).push(pid);
    }
    let total = 0;
    const stack = [root];
    while (stack.length) {
      const p = stack.pop();
      total += rss.get(p) ?? 0;
      for (const c of kids.get(p) ?? []) stack.push(c);
    }
    return Math.round(total / 1024);
  } catch {
    return 0;
  }
}

async function waitForServer(port, timeoutMs = 120000) {
  const t = Date.now();
  while (Date.now() - t < timeoutMs) {
    try {
      const r = await fetch(`http://localhost:${port}/`, { signal: AbortSignal.timeout(2000) });
      if (r.ok) return;
    } catch {}
    await sleep(100);
  }
  throw new Error(`server on ${port} never came up`);
}

async function renderOnce(browser, port) {
  const page = await browser.newPage();
  const t = Date.now();
  await page.goto(`http://localhost:${port}/`, { waitUntil: "commit", timeout: 120000 });
  await page.waitForSelector("canvas", { timeout: 120000 });
  const ms = Date.now() - t;
  await page.close();
  return ms;
}

async function session(tool, cold, browser) {
  const t = TOOLS[tool];
  if (cold) t.clearCache();
  const proc = t.spawn();
  try {
    await waitForServer(t.port);
    const ms = await renderOnce(browser, t.port);
    await sleep(1500);
    const rss = treeRssMb(proc.pid);
    return { ms, rss };
  } finally {
    try {
      execSync(`pkill -P ${proc.pid}`);
    } catch {}
    try {
      proc.kill("SIGKILL");
    } catch {}
    try {
      execSync(`lsof -ti:${t.port} -sTCP:LISTEN | xargs -r kill -9`);
    } catch {}
    await sleep(500);
  }
}

async function bench(tool) {
  const browser = await chromium.launch();
  const cold = [],
    warm = [],
    rss = [];
  try {
    for (let i = 0; i < ITERS; i++) {
      process.stderr.write(`  ${tool} cold ${i + 1}/${ITERS}\n`);
      const c = await session(tool, true, browser);
      cold.push(c.ms);
      rss.push(c.rss);
      process.stderr.write(`  ${tool} warm ${i + 1}/${ITERS}\n`);
      const w = await session(tool, false, browser);
      warm.push(w.ms);
    }
  } finally {
    await browser.close();
  }
  return { tool, cold, warm, rss };
}

const results = [];
for (const tool of ["oj", "vite"]) {
  process.stderr.write(`benchmarking ${tool} on Excalidraw…\n`);
  results.push(await bench(tool));
}

console.log(`\nExcalidraw (real app), ${ITERS} restarts — p50/p95 time-to-canvas, macOS ${process.arch}`);
console.log("tool | cold start   | warm start   | process-tree RSS");
console.log("-----|--------------|--------------|------------------");
for (const r of results) {
  console.log(`${r.tool.padEnd(4)} | ${fmt(r.cold).padEnd(12)} | ${fmt(r.warm).padEnd(12)} | ${pct(r.rss, 50)}MB`);
}
