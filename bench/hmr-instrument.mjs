//node bench/hmr-instrument.mjs <app-dir> [edits]
import { chromium } from "playwright";
import { spawn, execSync } from "node:child_process";
import { writeFileSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.resolve(process.argv[2] ?? path.join(here, "apps", "app-1000"));
const EDITS = Number(process.argv.find((a, i) => i >= 3 && /^\d+$/.test(a)) ?? 20);
const OJ = path.join(repo, "target", "release", "oj");
const PORT = 5199;
const N = path.basename(app).replace("app-", "");
const leaf = path.join(app, "src", "components", `Comp${N - 1}.tsx`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// injected before any page script: wrap WebSocket + fetch, stamp DOM arrival.
const PROBE = `
(() => {
  const OWS = window.WebSocket;
  class WS extends OWS {
    constructor(...a){ super(...a);
      this.addEventListener('message', e => {
        (window.__hmr ||= []).push({ perf: performance.now(), wall: Date.now(), data: String(e.data).slice(0,140) });
      });
    }
  }
  window.WebSocket = WS;
  const of = window.fetch;
  window.fetch = function(u, ...r){
    const url = String(u), t0 = performance.now();
    return of.call(this, u, ...r).then(res => { (window.__fetch ||= []).push({ url, start: t0, end: performance.now() }); return res; });
  };
  const mo = new MutationObserver(() => {
    if (window.__target && !window.__domHit && document.body && document.body.textContent.includes(window.__target))
      window.__domHit = { perf: performance.now(), wall: Date.now() };
  });
  addEventListener('DOMContentLoaded', () => mo.observe(document.body, { subtree:true, childList:true, characterData:true }));
})();
`;

function pct(xs, p) { const s=[...xs].sort((a,b)=>a-b); return s[Math.min(s.length-1, Math.ceil(p/100*s.length)-1)]; }
const med = (xs) => pct(xs, 50);

async function main() {
  try { execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { stdio: "ignore" }); } catch {}
  try { execSync(`rm -rf ${app}/.oj-cache`); } catch {}
  const args = ["dev", app, "--port", String(PORT)];
  const proc = spawn(OJ, args, { stdio: "ignore" });
  // wait for server
  for (let i=0;i<600;i++){ try { const r=await fetch(`http://localhost:${PORT}/`); if(r.ok) break; } catch{} await sleep(50); }

  const browser = await chromium.launch();
  const page = await browser.newPage();
  await page.addInitScript(PROBE);
  await page.goto(`http://localhost:${PORT}/`, { timeout: 120000 });
  await page.waitForSelector("[data-done]", { timeout: 120000 });

  const orig = readFileSync(leaf, "utf8");
  const rows = [];
  for (let e = 0; e < EDITS + 1; e++) {
    const marker = `marker-P${e}-${Date.now()}`;
    await page.evaluate((m) => { window.__hmr = []; window.__fetch = []; window.__domHit = null; window.__target = m; }, marker);
    await sleep(160); // clear watcher debounce window between edits
    const tEdit = Date.now();
    writeFileSync(leaf, orig.replace(/marker-\w+/, marker));
    await page.waitForFunction((m) => window.__domHit && document.body.textContent.includes(m), marker, { timeout: 30000, polling: 8 });
    const d = await page.evaluate((tEdit) => {
      const hmr = (window.__hmr||[]).filter(x => x.wall >= tEdit - 5);
      const ws = hmr[0];
      const dom = window.__domHit;
      const fetches = (window.__fetch||[]).filter(f => ws && f.start >= ws.perf - 1);
      return { ws, dom, fetches, nHmr: hmr.length };
    }, tEdit);
    if (e === 0) continue;
    if (!d.ws || !d.dom) { rows.push(null); continue; }
    const serverTransport = d.ws.wall - tEdit;
    const clientApply = d.dom.perf - d.ws.perf;
    const total = d.dom.wall - tEdit;
    const fetchMs = d.fetches.reduce((s,f)=>s+(f.end-f.start),0);
    rows.push({ serverTransport, clientApply, total, fetchMs, nFetch: d.fetches.length, msg: d.ws.data });
  }
  writeFileSync(leaf, orig);
  await browser.close();
  proc.kill("SIGKILL");
  try { execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { stdio: "ignore" }); } catch {}

  const ok = rows.filter(Boolean);
  const g = (k) => ok.map(r => r[k]);
  console.log(`\napp-${N} — ${ok.length} edits, medians (ms):`);
  console.log(`  total save->paint      : ${med(g("total")).toFixed(1)}`);
  console.log(`  server + transport     : ${med(g("serverTransport")).toFixed(1)}   (edit -> HMR msg: debounce + recompile + serialize + ws)`);
  console.log(`  client apply           : ${med(g("clientApply")).toFixed(1)}   (HMR msg -> DOM paint: fetch + refresh + rerender)`);
  console.log(`    of which module fetch: ${med(g("fetchMs")).toFixed(1)}   (n=${med(g("nFetch"))} req)`);
  console.log(`  sample HMR msg         : ${ok[0]?.msg}`);
}
main().catch(e => { console.error(e); process.exit(1); });
