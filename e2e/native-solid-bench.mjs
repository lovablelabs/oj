// SPDX-License-Identifier: MIT
// T1 bench (docs/20261108_native-solid-plugins.md): Vite vs OJ hosted on the
// same app, one at a time, different free ports:
// cold boot to first HTTP 200 on /, crawl of all routes, settled RSS of the
// whole process tree (same /proc method both sides), single .tsx edit latency,
// CSS-only edit latency, per-plugin transform wall-time + call counts.
// The reference app is used read-only except the two edit probes, which
// restore the original file contents (verified via git status at the end).
//
// Run: `node e2e/native-solid-bench.mjs [--runs=2] [--vite-port=3816]
//   [--oj-port=3817] [--app=/path/to/app] [--out-dir=/tmp/oj-bench]`
// Env: BENCH_OJ overrides the oj binary (default: ../target/release/oj).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");

const argv = Object.fromEntries(
  process.argv.slice(2).map((a) => {
    const m = /^--([^=]+)=(.*)$/.exec(a);
    return m ? [m[1], m[2]] : [a.replace(/^--/, ""), "1"];
  }),
);
const RUNS = Number(argv.runs ?? 2);
const VITE_PORT = Number(argv["vite-port"] ?? 3816);
const OJ_PORT = Number(argv["oj-port"] ?? 3817);
const APP = argv.app ?? "/home/david/leo_customers/intersolaris";
const OUT_DIR = argv["out-dir"] ?? "/tmp/oj-bench";
const OJ = process.env.BENCH_OJ ?? path.join(repo, "target", "release", "oj");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Routes: the app's own prerender set (vite.config builds it from seoPages +
// allContent), derived read-only via regex so no app code executes.
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
  // No lsof on this box; parse pids out of `ss -ltnp` instead.
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
      proc.kill("SIGKILL");
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
  let last = "no response";
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
        last = `status ${res.status}`;
      } finally {
        clearTimeout(t);
      }
    } catch (e) {
      last = e?.cause?.code ?? e?.message ?? String(e);
    }
    if (Date.now() >= deadline) throw new Error(`port ${port} never answered 200 (${last})`);
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

// RSS of the whole process tree rooted at pid, summed over /proc/*/status
// VmRSS. Same procedure for Vite and OJ. Short-lived children that exit
// mid-scan are skipped; the root going away throws.
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

async function settledRss(pid) {
  await sleep(8000);
  const samples = [];
  for (let i = 0; i < 3; i++) {
    samples.push(rssTreeKb(pid).kb);
    await sleep(3000);
  }
  samples.sort((a, b) => a - b);
  return { kb: samples[1], samples };
}

// Edit probe: replace `find` with a unique token in `file`, poll `urlPath`
// until the served body contains the token (HTTP 200), then restore. Restores
// in a finally so the app is left clean even on failure. Returns latency ms.
// `touch` re-fires the probe write (see e2e/util.mjs settles rationale).
async function editProbe(port, { file, find, token, urlPath, timeoutMs = 90000 }) {
  const original = fs.readFileSync(file, "utf8");
  if (!original.includes(find)) throw new Error(`probe marker missing in ${file}`);
  if (original.includes(token)) throw new Error(`stale probe token already in ${file}`);
  const edited = original.replace(find, token);
  const sees = async (marker) => {
    try {
      const { status, body } = await get(port, urlPath);
      return status === 200 && body.includes(marker);
    } catch {
      return false;
    }
  };
  if (!(await sees(find))) throw new Error(`baseline marker not served at ${urlPath} before edit`);
  const touch = () => fs.writeFileSync(file, fs.readFileSync(file, "utf8"));
  const t0 = Date.now();
  let latencyMs = -1;
  try {
    fs.writeFileSync(file, edited);
    let touched = Date.now();
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (await sees(token)) break;
      if (Date.now() >= deadline) throw new Error(`edit not visible at ${urlPath} within ${timeoutMs}ms`);
      if (Date.now() - touched >= 3000) {
        touch();
        touched = Date.now();
      }
      await sleep(250);
    }
    latencyMs = Date.now() - t0;
  } finally {
    fs.writeFileSync(file, original);
    let touched = Date.now();
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (await sees(find)) break;
      if (Date.now() >= deadline) throw new Error(`restore of ${file} not visible at ${urlPath}`);
      if (Date.now() - touched >= 3000) {
        touch();
        touched = Date.now();
      }
      await sleep(250);
    }
  }
  return latencyMs;
}

