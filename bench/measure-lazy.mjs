import { chromium } from "playwright";
import { spawn, execSync } from "node:child_process";
import { rmSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const OJ = path.join(repo, "target", "release", "oj");
const ITERS = Number(process.argv.find((a, i) => i >= 2 && /^\d+$/.test(a)) ?? 5);
const PORT = 5199;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const med = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

async function coldOnce(app, browser) {
  try { execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { stdio: "ignore" }); } catch {}
  rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const log = "/tmp/oj-lazy.log";
  const proc = spawn("sh", ["-c", `'${OJ}' dev '${app}' --port ${PORT} > ${log} 2>&1`]);
  const t0 = Date.now();
  const page = await browser.newPage();
  for (;;) {
    try { await page.goto(`http://localhost:${PORT}/`, { timeout: 2000 }); break; }
    catch { await sleep(30); if (Date.now() - t0 > 60000) throw new Error("no server"); }
  }
  await page.waitForSelector("[data-done]", { timeout: 60000 });
  const cold = Date.now() - t0;
  await sleep(400); // let the background crawl finish + log
  let crawl = "?";
  try { crawl = (readFileSync(log, "utf8").match(/eager graph ready: (\d+) modules in ([\d.]+\w+)/) || []).slice(1).join(" in "); } catch {}
  await page.close();
  proc.kill("SIGKILL");
  try { execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { stdio: "ignore" }); } catch {}
  await sleep(300);
  return { cold, crawl };
}

async function main() {
  const browser = await chromium.launch();
  for (const variant of ["eager", "lazy"]) {
    const app = path.join(here, "apps", `routes-${variant}`);
    const colds = [];
    let crawl = "?";
    for (let i = 0; i < ITERS; i++) {
      const r = await coldOnce(app, browser);
      colds.push(r.cold);
      crawl = r.crawl;
    }
    console.log(`routes-${variant.padEnd(5)}  cold(spawn->painted) median ${med(colds)}ms  min ${Math.min(...colds)}ms  | eager crawl: ${crawl}`);
  }
  await browser.close();
}
main().catch((e) => { console.error(e); process.exit(1); });
