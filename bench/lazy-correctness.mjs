import { chromium } from "playwright";
import { spawn, execSync } from "node:child_process";
import { mkdirSync, writeFileSync, rmSync, symlinkSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const OJ = path.join(repo, "target", "release", "oj");
const app = path.join(here, "apps", "lazy-nav");
const donor = path.join(here, "apps", "app-1000");
const PORT = 5199;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

rmSync(app, { recursive: true, force: true });
mkdirSync(path.join(app, "src", "routes"), { recursive: true });
for (let r = 0; r < 3; r++) {
  writeFileSync(
    path.join(app, "src", "routes", `Route${r}.tsx`),
    `import { useState } from "react";
export default function Route${r}() {
  const [n] = useState(${r}00);
  return <section data-route="${r}"><h2>route ${r}</h2><p>hook value {n}</p></section>;
}
`,
  );
}
writeFileSync(
  path.join(app, "src", "App.tsx"),
  `import { lazy, Suspense, useState } from "react";
import Route0 from "./routes/Route0";
const Route1 = lazy(() => import("./routes/Route1"));
const Route2 = lazy(() => import("./routes/Route2"));
const routes: Record<number, any> = { 0: Route0, 1: Route1, 2: Route2 };
export function App() {
  const [r, setR] = useState(0);
  const Route = routes[r];
  return (
    <main data-done="yes">
      <nav>{[0,1,2].map((i) => <button key={i} data-nav={i} onClick={() => setR(i)}>go {i}</button>)}</nav>
      <Suspense fallback={<div data-loading>loading</div>}><Route /></Suspense>
    </main>
  );
}
`,
);
writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";
import { App } from "./App";
createRoot(document.getElementById("root")!).render(<App />);
`,
);
writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><meta charset="utf-8"/></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>\n`,
);
writeFileSync(path.join(app, "package.json"), `{ "name": "lazy-nav", "private": true, "type": "module" }\n`);
rmSync(path.join(app, "node_modules"), { recursive: true, force: true });
symlinkSync(path.join(donor, "node_modules"), path.join(app, "node_modules"), "dir");
rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });

try { execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { stdio: "ignore" }); } catch {}
const proc = spawn(OJ, ["dev", app, "--port", String(PORT),], { stdio: "ignore" });
const errors = [];
let pass = true;
const check = (cond, msg) => { console.log(`  ${cond ? "OK " : "FAIL"} ${msg}`); if (!cond) pass = false; };
try {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
  page.on("pageerror", (e) => errors.push(String(e)));
  for (let i = 0; i < 200; i++) { try { await page.goto(`http://localhost:${PORT}/`, { timeout: 2000 }); break; } catch { await sleep(30); } }
  await page.waitForSelector('[data-route="0"]', { timeout: 30000 });
  check(true, "landing route 0 rendered (eager)");
  // lazy chunk requests we can observe
  const lazyReqs = [];
  page.on("request", (req) => { if (req.url().includes("/@oj/lazy.js")) lazyReqs.push(req.url()); });
  // navigate to lazy route 1
  await page.click('[data-nav="1"]');
  await page.waitForSelector('[data-route="1"]', { timeout: 30000 });
  const hook1 = await page.textContent('[data-route="1"] p');
  check(/hook value 100/.test(hook1 || ""), `lazy route 1 rendered with working hook (${JSON.stringify(hook1)})`);
  check(lazyReqs.some((u) => u.includes("Route1")), "lazy chunk fetched via /@oj/lazy.js");
  // navigate to lazy route 2
  await page.click('[data-nav="2"]');
  await page.waitForSelector('[data-route="2"]', { timeout: 30000 });
  const hook2 = await page.textContent('[data-route="2"] p');
  check(/hook value 200/.test(hook2 || ""), `lazy route 2 rendered with working hook (${JSON.stringify(hook2)})`);
  // back to 0 (still works)
  await page.click('[data-nav="0"]');
  await page.waitForSelector('[data-route="0"]', { timeout: 5000 });
  check(true, "navigated back to route 0");
  check(errors.length === 0, `no console/page errors (${errors.length}: ${errors.slice(0,2).join(" | ")})`);
  await browser.close();
} finally {
  proc.kill("SIGKILL");
  try { execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { stdio: "ignore" }); } catch {}
}
console.log(pass ? "\nBUNDLE-LAZY CORRECTNESS PASSED" : "\nBUNDLE-LAZY CORRECTNESS FAILED");
process.exit(pass ? 0 : 1);