// CSS probe: same pattern, appending a comment token. The served CSS URL is
// discovered from the homepage HTML (first same-origin .css link, preferring
// a globals match); falls back to /src/globals.css.
async function cssUrlFor(port) {
  const { body } = await get(port, "/");
  const hrefs = [...body.matchAll(/<link[^>]+href="([^"]+\.css[^"]*)"/g)].map((m) => m[1]);
  const same = hrefs.filter((h) => h.startsWith("/") || h.includes(`localhost:${port}`));
  if (!same.length) return "/src/globals.css";
  const globals = same.find((h) => h.includes("globals"));
  if (globals)
    return new URL(globals, `http://localhost:${port}`).pathname + new URL(globals, `http://localhost:${port}`).search;
  const u = new URL(same[0], `http://localhost:${port}`);
  return u.pathname + u.search;
}

async function cssProbe(port) {
  const file = path.join(APP, "src", "globals.css");
  const token = `BENCHCSS-${Date.now().toString(36).toUpperCase()}`;
  const urlPath = await cssUrlFor(port);
  const original = fs.readFileSync(file, "utf8");
  // A real rule, not a comment: lightningcss strips plain comments, so a
  // comment token would never arrive in the served CSS.
  const probeLine = `\n.benchcss-${token.toLowerCase()}{display:none}\n`;
  // Class names are case-sensitive; the served CSS carries the lowercased form.
  const marker = token.toLowerCase();
  const sees = async (marker) => {
    try {
      const { status, body } = await get(port, urlPath);
      return status === 200 && body.includes(marker);
    } catch {
      return false;
    }
  };
  const touch = () => fs.writeFileSync(file, fs.readFileSync(file, "utf8"));
  const t0 = Date.now();
  let latencyMs = -1;
  try {
    fs.writeFileSync(file, original + probeLine);
    let touched = Date.now();
    const deadline = Date.now() + 60000;
    for (;;) {
      if (await sees(marker)) break;
      if (Date.now() >= deadline) throw new Error(`CSS edit not visible at ${urlPath} within 60s`);
      if (Date.now() - touched >= 3000) {
        touch();
        touched = Date.now();
      }
      await sleep(250);
    }
    latencyMs = Date.now() - t0;
  } finally {
    fs.writeFileSync(file, original);
    let touched = Date.now();
    const deadline = Date.now() + 60000;
    for (;;) {
      const { status, body } = await get(port, urlPath).catch(() => ({}));
      if (status === 200 && body != null && !body.includes(marker)) break;
      if (Date.now() >= deadline) throw new Error(`CSS restore of ${file} not visible at ${urlPath}`);
      if (Date.now() - touched >= 3000) {
        touch();
        touched = Date.now();
      }
      await sleep(250);
    }
  }
  return { ms: latencyMs, urlPath };
}

function readTimingFile(f) {
  try {
    const raw = fs.readFileSync(f, "utf8");
    const json = JSON.parse(raw);
    const rows = Array.isArray(json.plugins) ? json.plugins : [];
    rows.sort((a, b) => b.ms - a.ms);
    return rows;
  } catch (e) {
    return { error: `no timing data at ${f}: ${e.message}` };
  }
}

// Harness-only timing wrapper for Vite: a temp config (outside the app, the
// app's vite.config.ts is never edited) that re-exports the app config with
// every plugin's transform hook wrapped in a performance.now() accumulator,
// flushed to VITE_PLUGIN_TIMING_FILE. No behavior change besides timing.
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

