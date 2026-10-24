// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start dev, a server function importing an alias (`@app/lib/...`,
// tsconfig `paths`) of a file that does not exist yet: the first call fails,
// the file is created, and the next call must find it without the importer
// being touched.
//
// The rule (Vite's resolver): `tryFsResolve` stats the disk on every resolve
// and keeps no cache of misses, so a created file resolves at once, whatever
// HMR does. oj's resolvers cache lookups, misses included, and only HMR's
// `decide` dropped them: with `server.hmr: false` (and while an editor gate
// held updates, or flushed one as a full reload) the alias stayed a bare
// package to the SSR runtime ("Could not find package '@app/lib'") until the
// importer was edited again. Runs under `server.hmr: false` and the default.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 3114);

const installed = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start new file alias: fixture deps not installed");
  process.exit(0);
}
let pw = null;
try {
  pw = await import("playwright");
} catch {}
if (!pw) {
  console.log("SKIP start new file alias: playwright not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// A fixture copy whose page calls `lab()`, a server fn that imports the
// helper inside its handler (so only the server resolves it: the client
// stub drops the handler body).
function makeApp(hmr) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-new-file-"));
  fs.cpSync(fixture, app, {
    recursive: true,
    filter: (src) => !/[\\/](node_modules|\.oj-cache)$/.test(src),
  });
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  const write = (rel, text) => {
    fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
    fs.writeFileSync(path.join(app, rel), text);
  };
  if (!hmr) {
    const cfg = path.join(app, "vite.config.ts");
    const src = fs.readFileSync(cfg, "utf8");
    must(src.includes("defineConfig({"), "fixture vite.config.ts changed shape");
    fs.writeFileSync(cfg, src.replace("defineConfig({", "defineConfig({\n  server: { hmr: false },"));
  }
  write(
    "src/server/lab.functions.ts",
    [
      'import { createServerFn } from "@tanstack/react-start";',
      "",
      'export const lab = createServerFn({ method: "GET" }).handler(async () => {',
      '  const { helper } = await import("@app/lib/auth2/helpers");',
      "  return helper();",
      "});",
      "",
    ].join("\n"),
  );
  const aboutPath = path.join(app, "src/routes/about.tsx");
  let about = fs.readFileSync(aboutPath, "utf8");
  about = about.replace(
    'import { rootRoute } from "./__root";',
    'import { rootRoute } from "./__root";\nimport { useEffect, useState } from "react";\nimport { lab } from "../server/lab.functions";\n' +
      "function Lab() {\n" +
      '  const [out, setOut] = useState("pending");\n' +
      "  useEffect(() => {\n" +
      '    lab().then((r) => setOut(String(r)), (e) => setOut("error " + String(e)));\n' +
      "  }, []);\n" +
      '  return <p id="lab">{out}</p>;\n' +
      "}",
  );
  about = about.replace(
    '<h1 className="fixture-heading">about-page-marker</h1>',
    '<h1 className="fixture-heading">about-page-marker</h1>\n      <Lab />',
  );
  fs.writeFileSync(aboutPath, about);
  return app;
}

// What `#lab` settles on for a fresh load of /about.
async function labAnswer(browser) {
  const page = await browser.newPage();
  try {
    await page.goto(`http://localhost:${PORT}/about`);
    await page.waitForFunction(() => document.querySelector("#lab")?.textContent !== "pending", null, {
      timeout: 20000,
    });
    return await page.locator("#lab").textContent();
  } finally {
    await page.close();
  }
}

async function run(hmr) {
  const label = hmr ? "start-dev (hmr)" : "start-dev (server.hmr: false)";
  const app = makeApp(hmr);
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "ignore", "pipe"], detached: true });
  let stderr = "";
  srv.stderr.on("data", (d) => (stderr += d));
  const exited = new Promise((r) => srv.once("exit", r));
  const browser = await pw.chromium.launch();
  try {
    const up = await waitUp(`http://localhost:${PORT}/`, { until: () => true }).then(
      () => true,
      () => false,
    );
    must(up, `${label}: server on :${PORT} did not start\n${stderr.slice(-2000)}`);

    // The helper does not exist: the call fails (and the miss is cached).
    const before = await labAnswer(browser);
    must(before.startsWith("error"), `${label}: the call succeeded before the helper existed: ${before}`);

    // Created in a directory that did not exist either; the importer is
    // never touched again.
    fs.mkdirSync(path.join(app, "src/lib/auth2"), { recursive: true });
    fs.writeFileSync(path.join(app, "src/lib/auth2/helpers.ts"), 'export const helper = () => "helper-ok";\n');

    let after = null;
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      after = await labAnswer(browser);
      if (after === "helper-ok") break;
      await sleep(500);
    }
    must(
      after === "helper-ok",
      `${label}: the created helper never resolved: ${JSON.stringify(after)} (before: ${before})\n${stderr.slice(-2000)}`,
    );
    console.log(`${label}: an alias import of a created file resolves without touching its importer`);
  } finally {
    await browser.close();
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
    await exited;
    fs.rmSync(app, { recursive: true, force: true });
  }
}

try {
  await run(false);
  await run(true);
  console.log("\nSTART NEW FILE ALIAS PASSED");
} catch (e) {
  console.error(`\nSTART NEW FILE ALIAS FAILED: ${e.message}`);
  process.exitCode = 1;
}
