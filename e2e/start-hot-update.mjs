// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Start dev applies an edit in place, as Vite's bundled dev does: the client
// bundler's engine turns the save into a patch for the page, React Fast
// Refresh swaps the component, and the page keeps its state (no reload).
// An edit no module accepts reloads the page into the fresh bundle, and so
// does a manual reload right after a hot update: the document waits for an
// output that carries it. The patch protocol and the reload decision are
// covered in Rust (start_hot.rs, start_dev.rs); this checks the browser
// wiring end to end.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { rmrf, settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5404);

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start hot update: fixture deps not installed");
  process.exit(0);
}
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const widget = path.join(app, "src", "widgets", "glob-widget.jsx");
const format = path.join(app, "src", "lib", "format.ts");
const sheet = path.join(app, "src", "widgets", "hot-sheet.css");
const draft = path.join(app, "src", "content", "draft.txt");
const miniflare = path.join(app, ".wrangler", "state", "v3", "trace-store", "a.sqlite");
const stray = path.join(app, "stray-notes.txt");
const originals = new Map([widget, format].map((f) => [f, fs.readFileSync(f, "utf8")]));
const restore = () => {
  for (const [f, src] of originals) fs.writeFileSync(f, src);
  fs.rmSync(sheet, { force: true });
  fs.rmSync(draft, { force: true });
  fs.rmSync(stray, { force: true });
  fs.rmSync(path.join(app, ".wrangler"), { recursive: true, force: true });
};
process.on("exit", restore);

// The async rm: a previous run's server can still be landing cache writes.
await rmrf(path.join(app, ".oj-cache"));
let log = "";
// Its own process group: the bundler and plugin host children go with it.
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { detached: true, stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));
const exited = new Promise((r) => srv.once("exit", r));
const stop = async () => {
  try {
    process.kill(-srv.pid, "SIGTERM");
  } catch {}
  const kill = setTimeout(() => {
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
  }, 3000);
  await exited;
  clearTimeout(kill);
};