async function runSide({ side, port, routes, timingFile, viteTimingConfig }) {
  killPort(port);
  await sleep(500);
  fs.rmSync(timingFile, { force: true });
  let proc;
  let log = "";
  if (side === "vite") {
    // As the app runs it: `bun run vite:dev` == `vite --host`.
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
    // As the ops unit runs it: `oj dev --port 3016 --host=127.0.0.1`.
    proc = spawn(OJ, ["dev", APP, "--port", String(port), "--host=127.0.0.1"], {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, OJ_PLUGIN_TIMING: "1", OJ_PLUGIN_TIMING_FILE: timingFile },
    });
  }
  proc.stdout.on("data", (d) => (log += d));
  proc.stderr.on("data", (d) => (log += d));
  const fail = async (msg) => {
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

    const rss = await settledRss(proc.pid);
    if (proc.exitCode !== null) await fail(`exited after crawl (exit=${proc.exitCode})`);
    const pluginsPostCrawl = readTimingFile(timingFile);

    const tsxToken = `CookieBench${Date.now().toString(36).toUpperCase()}XZ`;
    const tsxMs = await editProbe(port, {
      file: path.join(APP, "src", "components", "cookie-settings-button.tsx"),
      find: "Cookie-Einstellungen",
      token: tsxToken,
      urlPath: "/",
    }).catch((e) => fail(`tsx edit probe: ${e.message}`));

    const css = await cssProbe(port).catch((e) => fail(`css edit probe: ${e.message}`));

    // Timing files flush on a ~2s throttle: settle so the final edit/restore
    // records land before the snapshot read.
    await sleep(2500);
    const pluginsFinal = readTimingFile(timingFile);
    return {
      side,
      port,
      bootMs,
      routes: crawl.length,
      non200: bad,
      rssKb: rss.kb,
      rssSamplesKb: rss.samples,
      tsxMs,
      cssMs: css.ms,
      cssUrl: css.urlPath,
      pluginsPostCrawl,
      pluginsFinal,
    };
  } finally {
    await killAndWait(proc);
    killPort(port);
  }
}

function summarizePlugins(rows) {
  if (!Array.isArray(rows)) return { error: rows?.error ?? "no data" };
  const total = rows.reduce((s, r) => s + r.ms, 0);
  return {
    totalMs: Math.round(total * 100) / 100,
    rows: rows.map((r) => ({ ...r, share: total ? Math.round((r.ms / total) * 1000) / 10 : 0 })),
  };
}

const routes = deriveRoutes();
console.log(`routes (${routes.length}): ${routes.slice(0, 8).join(" ")}${routes.length > 8 ? " ..." : ""}`);
if (!fs.existsSync(OJ)) throw new Error(`oj binary missing at ${OJ} (build it: cargo build --release)`);
fs.mkdirSync(OUT_DIR, { recursive: true });
const viteTimingConfig = writeViteTimingConfig(OUT_DIR);

const results = { app: APP, oj: OJ, at: new Date().toISOString(), method: {}, runs: [] };
results.method = {
  viteCmd:
    "bun run vite:dev -- --port <port> --strictPort --config <harness timing wrapper> (cwd=app; `vite --host` as the app runs it)",
  ojCmd: "oj dev <app> --port <port> --host=127.0.0.1 + OJ_PLUGIN_TIMING=1 (as ops/intersolaris-oj.service runs it)",
  boot: "spawn -> first HTTP 200 on /, polled every 200ms",
  routes: "seo.ts path: + contentList.ts path: regex (the vite.config prerender set), sequential GET, status recorded",
  rss: "settled 8s after crawl, then 3x VmRSS summed over the whole process tree via /proc (children by PPid scan), median of 3 samples 3s apart; identical procedure both sides",
  tsxEdit:
    "cookie-settings-button.tsx Cookie-Einstellungen -> unique token, poll / until served (200), restore + poll restore",
  cssEdit:
    "append comment token to src/globals.css, poll discovered served CSS URL until token, restore + poll restore",
  pluginTiming:
    "OJ: opt-in OJ_PLUGIN_TIMING=1 in plugin-host.mjs transform dispatch (off by default). Vite: harness-only wrapper config (app vite.config untouched). Wall-time + call counts per plugin name.",
};

for (let run = 1; run <= RUNS; run++) {
  console.log(`\n=== run ${run}/${RUNS} ===`);
  const runOut = { run, sides: {} };
  for (const side of ["vite", "oj"]) {
    const port = side === "vite" ? VITE_PORT : OJ_PORT;
    const timingFile = path.join(OUT_DIR, `${side}-timing-run${run}.json`);
    console.log(`-- ${side} on :${port} ...`);
    const t0 = Date.now();
    runOut.sides[side] = await runSide({ side, port, routes, timingFile, viteTimingConfig });
    console.log(
      `   boot=${runOut.sides[side].bootMs}ms routes=${runOut.sides[side].routes} non200=${runOut.sides[side].non200.length} ` +
        `rss=${Math.round(runOut.sides[side].rssKb / 1024)}MB tsx=${runOut.sides[side].tsxMs}ms css=${runOut.sides[side].cssMs}ms ` +
        `(${((Date.now() - t0) / 1000) | 0}s wall)`,
    );
    if (runOut.sides[side].non200.length) {
      console.log(`   non-200: ${JSON.stringify(runOut.sides[side].non200.slice(0, 10))}`);
    }
  }
  results.runs.push(runOut);
}

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const jsonPath = path.join(OUT_DIR, `results-${stamp}.json`);
fs.writeFileSync(jsonPath, JSON.stringify(results, null, 2));

