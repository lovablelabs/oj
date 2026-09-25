import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const N = parseInt(process.argv[2] ?? "1000", 10);
const ITERS = parseInt(process.env.BENCH_ITERS ?? "5", 10); // cold/warm restarts
const HMR_EDITS = parseInt(process.env.BENCH_HMR_EDITS ?? "10", 10);
const pct = (xs, p) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.ceil((p / 100) * s.length) - 1)];
};
const fmt = (xs) => `${pct(xs, 50)}/${pct(xs, 95)}ms`;
const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "apps", `app-${N}`);
const OJ_BIN = path.join(repo, "target", "release", "oj");

const TOOLS = {
  oj: {
    port: 5199,
    spawn: () => spawn(OJ_BIN, ["dev", app, "--port", "5199"], { stdio: "ignore" }),
    clearCache: () => fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true }),
  },
  vite: {
    port: 5200,
    spawn: () =>
      spawn(process.execPath, [path.join(app, "node_modules", "vite", "bin", "vite.js")], {
        cwd: app,
        stdio: "ignore",
      }),
    clearCache: () =>
      fs.rmSync(path.join(app, "node_modules", ".vite"), { recursive: true, force: true }),
  },
  "vite-fbm": {
    // Vite's experimental bundled dev mode (full bundle mode).
    port: 5200,
    spawn: () =>
      spawn(
        process.execPath,
        [
          path.join(app, "node_modules", "vite", "bin", "vite.js"),
          "--config",
          "vite.bundled.config.mjs",
        ],
        { cwd: app, stdio: "ignore" }
      ),
    clearCache: () =>
      fs.rmSync(path.join(app, "node_modules", ".vite"), { recursive: true, force: true }),
  },
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitForServer(port, timeoutMs = 60000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const res = await fetch(`http://localhost:${port}/`);
      if (res.ok) return;
    } catch {}
    await sleep(30);
  }
  throw new Error(`server on :${port} did not come up`);
}

async function renderOnce(browser, port) {
  const page = await browser.newPage();
  const t0 = Date.now();
  await page.goto(`http://localhost:${port}/`, { timeout: 120000 });
  await page.waitForSelector("[data-done]", { timeout: 120000 });
  const ms = Date.now() - t0;
  return { page, ms };
}

function rssMb(pid) {
  try {
    return Math.round(parseInt(execSync(`ps -o rss= -p ${pid}`).toString().trim(), 10) / 1024);
  } catch {
    return NaN;
  }
}

async function measureHmr(page, marker) {
  const leaf = path.join(app, "src", "components", `Comp${N - 1}.tsx`);
  const original = fs.readFileSync(leaf, "utf8");
  const t0 = Date.now();
  fs.writeFileSync(leaf, original.replace(/leaf-\d+-marker-\w+/, marker));
  await page.waitForFunction(
    (m) => document.body.innerText.includes(m),
    marker,
    { timeout: 30000, polling: 16 }
  );
  return Date.now() - t0;
}

async function session(tool, cold) {
  const { port, spawn: spawnTool, clearCache } = TOOLS[tool];
  if (cold) clearCache();
  const t0 = Date.now();
  const proc = spawnTool();
  await waitForServer(port);
  const ready = Date.now() - t0;
  return { proc, ready, port };
}

async function bench(tool) {
  const browser = await chromium.launch();
  const result = { tool, cold: [], warm: [], reload: [], hmr: [] };

  for (let i = 0; i < ITERS; i++) {
    // cold restart
    let { proc, ready, port } = await session(tool, true);
    let { page, ms } = await renderOnce(browser, port);
    result.cold.push(ready + ms);
    if (i === 0) {
      // hmr percentiles: repeated edits within one session
      for (let e = 0; e < HMR_EDITS; e++) {
        result.hmr.push(await measureHmr(page, `leaf-${N - 1}-marker-H${i}x${e}x${Date.now()}`));
        await sleep(150); // clear the watcher debounce window between edits
      }
      result.rssMb = rssMb(proc.pid);
    }
    await page.close();
    proc.kill("SIGKILL");
    await sleep(700);

    // warm restart (caches primed by the cold run)
    ({ proc, ready, port } = await session(tool, false));
    ({ page, ms } = await renderOnce(browser, port));
    result.warm.push(ready + ms);
    const t = Date.now();
    await page.reload();
    await page.waitForSelector("[data-done]", { timeout: 120000 });
    result.reload.push(Date.now() - t);
    await page.close();
    proc.kill("SIGKILL");
    await sleep(700);
    restoreLeaf();
  }

  await browser.close();
  return result;
}

function restoreLeaf() {
  const leaf = path.join(app, "src", "components", `Comp${N - 1}.tsx`);
  fs.writeFileSync(
    leaf,
    fs.readFileSync(leaf, "utf8").replace(/leaf-\d+-marker-\w+/, `leaf-${N - 1}-marker-A`)
  );
}

const rows = [];
for (const tool of Object.keys(TOOLS)) {
  console.error(`benchmarking ${tool} on ${N} components…`);
  rows.push(await bench(tool));
  restoreLeaf();
  await sleep(300);
}

// again, table??
console.log(`\n${N} components (fanout-10 tree), ${ITERS} restarts, ${HMR_EDITS} hmr edits — p50/p95, macOS ${process.arch}, ${new Date().toISOString().slice(0, 10)}`);
console.log("tool      | cold start   | warm start   | reload       | HMR         | RSS");
console.log("----------|--------------|--------------|--------------|-------------|-----");
for (const r of rows) {
  console.log(
    `${r.tool.padEnd(9)} | ${fmt(r.cold).padEnd(12)} | ${fmt(r.warm).padEnd(12)} | ${fmt(r.reload).padEnd(12)} | ${fmt(r.hmr).padEnd(11)} | ${r.rssMb}MB`
  );
}