const browser = await chromium.launch();
let failed = false;
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector('[data-testid="client-mounted"]', { timeout: 60000 });
  await page.click('[data-testid="counter"]');
  await page.waitForSelector('[data-testid="counter"]:has-text("count: 1")');
  await page.evaluate(() => (window.__hotProbe = "kept"));

  // A component edit: patched in place, state and page kept.
  fs.writeFileSync(widget, originals.get(widget).replace('"jsxglob:"', '"jsxhot:"'));
  await page.waitForSelector('[data-testid="glob-jsx"]:has-text("jsxhot:")', { timeout: 30000 });
  must((await page.textContent('[data-testid="counter"]')) === "count: 1", "the hot update reset component state");
  must((await page.evaluate(() => window.__hotProbe)) === "kept", "the hot update reloaded the page");
  // The server's own pass over the edit (its render reloads the module).
  must(
    await settles(() => log.includes("oj start: hot updated"), { timeoutMs: 30000, pollMs: 50 }),
    "the dev server did not report a hot update",
  );

  // Vite re-expands a glob only for an added file its patterns match. An
  // edit batched with a create the fixture's `../content/*.json` globs never
  // expand to still patches in place: a file under their scan base they do
  // not match, and a matching one that came and went before the batch
  // settled (GNU sed's temp, an editor's safe-write).
  const hotCount = () => log.split("oj start: hot updated").length - 1;
  for (const [label, create] of [
    ["a create no glob pattern matches", () => fs.writeFileSync(draft, "draft")],
    [
      // Miniflare writes its state on SSR requests; @cloudflare/vite-plugin
      // ignores it from 1.55 on, and before that Vite's graphs never knew it.
      "a write outside every module graph and glob (miniflare state)",
      () => {
        fs.mkdirSync(path.dirname(miniflare), { recursive: true });
        fs.writeFileSync(miniflare, String(Date.now()));
      },
    ],
    [
      "a matching temp gone before the batch settled",
      () => {
        const temp = path.join(app, "src", "content", "sedf1aa6r.json");
        fs.writeFileSync(temp, "{}");
        return () => fs.rmSync(temp);
      },
    ],
  ]) {
    const before = hotCount();
    const mark = log.length;
    const text = `jsx${before}:`;
    const after = create();
    fs.writeFileSync(widget, originals.get(widget).replace('"jsxglob:"', JSON.stringify(text)));
    after?.();
    await page.waitForSelector(`[data-testid="glob-jsx"]:has-text("${text}")`, { timeout: 30000 });
    must(
      await settles(() => hotCount() > before || /oj start: (rebuilt|updated)/.test(log.slice(mark)), {
        timeoutMs: 30000,
        pollMs: 50,
      }),
      `${label}: the dev server did not report the batch`,
    );
    must(hotCount() > before, `${label} turned the edit into a rebuild:\n${log.slice(mark)}`);
    must((await page.evaluate(() => window.__hotProbe)) === "kept", `${label} reloaded the page`);
  }

  // import.meta.hot.send reaches a plugin's server.ws.on, and its
  // server.ws.send reply reaches the page's import.meta.hot.on.
  fs.writeFileSync(
    widget,
    originals.get(widget).replace('"jsxglob:"', '"jsxhot:"') +
      '\nif (import.meta.hot) {\n  import.meta.hot.on("fixture:pong", (d) => (window.__pong = d));\n' +
      '  import.meta.hot.send("fixture:ping", { n: 1 });\n}\n',
  );
  await page.waitForFunction(() => window.__pong, null, { timeout: 30000 });
  must(
    JSON.stringify(await page.evaluate(() => window.__pong)) === '{"echo":{"n":1}}',
    "the plugin's reply carried the wrong data",
  );
  must((await page.evaluate(() => window.__hotProbe)) === "kept", "the hot.send edit reloaded the page");

  // A reload right after serves the edit from the server render and bundle.
  const html = await (await fetch(`http://localhost:${PORT}/`)).text();
  must(html.includes("jsxhot:"), "the server render after a hot update is stale");
  await page.reload();
  await page.waitForSelector('[data-testid="client-mounted"]', { timeout: 60000 });
  must(
    (await page.textContent('[data-testid="glob-jsx"]')).includes("jsxhot:"),
    "a reload after a hot update loaded the old bundle",
  );

  // A stylesheet the patched component imports for the first time: linked in
  // place (no reload), and in the server render's head from then on.
  await page.click('[data-testid="counter"]');
  await page.waitForSelector('[data-testid="counter"]:has-text("count: 1")');
  await page.evaluate(() => (window.__hotProbe = "kept"));
  fs.writeFileSync(sheet, ".hot-sheet { color: rgb(1, 2, 3); }\n");
  fs.writeFileSync(
    widget,
    'import "./hot-sheet.css";\n' +
      originals
        .get(widget)
        .replace('"jsxglob:"', '"jsxsheet:"')
        .replace("return <span>", 'return <span className="hot-sheet">'),
  );
  await page.waitForSelector('[data-testid="glob-jsx"]:has-text("jsxsheet:")', { timeout: 30000 });
  await page.waitForFunction(
    () => {
      const el = document.querySelector(".hot-sheet");
      return el && getComputedStyle(el).color === "rgb(1, 2, 3)";
    },
    null,
    { timeout: 30000 },
  );
  must((await page.evaluate(() => window.__hotProbe)) === "kept", "a new stylesheet import reloaded the page");
  must((await page.textContent('[data-testid="counter"]')) === "count: 1", "a new stylesheet import reset state");
  const head = await (await fetch(`http://localhost:${PORT}/`)).text();
  must(head.includes("hot-sheet.css"), "the server render does not link the new stylesheet");
  // The client output catches up once edits pause and rewrites the manifest
  // the bundle imports: that update applies in place too.
  await sleep(5000);
  must((await page.evaluate(() => window.__hotProbe)) === "kept", "the manifest rewrite reloaded the page");
  // That rewrite changed files the server render had imported, behind the
  // watcher's back. An unrelated file event next (an editor's or Finder's
  // stray write) names nothing any module graph knows, so it reloads
  // nothing: Vite acts on an event through that event's own file alone.
  const strayMark = log.length;
  fs.writeFileSync(stray, "notes");
  must(
    await settles(() => log.slice(strayMark).includes("stray-notes.txt"), { timeoutMs: 30000, pollMs: 50 }),
    "the watcher never reported the stray file",
  );
  await sleep(3000);
  must(
    (await page.evaluate(() => window.__hotProbe)) === "kept",
    `an unrelated file event reloaded the page:\n${log.slice(strayMark)}`,
  );

  // An edit no module accepts (a plain function the route imports): the page
  // reloads into the fresh bundle.
  await page.evaluate(() => (window.__hotProbe = "kept"));
  const shouted = originals.get(format).replace("toUpperCase()", "toUpperCase() + '~'");
  must(shouted !== originals.get(format), "format.ts no longer has the edit point");
  fs.writeFileSync(format, shouted);
  await page.waitForFunction(() => window.__hotProbe === undefined, null, { timeout: 30000 });
  await page.waitForSelector('[data-testid="client-mounted"]', { timeout: 60000 });
  must(
    (await page.textContent("h1.fixture-heading")).includes("~"),
    "the reload after an unaccepted edit missed the edit",
  );
  must(errors.length === 0, `page errors: ${errors.join(" | ")}`);
  console.log(
    "start hot update: component patched in place (state kept), reload fresh, new stylesheet linked in place, unaccepted edit reloads",
  );
} catch (e) {
  failed = true;
  console.error(e);
  console.error(log.split("\n").slice(-40).join("\n"));
} finally {
  await browser.close();
  await stop();
  restore();
}
process.exit(failed ? 1 : 0);
