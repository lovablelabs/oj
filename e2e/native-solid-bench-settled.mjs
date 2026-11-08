// SPDX-License-Identifier: MIT
// T7 (docs/20261108_native-solid-plugins.md): settlement-fair Vite-vs-OJ on
// the intersolaris app. Sibling of native-solid-bench.mjs: same boot/crawl/
// RSS/edit-probe core, plus (a) a REAL connected headless-Chromium page that
// performs a full client load of / (HTML + module graph) before anything is
// measured, identically on both sides, and (b) a POST-EDIT settled RSS taken
// after tsx edit + restore + css edit + restore and a 10s settle — the primary
// keep-rule metric.
// Run: node e2e/native-solid-bench-settled.mjs [--runs=3]
//   [--vite-port=3821] [--oj-port=3822] [--cdp-base=3922]
//   [--app=/path/to/app] [--out-dir=/tmp/oj-bench-settled]
//   [--chromium=/usr/bin/chromium]
// Env: BENCH_OJ overrides the oj binary (default: ../target/release/oj).
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");

const argv = Object.fromEntries(
  process.argv.slice(2).map((a) => {
    const m = /^--([^=]+)=(.*)$/.exec(a);
    return m ? [m[1], m[2]] : [a.replace(/^--/, ""), "1"];
  }),
);
const RUNS = Number(argv.runs ?? 3);
const VITE_PORT = Number(argv["vite-port"] ?? 3821);
const OJ_PORT = Number(argv["oj-port"] ?? 3822);
const CDP_BASE = Number(argv["cdp-base"] ?? 3922);
const APP = argv.app ?? "/home/david/leo_customers/intersolaris";
const OUT_DIR = argv["out-dir"] ?? "/tmp/oj-bench-settled";
const OJ = process.env.BENCH_OJ ?? path.join(repo, "target", "release", "oj");
const CHROMIUM = argv.chromium ?? "/usr/bin/chromium";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function deriveRoutes() {
  const seo = fs.readFileSync(path.join(APP, "src", "lib", "seo.ts"), "utf8");
  const list = fs.readFileSync(path.join(APP, "src", "app", "content", "contentList.ts"), "utf8");
  const out = [];
  for (const src of [seo, list]) {
    for (const m of src.matchAll(/path:\s*"([^"]+)"/g)) {
      if (!out.includes(m[1])) out.push(m[1]);
    }
  }
  out.sort((a, b) => (a === "/" ? -1 : b === "/" ? 1 : a.localeCompare(b)));
  return out;
}

function killPort(port) {
  try {
    const out = execSync("ss -ltnp 2>/dev/null", { shell: "/bin/sh", encoding: "utf8" });
    const pids = new Set();
    for (const line of out.split("\n")) {
      if (!line.includes(`:${port} `) && !line.includes(`:${port}\t`)) continue;
      for (const m of line.matchAll(/pid=(\d+)/g)) pids.add(Number(m[1]));
    }
    for (const pid of pids) {
      try {
        process.kill(pid, "SIGKILL");
      } catch {}
    }
  } catch {}
}

const killAndWait = (proc) =>
  new Promise((resolve) => {
    if (!proc || proc.exitCode !== null) return resolve();
    const done = () => {
      clearTimeout(timer);
      try {
        proc.kill("SIGKILL");
      } catch {}
      resolve();
    };
    const timer = setTimeout(done, 8000);
    proc.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
    try {
      proc.kill("SIGTERM");
    } catch {
      done();
    }
  });

async function waitFor200(port, { timeoutMs = 240000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const ctl = new AbortController();
      const t = setTimeout(() => ctl.abort(), 10000);
      try {
        const res = await fetch(`http://localhost:${port}/`, { signal: ctl.signal });
        if (res.status === 200) {
          await res.arrayBuffer();
          return;
        }
      } finally {
        clearTimeout(t);
      }
    } catch {}
    if (Date.now() >= deadline) throw new Error(`port ${port} never answered 200`);
    await sleep(200);
  }
}

