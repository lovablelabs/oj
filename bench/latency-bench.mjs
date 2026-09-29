// SPDX-License-Identifier: MIT
// Latency benchmark for oj-native partial bundling.
//
// The request-count win from collapsing many per-file dependency modules into a
// few `/@oj-pkg` bundles is ~free on localhost but decisive over a network —
// remote/containerized dev, where every request pays an RTT *and* the browser
// serializes them across ~6 connections per host (HTTP/1.1). This benchmark
// models that faithfully: it puts a latency proxy in front of oj (each response
// delayed by a fixed RTT) and drives the app through it with a real browser, so
// the browser's own connection limit does the serialization — the thing that
// turns "many small requests" into wall-clock. It reports time-to-first-render
// and full page-load, partial bundling OFF vs ON.
//
// Usage: node latency-bench.mjs   (from a dir where "playwright" resolves)
//   env: OJ_BIN, APP, ITERS, LATENCIES (csv ms)
import { spawn, execSync } from "node:child_process";
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { chromium } from "playwright";

const OJ_BIN = process.env.OJ_BIN || "/Users/rapha/Documents/a/oj-partial-bundle/target/release/oj";
const APP = process.env.APP || "/Users/rapha/Documents/a/oj-partial-bundle/bench/pbench-app";
const PORT = parseInt(process.env.PORT || "5599", 10);
const PROXY = parseInt(process.env.PROXY || "5600", 10);
const ITERS = parseInt(process.env.ITERS || "4", 10);
const LATENCIES = (process.env.LATENCIES || "0,10,25,50").split(",").map((s) => parseInt(s.trim(), 10));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor((s.length - 1) / 2)];
};

function killPort(p) {
  try {
    execSync(`lsof -ti:${p} -sTCP:LISTEN | xargs -r kill -9`, {
      stdio: "ignore",
    });
  } catch {}
}

// Latency proxy: forward every request to oj, delaying the response head by
// `latencyMs` (a one-way RTT model). The browser opens its usual ~6 keep-alive
// connections to this proxy, so requests beyond 6 in flight queue — exactly the
// serialization a remote dev server sees.
let currentLatency = 0;
function startProxy() {
  const server = http.createServer((req, res) => {
    const opts = {
      host: "127.0.0.1",
      port: PORT,
      path: req.url,
      method: req.method,
      headers: req.headers,
    };
    const fwd = http.request(opts, (up) => {
      setTimeout(() => {
        res.writeHead(up.statusCode || 502, up.headers);
        up.pipe(res);
      }, currentLatency);
    });
    fwd.on("error", () => {
      res.writeHead(502);
      res.end("proxy error");
    });
    req.pipe(fwd);
  });
  server.keepAliveTimeout = 60000;
  return new Promise((resolve) => server.listen(PROXY, () => resolve(server)));
}

async function waitForServer(port, timeoutMs = 120000) {
  const t = Date.now();
  while (Date.now() - t < timeoutMs) {
    try {
      const r = await fetch(`http://localhost:${port}/`, {
        signal: AbortSignal.timeout(2000),
      });
      if (r.ok) return;
    } catch {}
    await sleep(100);
  }
  throw new Error(`server on ${port} never came up`);
}

function startServer(partial) {
  fs.rmSync(path.join(APP, ".oj-cache"), { recursive: true, force: true });
  const env = { ...process.env };
  if (partial) env.OJ_PARTIAL_BUNDLE = "1";
  else delete env.OJ_PARTIAL_BUNDLE;
  return spawn(OJ_BIN, ["dev", APP, "--port", String(PORT)], {
    stdio: "ignore",
    env,
  });
}

// One navigation through the proxy: returns { ttfr, load, requests }.
// ttfr = time to first render (#root populated); load = time until requests go
// quiet for 400ms (all modules fetched).
async function render(browser) {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  let requests = 0;
  let lastActivity = Date.now();
  page.on("request", () => {
    requests++;
    lastActivity = Date.now();
  });
  page.on("requestfinished", () => (lastActivity = Date.now()));
  page.on("requestfailed", () => (lastActivity = Date.now()));
  const t = Date.now();
  await page.goto(`http://localhost:${PROXY}/`, {
    waitUntil: "commit",
    timeout: 240000,
  });
  await page.waitForFunction(
    "document.getElementById('root') && document.getElementById('root').children.length > 0",
    undefined,
    { timeout: 240000, polling: 50 },
  );
  const ttfr = Date.now() - t;
  // settle: wait until 400ms pass with no new request activity
  while (Date.now() - lastActivity < 400) await sleep(50);
  const load = Date.now() - t;
  await ctx.close();
  return { ttfr, load, requests };
}

async function benchMode(partial) {
  killPort(PORT);
  const proc = startServer(partial);
  const browser = await chromium.launch();
  const out = {};
  try {
    await waitForServer(PORT);
    currentLatency = 0;
    await render(browser); // warm: build every lazy bundle + transform once
    for (const lat of LATENCIES) {
      currentLatency = lat;
      const ttfrs = [];
      const loads = [];
      let reqs = 0;
      for (let i = 0; i < ITERS; i++) {
        const r = await render(browser);
        ttfrs.push(r.ttfr);
        loads.push(r.load);
        reqs = r.requests;
      }
      out[lat] = { ttfr: median(ttfrs), load: median(loads), requests: reqs };
      process.stderr.write(
        `  ${partial ? "on " : "off"} lat=${lat}ms -> ttfr ${out[lat].ttfr}ms, load ${out[lat].load}ms, ${reqs} reqs\n`,
      );
    }
  } finally {
    await browser.close();
    try {
      execSync(`pkill -P ${proc.pid}`, { stdio: "ignore" });
    } catch {}
    try {
      proc.kill("SIGKILL");
    } catch {}
    killPort(PORT);
    await sleep(700);
  }
  return out;
}

const proxy = await startProxy();
process.stderr.write("benchmarking partial-bundle OFF...\n");
const off = await benchMode(false);
process.stderr.write("benchmarking partial-bundle ON...\n");
const on = await benchMode(true);
proxy.close();

console.log(`\npbench-app — median time-to-first-render / full-load, ${ITERS} iters, latency proxy in front of oj`);
console.log("latency | OFF ttfr / load (reqs)    | ON ttfr / load (reqs)     | load speedup");
console.log("--------|---------------------------|---------------------------|-------------");
for (const lat of LATENCIES) {
  const o = off[lat],
    n = on[lat];
  const sx = (o.load / n.load).toFixed(2) + "x";
  const cell = (r) => `${(r.ttfr + "/" + r.load + "ms").padEnd(14)} (${r.requests})`.padEnd(25);
  console.log(`${(lat + "ms").padEnd(7)} | ${cell(o)} | ${cell(n)} | ${sx}`);
}
console.log("\nraw:", JSON.stringify({ off, on }));
