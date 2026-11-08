// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The editor HMR gate and rewrites that change nothing. An editor can rewrite
// a file with the bytes it already has (regenerating a generated file before
// it flushes, a formatter's no-op pass). The pages already show that content,
// so the gate neither holds nor pushes it: with a full-reload gate, holding it
// cost a page reload on the next flush. An edit reverted to the flushed bytes
// before the flush drops its hold the same way. A real change is still held
// and released as before, and the server always serves what is on disk.
// What the page has also moves when a change reaches it without a flush, so
// a write back to older flushed bytes is never taken for a no-op.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { sleep, waitUp } from "./util.mjs";

const OJ = process.env.OJ_BIN ?? path.join(process.cwd(), "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5339);
const base = `http://localhost:${PORT}`;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-noop-rewrite-"));
const file = path.join(app, "src", "color.js");
const content = (v) => `export const color = ${JSON.stringify(v)};\n`;

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), '{"name":"noop-rewrite","private":true,"type":"module"}');
fs.writeFileSync(
  path.join(app, "index.html"),
  '<!doctype html><html><body><div id="app"></div><script type="module" src="/src/main.js"></script></body></html>',
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  'import { color } from "./color.js";\ndocument.getElementById("app").textContent = color;\n',
);
fs.writeFileSync(file, content("c0"));

let log = "";
const env = { ...process.env, OJ_HMR_GATE: "1" };
// The gate's default mode: every release reloads the pages.
delete env.OJ_HMR_FULL_RELOAD;
delete env.LOVABLE_HMR_FULL_RELOAD;
const srv = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app, stdio: ["ignore", "pipe", "pipe"], env });
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));

let failed = false;
let ws;
try {
  await waitUp(`${base}/`, { proc: srv });
  must(log.includes("hmr gate: on (full-reload)"), `the gate did not come up in full-reload mode:\n${log}`);
  const frames = [];
  ws = new WebSocket(`ws://localhost:${PORT}/__ws`, "vite-hmr");
  ws.addEventListener("message", (e) => frames.push(JSON.parse(e.data)));
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve);
    ws.addEventListener("error", reject);
  });
  for (const p of ["/", "/src/main.js", "/src/color.js"]) await (await fetch(base + p)).text();
  await sleep(500);

  const flush = async () => (await fetch(`${base}/__hmr_flush`, { method: "POST" })).json();
  const pending = async () => (await (await fetch(`${base}/__hmr_gate`)).json()).count;
  const reloads = () => frames.filter((f) => f.type === "full-reload" || f.type === "update").length;
  const reloaded = async (ms) => {
    const until = Date.now() + ms;
    while (Date.now() < until) {
      if (reloads() > 0) return true;
      await sleep(50);
    }
    return false;
  };

  for (let round = 1; round <= 2; round++) {
    const a = `a${round}`;
    // 1. A real edit: held, then released with a reload.
    frames.length = 0;
    fs.writeFileSync(file, content(a));
    await sleep(400);
    must((await pending()) === 1, `round ${round}: the edit was not held`);
    must((await flush()).count === 1, `round ${round}: the flush released nothing`);
    must(await reloaded(3000), `round ${round}: no reload after the flush`);
    await sleep(800);

    // 2. The same bytes again: not held, and the flush reloads nothing.
    frames.length = 0;
    fs.writeFileSync(file, content(a));
    await sleep(400);
    must((await pending()) === 0, `round ${round}: a rewrite of the flushed content was held`);
    must((await flush()).count === 0, `round ${round}: the flush released a rewrite that changed nothing`);
    must(!(await reloaded(1500)), `round ${round}: a rewrite that changed nothing reloaded the page`);
    await sleep(800); // past the window an empty flush opens

    // 3. An edit reverted to the flushed bytes before the flush: its hold drops.
    frames.length = 0;
    fs.writeFileSync(file, content(`b${round}`));
    await sleep(400);
    must((await pending()) === 1, `round ${round}: the edit before the revert was not held`);
    fs.writeFileSync(file, content(a));
    await sleep(400);
    must((await pending()) === 0, `round ${round}: the reverted edit stayed held`);
    must((await flush()).count === 0, `round ${round}: the flush released a reverted edit`);
    must(!(await reloaded(1500)), `round ${round}: a reverted edit reloaded the page`);
    const body = await (await fetch(`${base}/src/color.js?t=${Date.now()}`)).text();
    must(body.includes(`"${a}"`), `round ${round}: the server did not serve the reverted file:\n${body}`);
    await sleep(800);

    // 4. A real change after all that is still held and released.
    frames.length = 0;
    fs.writeFileSync(file, content(`c${round}`));
    await sleep(400);
    must((await pending()) === 1, `round ${round}: a real change after a no-op was not held`);
    must((await flush()).count === 1, `round ${round}: the real change was not released`);
    must(await reloaded(3000), `round ${round}: no reload for the real change`);
    await sleep(800);

    // 5. A change that reaches the page without a flush (it lands right
    //    after a flush that released nothing) moves what the page has: a
    //    later write back to the flushed bytes is a change, not a no-op.
    frames.length = 0;
    const racing = flush();
    await sleep(5);
    fs.writeFileSync(file, content(`d${round}`));
    must((await racing).count === 0, `round ${round}: the racing flush released something`);
    must(await reloaded(3000), `round ${round}: the write after an empty flush did not reach the page`);
    await sleep(800);
    frames.length = 0;
    fs.writeFileSync(file, content(`c${round}`));
    await sleep(400);
    must(
      (await pending()) === 1,
      `round ${round}: a write back to the flushed bytes was dropped while the page shows newer content`,
    );
    must((await flush()).count === 1, `round ${round}: the write back was not released`);
    must(await reloaded(3000), `round ${round}: the write back never reached the page`);
    await sleep(800);
  }
  console.log(
    "ok: rewrites of what the page has are neither held nor reloaded; real changes, and writes back to older bytes, still are",
  );
} catch (e) {
  failed = true;
  console.error(String(e?.stack ?? e));
  console.error(`--- oj log (tail)\n${log.slice(-3000)}`);
} finally {
  ws?.close();
  srv.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