async function get(port, route, timeoutMs = 60000) {
  const ctl = new AbortController();
  const t = setTimeout(() => ctl.abort(), timeoutMs);
  try {
    const res = await fetch(`http://localhost:${port}${route}`, { signal: ctl.signal });
    const body = await res.text();
    return { status: res.status, body };
  } finally {
    clearTimeout(t);
  }
}

function rssTreeKb(rootPid) {
  const childrenOf = new Map();
  for (const name of fs.readdirSync("/proc")) {
    if (!/^\d+$/.test(name)) continue;
    const pid = Number(name);
    if (pid === rootPid) continue;
    let stat;
    try {
      stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
    } catch {
      continue;
    }
    const rp = stat.lastIndexOf(")");
    if (rp < 0) continue;
    const ppid = Number(stat.slice(rp + 2).split(" ")[1]);
    if (!Number.isFinite(ppid)) continue;
    if (!childrenOf.has(ppid)) childrenOf.set(ppid, []);
    childrenOf.get(ppid).push(pid);
  }
  let total = 0;
  let procs = 0;
  const stack = [rootPid];
  const seen = new Set([rootPid]);
  while (stack.length) {
    const pid = stack.pop();
    let status;
    try {
      status = fs.readFileSync(`/proc/${pid}/status`, "utf8");
    } catch {
      if (pid === rootPid) throw new Error(`root pid ${rootPid} is gone`);
      continue;
    }
    const m = /VmRSS:\s+(\d+)\s+kB/.exec(status);
    if (m) {
      total += Number(m[1]);
      procs++;
    }
    for (const c of childrenOf.get(pid) ?? []) {
      if (!seen.has(c)) {
        seen.add(c);
        stack.push(c);
      }
    }
  }
  return { kb: total, procs };
}

async function settledRss(pid, settleMs = 8000) {
  await sleep(settleMs);
  const samples = [];
  for (let i = 0; i < 3; i++) {
    samples.push(rssTreeKb(pid).kb);
    await sleep(3000);
  }
  const sorted = [...samples].sort((a, b) => a - b);
  return { kb: sorted[1], samples };
}

// --- Minimal CDP driver (no npm deps): real headless Chromium page. ---
class Cdp {
  constructor(url) {
    this.url = url;
    this.nextId = 1;
    this.pending = new Map();
    this.events = [];
    this.loadFired = false;
    this.inflight = new Map(); // requestId -> url
    this.bytes = 0;
    this.reqs = 0;
  }
  async connect() {
    this.ws = new WebSocket(this.url);
    await new Promise((res, rej) => {
      const t = setTimeout(() => rej(new Error("cdp connect timeout")), 15000);
      this.ws.addEventListener(
        "open",
        () => {
          clearTimeout(t);
          res();
        },
        { once: true },
      );
      this.ws.addEventListener(
        "error",
        () => {
          clearTimeout(t);
          rej(new Error("cdp connect error"));
        },
        { once: true },
      );
    });
    this.ws.addEventListener("message", (ev) => {
      let m;
      try {
        m = JSON.parse(String(ev.data));
      } catch {
        return;
      }
      if (m.id != null && this.pending.has(m.id)) {
        const { res, rej } = this.pending.get(m.id);
        this.pending.delete(m.id);
        if (m.error) rej(new Error(`cdp: ${m.error.message}`));
        else res(m.result);
      } else if (m.method) {
        this.events.push(m);
        if (m.method === "Page.loadEventFired") this.loadFired = true;
        if (m.method === "Network.requestWillBeSent" && m.params?.requestId) {
          this.inflight.set(m.params.requestId, m.params.request?.url ?? "");
        }
        if ((m.method === "Network.loadingFinished" || m.method === "Network.loadingFailed") && m.params?.requestId) {
          if (this.inflight.has(m.params.requestId)) {
            this.inflight.delete(m.params.requestId);
            this.reqs++;
            this.bytes += m.params.encodedDataLength ?? 0;
          }
        }
      }
    });
  }
  send(method, params = {}, sessionId) {
    const id = this.nextId++;
    const msg = sessionId ? { id, method, params, sessionId } : { id, method, params };
    return new Promise((res, rej) => {
      this.pending.set(id, { res, rej });
      this.ws.send(JSON.stringify(msg));
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          rej(new Error(`cdp timeout: ${method}`));
        }
      }, 30000);
    });
  }
  close() {
    try {
      this.ws.close();
    } catch {}
  }
  async evaluate(sessionId, expression) {
    const r = await this.send("Runtime.evaluate", { expression, returnByValue: true }, sessionId);
    if (r?.exceptionDetails) throw new Error(`evaluate threw: ${r.exceptionDetails.text ?? "js error"}`);
    return r?.result?.value;
  }
}