let md = `# Native solid plugins bench (T1)\n\n${results.at} — app ${APP}, oj ${OJ}\n\n## Method\n\n`;
for (const [k, v] of Object.entries(results.method)) md += `- **${k}**: ${v}\n`;
md += `\n## Baseline table\n\n| side | run | boot ms | routes 200/total | RSS MB (median, samples kB) | tsx edit ms | css edit ms |\n|---|---|---|---|---|---|---|\n`;
for (const r of results.runs) {
  for (const side of ["vite", "oj"]) {
    const s = r.sides[side];
    md += `| ${side} | ${r.run} | ${s.bootMs} | ${s.routes - s.non200.length}/${s.routes} | ${(s.rssKb / 1024).toFixed(0)} (${s.rssSamplesKb.join("/")}) | ${s.tsxMs} | ${s.cssMs} |\n`;
  }
}
function pluginSection(title, rows) {
  const s = summarizePlugins(rows);
  let out = `\n### ${title}\n\n`;
  if (s.error) return out + `no timing data: ${s.error}\n`;
  out += `total measured transform wall: ${s.totalMs} ms\n\n| plugin | env | calls | wall ms | share % |\n|---|---|---|---|---|\n`;
  for (const row of s.rows.slice(0, 20)) {
    out += `| ${row.name} | ${row.env ?? "-"} | ${row.calls} | ${row.ms} | ${row.share} |\n`;
  }
  if (s.rows.length > 20) out += `| ... (${s.rows.length - 20} more) | | | | |\n`;
  return out;
}

// Edit-phase delta: final minus post-crawl snapshot, per plugin+env.
function deltaSection(title, before, after) {
  let out = `\n### ${title}\n\n`;
  if (!Array.isArray(before) || !Array.isArray(after)) return out + "no timing data for delta\n";
  const key = (r) => `${r.env ?? ""}::${r.name}`;
  const bmap = new Map(before.map((r) => [key(r), r]));
  const deltas = [];
  for (const r of after) {
    const b = bmap.get(key(r)) ?? { calls: 0, ms: 0 };
    const dc = r.calls - b.calls;
    const dm = Math.round((r.ms - b.ms) * 100) / 100;
    if (dc > 0 || dm !== 0) deltas.push({ name: r.name, env: r.env, calls: dc, ms: dm });
  }
  deltas.sort((a, b) => b.ms - a.ms);
  const total = deltas.reduce((s, r) => s + r.ms, 0);
  out += `total edit-phase transform wall: ${Math.round(total * 100) / 100} ms (2 edits + restores)\n\n| plugin | env | calls | wall ms |\n|---|---|---|---|\n`;
  for (const row of deltas.slice(0, 15)) {
    out += `| ${row.name} | ${row.env ?? "-"} | ${row.calls} | ${row.ms} |\n`;
  }
  if (!deltas.length) out += "no additional transforms during edit probes\n";
  return out;
}

md += `\n## Per-plugin transform time (wall ms, calls, share)\n`;
for (const r of results.runs) {
  for (const side of ["vite", "oj"]) {
    const s = r.sides[side];
    md += pluginSection(`${side} run ${r.run} — boot+crawl`, s.pluginsPostCrawl);
    md += pluginSection(`${side} run ${r.run} — full run (incl. edits)`, s.pluginsFinal);
    md += deltaSection(`${side} run ${r.run} — edit-phase delta`, s.pluginsPostCrawl, s.pluginsFinal);
  }
}
md += `\n## RSS stability (±10% across runs)\n\n`;
for (const side of ["vite", "oj"]) {
  const rss = results.runs.map((r) => r.sides[side].rssKb);
  if (rss.length > 1) {
    const drift = ((Math.max(...rss) - Math.min(...rss)) / Math.min(...rss)) * 100;
    md += `- ${side}: ${rss.map((k) => `${(k / 1024).toFixed(0)}MB`).join(" vs ")} — drift ${drift.toFixed(1)}% ${drift <= 10 ? "(stable)" : "(UNSTABLE — see blockers)"}\n`;
  }
}
const mdPath = path.join(OUT_DIR, `results-${stamp}.md`);
fs.writeFileSync(mdPath, md);
console.log(`\nwrote ${jsonPath}\nwrote ${mdPath}`);
console.log(md);