async function launchBrowser(cdpPort, profileDir) {
  killPort(cdpPort);
  fs.rmSync(profileDir, { recursive: true, force: true });
  fs.mkdirSync(profileDir, { recursive: true });
  const proc = spawn(
    CHROMIUM,
    [
      "--headless=new",
      "--no-sandbox",
      "--disable-gpu",
      "--disable-dev-shm-usage",
      "--no-first-run",
      "--no-default-browser-check",
      "--user-data-dir=" + profileDir,
      "--remote-debugging-port=" + cdpPort,
      "--remote-debugging-address=127.0.0.1",
      "--host-resolver-rules=MAP localhost 127.0.0.1",
      "about:blank",
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "";
  proc.stdout.on("data", (d) => (log += d));
  proc.stderr.on("data", (d) => (log += d));
  const deadline = Date.now() + 60000;
  let targets = null;
  for (;;) {
    try {
      const res = await fetch(`http://127.0.0.1:${cdpPort}/json/list`);
      if (res.status === 200) {
        targets = await res.json();
        break;
      }
    } catch {}
    if (Date.now() >= deadline) {
      try {
        proc.kill("SIGKILL");
      } catch {}
      throw new Error(`chromium debug endpoint never came up\n${log.slice(-2000)}`);
    }
    await sleep(250);
  }
  const page = targets.find((t) => t.type === "page") ?? targets[0];
  if (!page?.webSocketDebuggerUrl) {
    try {
      proc.kill("SIGKILL");
    } catch {}
    throw new Error(`no page target: ${JSON.stringify(targets).slice(0, 500)}`);
  }
  return { proc, pageWs: page.webSocketDebuggerUrl, log: () => log };
}

// Full client load of / in the connected page: navigate, wait for load event,
// then for network idle (no inflight dev-server requests for 2.5s).
async function pageLoad(port, cdpPort, profileDir) {
  const { proc, pageWs } = await launchBrowser(cdpPort, profileDir);
  const cdp = new Cdp(pageWs);
  try {
    await cdp.connect();
    await cdp.send("Page.enable");
    await cdp.send("Runtime.enable");
    await cdp.send("Network.enable");
    const t0 = Date.now();
    await cdp.send("Page.navigate", { url: `http://localhost:${port}/` });
    const loadDeadline = Date.now() + 180000;
    for (;;) {
      if (cdp.loadFired) break;
      if (Date.now() >= loadDeadline) throw new Error("Page.loadEventFired never fired");
      await sleep(250);
    }
    const loadMs = Date.now() - t0;
    // Network idle: 2.5s with nothing inflight (dev-server requests only).
    let calmSince = Date.now();
    let lastInflight = -1;
    for (;;) {
      const n = [...cdp.inflight.values()].filter((u) => u.includes(`localhost:${port}`)).length;
      if (n === 0) {
        if (lastInflight !== 0) calmSince = Date.now();
        if (Date.now() - calmSince >= 2500) break;
      } else {
        calmSince = Date.now();
      }
      lastInflight = n;
      if (Date.now() >= loadDeadline) throw new Error("network never went idle");
      await sleep(250);
    }
    const idleMs = Date.now() - t0;
    return { browser: proc, cdp, loadMs, idleMs, reqs: cdp.reqs, bytes: cdp.bytes };
  } catch (e) {
    cdp.close();
    await killAndWait(proc);
    killPort(cdpPort);
    throw e;
  }
}

async function pollBrowser(cdp, sessionId, expr, { timeoutMs = 90000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      if (await cdp.evaluate(sessionId, expr)) return true;
    } catch {
      // Page mid-reload (context destroyed): keep polling.
    }
    if (Date.now() >= deadline) return false;
    await sleep(250);
  }
}

function writeViteTimingConfig(outDir) {
  const cfgPath = path.join(outDir, "vite.timing.config.mts");
  const appConfig = path.join(APP, "vite.config.ts");
  fs.writeFileSync(
    cfgPath,
    `import base from ${JSON.stringify(appConfig)};
import fs from "node:fs";
const out = process.env.VITE_PLUGIN_TIMING_FILE ?? "";
const stats = new Map();
let lastFlush = 0;
function record(name, ms) {
  let r = stats.get(name);
  if (!r) { r = { calls: 0, ms: 0 }; stats.set(name, r); }
  r.calls += 1; r.ms += ms;
  const now = Date.now();
  if (out && now - lastFlush > 2000) { lastFlush = now; flush(); }
}
function flush() {
  if (!out) return;
  try {
    fs.writeFileSync(out, JSON.stringify({ pid: process.pid, at: new Date().toISOString(),
      plugins: [...stats].map(([name, r]) => ({ name, calls: r.calls, ms: Math.round(r.ms * 100) / 100 })) }));
  } catch {}
}
const timer = setInterval(flush, 2000);
if (typeof timer.unref === "function") timer.unref();
process.on("exit", flush);
function wrapHook(name, hook) {
  if (typeof hook === "function") {
    const fn = hook;
    return async function (...args) {
      const t0 = performance.now();
      try { return await fn.apply(this, args); } finally { record(name, performance.now() - t0); }
    };
  }
  if (hook && typeof hook === "object" && typeof hook.handler === "function") {
    const orig = hook.handler;
    return { ...hook, handler: async function (...args) {
      const t0 = performance.now();
      try { return await orig.apply(this, args); } finally { record(name, performance.now() - t0); }
    } };
  }
  return hook;
}
function wrapPlugin(p) {
  if (!p) return p;
  if (Array.isArray(p)) return p.map(wrapPlugin);
  if (typeof p !== "object" || p.transform == null || p.__ojBenchWrapped) return p;
  const name = typeof p.name === "string" ? p.name : "unknown";
  return { ...p, __ojBenchWrapped: true, transform: wrapHook(name, p.transform) };
}
const resolved = typeof base === "function"
  ? await base({ command: "serve", mode: "development" })
  : base;
export default { ...resolved, plugins: [resolved.plugins].flat(Infinity).map(wrapPlugin) };
`,
  );
  return cfgPath;
}

function boundPortFromLog(log, fallback) {
  const m = /localhost:(\d+)/.exec(log) ?? /127\.0\.0\.1:(\d+)/.exec(log) ?? /0\.0\.0\.0:(\d+)/.exec(log);
  return m ? Number(m[1]) : fallback;
}

// One measured side-run: boot -> connected full client load of / -> crawl ->
// post-crawl RSS -> tsx edit (browser-visible + served-HTML) -> css edit
// (browser-visible + served-CSS) -> restores -> 10s settle -> post-edit RSS.
async function runSide({ side, port, cdpPort, routes, timingFile, viteTimingConfig, profileBase }) {
  killPort(port);
  killPort(cdpPort);
  await sleep(500);
  fs.rmSync(timingFile, { force: true });
  let proc;
  let log = "";
  if (side === "vite") {
    proc = spawn(
      "bun",
      ["run", "vite:dev", "--", "--port", String(port), "--strictPort", "--config", viteTimingConfig],
      {
        cwd: APP,
        stdio: ["ignore", "pipe", "pipe"],
        env: { ...process.env, VITE_PLUGIN_TIMING_FILE: timingFile },
      },
    );
  } else {
    proc = spawn(OJ, ["dev", APP, "--port", String(port), "--host=127.0.0.1"], {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, OJ_PLUGIN_TIMING: "1", OJ_PLUGIN_TIMING_FILE: timingFile },
    });
  }
  proc.stdout.on("data", (d) => (log += d));
  proc.stderr.on("data", (d) => (log += d));
  let browser = null;
  let cdp = null;
  const fail = async (msg) => {
    try {
      cdp?.close();
    } catch {}
    if (browser) {
      await killAndWait(browser);
      killPort(cdpPort);
    }
    await killAndWait(proc);
    killPort(port);
    throw new Error(`${side}: ${msg}\n--- server log tail ---\n${log.slice(-4000)}`);
  };
  try {
    const t0 = Date.now();
    try {
      await waitFor200(port);
    } catch (e) {
      const bound = boundPortFromLog(log, port);
      if (bound !== port) {
        await waitFor200(bound);
        port = bound;
      } else {
        await fail(`cold boot failed (${e.message})`);
      }
    }
    const bootMs = Date.now() - t0;
    if (proc.exitCode !== null) await fail(`exited during boot (exit=${proc.exitCode})`);

    // Full client load of / in a real connected page, before measuring.
    const loaded = await pageLoad(port, cdpPort, `${profileBase}-${side}`).catch((e) =>
      fail(`page load: ${e.message}`),
    );
    browser = loaded.browser;
    cdp = loaded.cdp;

    const crawl = [];
    for (const route of routes) {
      try {
        const { status, body } = await get(port, route);
        crawl.push({ route, status, bytes: body.length });
      } catch (e) {
        crawl.push({ route, status: `ERR ${e.message}`, bytes: 0 });
      }
    }
    const bad = crawl.filter((r) => r.status !== 200);

    const rssPostCrawl = await settledRss(proc.pid);
    if (proc.exitCode !== null) await fail(`exited after crawl (exit=${proc.exitCode})`);

    // tsx edit: token into cookie-settings-button; browser-visible = page
    // innerText (HMR update or reload), served = GET / body. Timed together
    // from the write; whichever lands first is the honest signal.
    const tsxFile = path.join(APP, "src", "components", "cookie-settings-button.tsx");
    const tsxFind = "Cookie-Einstellungen";
    const tsxToken = `CookieBench${Date.now().toString(36).toUpperCase()}XZ`;
    const tsxOrig = fs.readFileSync(tsxFile, "utf8");
    if (!tsxOrig.includes(tsxFind)) await fail("probe marker missing");
    const servedSees = async (marker) => {
      try {
        const { status, body } = await get(port, "/");
        return status === 200 && body.includes(marker);
      } catch {
        return false;
      }
    };
    if (!(await servedSees(tsxFind))) await fail("baseline marker not served at / before edit");
    const tsxExpr = (m) => `document.body ? document.body.innerText.includes(${JSON.stringify(m)}) : false`;
    let tsxBrowserMs = -1;
    let tsxServedMs = -1;
    {
      const tE = Date.now();
      fs.writeFileSync(tsxFile, tsxOrig.replace(tsxFind, tsxToken));
      const deadline = Date.now() + 120000;
      for (;;) {
        let b = false;
        try {
          b = await cdp.evaluate(undefined, tsxExpr(tsxToken));
        } catch {}
        if (b && tsxBrowserMs < 0) tsxBrowserMs = Date.now() - tE;
        let s = false;
        try {
          s = await servedSees(tsxToken);
        } catch {}
        if (s && tsxServedMs < 0) tsxServedMs = Date.now() - tE;
        if (tsxBrowserMs >= 0 && tsxServedMs >= 0) break;
        if (Date.now() >= deadline) {
          if (tsxBrowserMs < 0 && tsxServedMs < 0) await fail("tsx edit visible nowhere within 120s");
          break; // one signal landed; take it, note the other as missing
        }
        await sleep(250);
      }
      fs.writeFileSync(tsxFile, tsxOrig);
      const rdeadline = Date.now() + 120000;
      for (;;) {
        let ok = false;
        try {
          ok = await servedSees(tsxFind);
        } catch {}
        if (ok) break;
        if (Date.now() >= rdeadline) await fail("tsx restore not served");
        await sleep(250);
      }
    }

    // css edit: real rule appended to globals.css; browser-visible = computed
    // style of an injected probe div (re-injected each poll in case the page
    // reloaded), served = GET of the discovered served CSS URL.
    const cssFile = path.join(APP, "src", "globals.css");
    const cssOrig = fs.readFileSync(cssFile, "utf8");
    const { body: home } = await get(port, "/");
    const hrefs = [...home.matchAll(/<link[^>]+href="([^"]+\.css[^"]*)"/g)].map((m) => m[1]);
    const same = hrefs.filter((h) => h.startsWith("/") || h.includes(`localhost:${port}`));
    const cssUrl = same.length
      ? new URL(same.find((h) => h.includes("globals")) ?? same[0], `http://localhost:${port}`).pathname +
        new URL(same.find((h) => h.includes("globals")) ?? same[0], `http://localhost:${port}`).search
      : "/src/globals.css";
    const cssToken = `benchcss${Date.now().toString(36)}x`.toLowerCase();
    const cssClass = `benchcss-${cssToken}`;
    const cssRule = `\n.${cssClass}{display:none}\n`;
    const cssServedSees = async (marker) => {
      try {
        const { status, body } = await get(port, cssUrl);
        return status === 200 && body.includes(marker);
      } catch {
        return false;
      }
    };
    const cssExpr =
      `(function(){var d=document.getElementById('ojbenchcss');` +
      `if(!d&&document.body){document.body.insertAdjacentHTML('beforeend','<div id="ojbenchcss" class=${JSON.stringify(cssClass)}></div>');d=document.getElementById('ojbenchcss');}` +
      `return !!d&&getComputedStyle(d).display==='none';})()`;
    let cssBrowserMs = -1;
    let cssServedMs = -1;
    {
      const tC = Date.now();
      fs.writeFileSync(cssFile, cssOrig + cssRule);
      const deadline = Date.now() + 90000;
      for (;;) {
        let b = false;
        try {
          b = await cdp.evaluate(undefined, cssExpr);
        } catch {}
        if (b && cssBrowserMs < 0) cssBrowserMs = Date.now() - tC;
        let s = false;
        try {
          s = await cssServedSees(cssToken);
        } catch {}
        if (s && cssServedMs < 0) cssServedMs = Date.now() - tC;
        if (cssBrowserMs >= 0 && cssServedMs >= 0) break;
        if (Date.now() >= deadline) {
          if (cssBrowserMs < 0 && cssServedMs < 0) await fail("css edit visible nowhere within 90s");
          break;
        }
        await sleep(250);
      }
      fs.writeFileSync(cssFile, cssOrig);
      const rdeadline = Date.now() + 90000;
      for (;;) {
        const r = await get(port, cssUrl).catch(() => ({}));
        if (r.status === 200 && r.body != null && !r.body.includes(cssToken)) break;
        if (Date.now() >= rdeadline) await fail("css restore not served");
        await sleep(250);
      }
    }

    // Primary keep-rule metric: RSS after BOTH edits + restores and a 10s settle.
    const rssPostEdit = await settledRss(proc.pid, 10000);
    if (proc.exitCode !== null) await fail(`exited before post-edit rss (exit=${proc.exitCode})`);
    return {
      side,
      port,
      bootMs,
      pageLoadMs: loaded.loadMs,
      pageIdleMs: loaded.idleMs,
      pageReqs: loaded.reqs,
      pageBytes: loaded.bytes,
      routes: crawl.length,
      non200: bad,
      rssPostCrawlKb: rssPostCrawl.kb,
      rssPostCrawlSamplesKb: rssPostCrawl.samples,
      tsxBrowserMs,
      tsxServedMs,
      cssBrowserMs,
      cssServedMs,
      cssUrl,
      rssPostEditKb: rssPostEdit.kb,
      rssPostEditSamplesKb: rssPostEdit.samples,
    };
  } finally {
    try {
      cdp?.close();
    } catch {}
    if (browser) {
      await killAndWait(browser);
      killPort(cdpPort);
    }
    await killAndWait(proc);
    killPort(port);
  }
}

const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

const routes = deriveRoutes();
console.log(`routes: ${routes.length}`);
if (!fs.existsSync(OJ)) throw new Error(`oj binary missing at ${OJ}`);
if (!fs.existsSync(CHROMIUM)) throw new Error(`chromium missing at ${CHROMIUM}`);
fs.mkdirSync(OUT_DIR, { recursive: true });
const viteTimingConfig = writeViteTimingConfig(OUT_DIR);

// Fix warm cache state: one unmeasured boot + full page load + crawl per side
// (Vite dep optimizer + OJ bundle caches warm), then the measured runs reuse
// them untouched. Caches are never deleted between runs.
console.log("warm-up (unmeasured)...");
for (const side of ["vite", "oj"]) {
  const port = side === "vite" ? VITE_PORT : OJ_PORT;
  const warm = await runSide({
    side,
    port,
    cdpPort: CDP_BASE + (side === "vite" ? 10 : 11),
    routes,
    timingFile: path.join(OUT_DIR, "warm-timing.json"),
    viteTimingConfig,
    profileBase: path.join(OUT_DIR, "warm-profile"),
  });
  console.log(`  warm ${side}: boot=${warm.bootMs}ms pageIdle=${warm.pageIdleMs}ms reqs=${warm.pageReqs}`);
}

const results = { app: APP, oj: OJ, chromium: CHROMIUM, at: new Date().toISOString(), runs: [] };
results.method = {
  viteCmd: "bun run vite:dev -- --port <port> --strictPort --config <harness timing wrapper> (cwd=app)",
  ojCmd: "oj dev <app> --port <port> --host=127.0.0.1",
  page: "real headless Chromium (system binary) over CDP: navigate to /, wait Page.loadEventFired, then network idle (2.5s calm, dev-server requests) — full client load of HTML + module graph, identical both sides, fresh browser profile per run",
  boot: "spawn -> first HTTP 200 on /, polled every 200ms",
  routes: "seo.ts path: + contentList.ts path: regex, sequential GET, status recorded",
  rssPostCrawl: "settled 8s after crawl, median of 3 VmRSS process-tree samples 3s apart",
  tsxEdit:
    "cookie-settings-button.tsx marker -> unique token; browser-visible = page innerText via CDP Runtime.evaluate (HMR or reload); served = GET / body; timed together from the write",
  cssEdit:
    "real rule appended to src/globals.css; browser-visible = getComputedStyle(display==='none') of injected probe div via CDP; served = GET discovered CSS URL",
  rssPostEdit:
    "PRIMARY METRIC: after tsx restore + css restore both verified served, 10s settle, then median of 3 VmRSS samples 3s apart",
  caches: "warm-up boot+load+crawl per side before measured runs; caches never deleted between runs",
};

for (let run = 1; run <= RUNS; run++) {
  console.log(`\n=== run ${run}/${RUNS} ===`);
  const runOut = { run, sides: {} };
  for (const side of ["vite", "oj"]) {
    const port = side === "vite" ? VITE_PORT : OJ_PORT;
    const cdpPort = CDP_BASE + run * 2 + (side === "vite" ? 0 : 1);
    const timingFile = path.join(OUT_DIR, `${side}-timing-run${run}.json`);
    console.log(`-- ${side} on :${port} ...`);
    const t0 = Date.now();
    runOut.sides[side] = await runSide({
      side,
      port,
      cdpPort,
      routes,
      timingFile,
      viteTimingConfig,
      profileBase: path.join(OUT_DIR, `profile-run${run}`),
    });
    const s = runOut.sides[side];
    console.log(
      `   boot=${s.bootMs}ms load=${s.pageLoadMs}/${s.pageIdleMs}ms reqs=${s.pageReqs} ` +
        `rssCrawl=${Math.round(s.rssPostCrawlKb / 1024)}MB tsxB=${s.tsxBrowserMs}ms tsxS=${s.tsxServedMs}ms ` +
        `cssB=${s.cssBrowserMs}ms cssS=${s.cssServedMs}ms rssEdit=${Math.round(s.rssPostEditKb / 1024)}MB ` +
        `(${((Date.now() - t0) / 1000) | 0}s wall)`,
    );
    if (s.non200.length) console.log(`   non-200: ${JSON.stringify(s.non200.slice(0, 10))}`);
  }
  results.runs.push(runOut);
}

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
fs.writeFileSync(path.join(OUT_DIR, `settled-results-${stamp}.json`), JSON.stringify(results, null, 2));

let md = `# T7 settlement-fair bench\n\n${results.at} — app ${APP}, oj ${OJ}, chromium ${CHROMIUM}\n\n## Method\n\n`;
for (const [k, v] of Object.entries(results.method)) md += `- **${k}**: ${v}\n`;
md += `\n## Runs\n\n| side | run | boot ms | page load/idle ms | page reqs | routes 200/total | RSS post-crawl MB | tsx browser ms | tsx served ms | css browser ms | css served ms | RSS post-edit MB |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n`;
for (const r of results.runs) {
  for (const side of ["vite", "oj"]) {
    const s = r.sides[side];
    md += `| ${side} | ${r.run} | ${s.bootMs} | ${s.pageLoadMs}/${s.pageIdleMs} | ${s.pageReqs} | ${s.routes - s.non200.length}/${s.routes} | ${(s.rssPostCrawlKb / 1024).toFixed(0)} | ${s.tsxBrowserMs} | ${s.tsxServedMs} | ${s.cssBrowserMs} | ${s.cssServedMs} | ${(s.rssPostEditKb / 1024).toFixed(0)} |\n`;
  }
}
const med = (f) => ({
  vite: median(results.runs.map((r) => f(r.sides.vite))),
  oj: median(results.runs.map((r) => f(r.sides.oj))),
});
const mBoot = med((s) => s.bootMs);
const mCrawl = med((s) => s.rssPostCrawlKb);
const mEdit = med((s) => s.rssPostEditKb);
const mTsxB = med((s) => s.tsxBrowserMs);
const mTsxS = med((s) => s.tsxServedMs);
const mCssB = med((s) => s.cssBrowserMs);
const mCssS = med((s) => s.cssServedMs);
md += `\n## Medians (3 runs)\n\n| metric | vite | oj |\n|---|---|---|\n| boot ms | ${mBoot.vite} | ${mBoot.oj} |\n| RSS post-crawl MB | ${(mCrawl.vite / 1024).toFixed(0)} | ${(mCrawl.oj / 1024).toFixed(0)} |\n| RSS post-edit MB (PRIMARY) | ${(mEdit.vite / 1024).toFixed(0)} | ${(mEdit.oj / 1024).toFixed(0)} |\n| tsx browser-visible ms | ${mTsxB.vite} | ${mTsxB.oj} |\n| tsx served-HTML ms | ${mTsxS.vite} | ${mTsxS.oj} |\n| css browser-visible ms | ${mCssB.vite} | ${mCssB.oj} |\n| css served ms | ${mCssS.vite} | ${mCssS.oj} |\n`;
const rssSaving = ((mEdit.vite - mEdit.oj) / mEdit.vite) * 100;
md += `\n## Keep rule\n\n- RSS: OJ median post-edit ${(mEdit.oj / 1024).toFixed(0)}MB vs Vite ${(mEdit.vite / 1024).toFixed(0)}MB = ${rssSaving.toFixed(1)}% ${mEdit.oj < mEdit.vite ? "below" : "ABOVE"} Vite (needs >=20% below): ${rssSaving >= 20 ? "PASS" : "FAIL"}\n`;
fs.writeFileSync(path.join(OUT_DIR, `settled-results-${stamp}.md`), md);
console.log(`\n${md.split("## Keep rule")[1]}`);
console.log(`wrote ${OUT_DIR}/settled-results-${stamp}.{json,md}`);
