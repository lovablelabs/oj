// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Edits in a TanStack Start app whose SSR runs in workerd
// (`cloudflare({ viteEnvironment: { name: "ssr" } })`), in a real browser.
//
// The rule (Vite): the worker renders the document from TanStack's own
// manifest, whose script is the unbundled dev client entry
// (`/@id/virtual:tanstack-start-dev-client-entry`). The page runs the client
// module graph, and an edit reaches it as an HMR update of the client
// environment: the page keeps its state, nothing bundles the client, and the
// worker environment's own update never reloads the page. A file that came
// and went inside the batch (GNU sed's temp, an editor's safe-write) is in no
// module graph, so it changes none of that.
//
// oj rebuilt its own Start client bundle on every edit, which no page loads
// here, and then reloaded the page, throwing the hot update away; it also
// built that bundle at boot. It is now built only for a page that loads it:
// one oj's own runner renders, as when the config names the plugin but no
// worker environment serves the documents.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseBoundPort } from "./lib/hydration.mjs";
import { CLOUDFLARE_DEPS, settles, sleep } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
let PORT = Number(process.env.OJ_E2E_PORT || 6870);

let chromium = null;
try {
  chromium = (await import("playwright")).chromium;
} catch {}
const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed || !chromium) {
  console.log("SKIP start cloudflare hmr: fixture deps or playwright not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-cf-hmr-"));
const app = path.join(tmp, "app");
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

function installCloudflareDeps() {
  const prepared = process.env.OJ_E2E_CF_DEPS;
  if (prepared) {
    fs.symlinkSync(path.join(path.resolve(prepared), "node_modules"), path.join(tmp, "node_modules"), "dir");
    return;
  }
  fs.writeFileSync(
    path.join(tmp, "package.json"),
    JSON.stringify({ name: "oj-cf-deps", private: true, type: "module" }),
  );
  execSync(`npm install --no-audit --no-fund --no-package-lock ${CLOUDFLARE_DEPS}`, {
    cwd: tmp,
    stdio: "inherit",
  });
}

const BANNER = path.join("src", "modules", "dashboard", "components", "Banner.tsx");

function makeApp() {
  fs.mkdirSync(app);
  for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json"]) {
    fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
  }
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  fs.writeFileSync(
    path.join(app, "wrangler.jsonc"),
    JSON.stringify({
      name: "oj-start-cf-hmr",
      main: "@tanstack/react-start/server-entry",
      compatibility_date: "2025-09-01",
      compatibility_flags: ["nodejs_compat"],
      vars: { EDITION: "fixture-edition" },
    }),
  );
  const original = fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8");
  const config = original
    .replace(
      'import { defineConfig } from "vite";',
      'import { defineConfig } from "vite";\nimport { cloudflare } from "@cloudflare/vite-plugin";',
    )
    .replace(/^(\s*)tanstackStart\(/m, '$1cloudflare({ viteEnvironment: { name: "ssr" } }),\n$1tanstackStart(');
  must(config.includes("cloudflare({"), "fixture vite.config.ts changed shape; update this script");
  fs.writeFileSync(path.join(app, "vite.config.ts"), config);

  // A feature-module layout: a memo component under modules/, and message
  // catalogs the page reads through an import.meta.glob over modules/.
  const mod = path.join(app, "src", "modules", "dashboard");
  fs.mkdirSync(path.join(mod, "components"), { recursive: true });
  fs.mkdirSync(path.join(mod, "messages"), { recursive: true });
  fs.writeFileSync(path.join(mod, "messages", "en.json"), JSON.stringify({ title: "catalog-ok" }));
  fs.writeFileSync(
    path.join(app, BANNER),
    [
      'import { memo } from "react";',
      "",
      "export const Banner = memo(function Banner() {",
      '  return <p className="banner banner-before" data-testid="banner">banner-before</p>;',
      "});",
      "",
    ].join("\n"),
  );
  fs.mkdirSync(path.join(app, "src", "server", "notes"), { recursive: true });
  fs.writeFileSync(path.join(app, "src", "server", "notes", "a.txt"), "note-a");
  // A glob only the worker reads: the handler is its one importer, and the
  // client build drops the handler, so no client module re-expands it.
  fs.writeFileSync(
    path.join(app, "src", "server", "notes.ts"),
    'export const notes = import.meta.glob<string>("./notes/*.txt", { eager: true, query: "?raw", import: "default" });\n',
  );
  fs.writeFileSync(
    path.join(app, "src", "server", "data.ts"),
    [
      'import { createServerFn } from "@tanstack/react-start";',
      'import { env } from "cloudflare:workers";',
      "",
      'import { notes } from "./notes";',
      "",
      'export const getEdition = createServerFn({ method: "GET" }).handler(async () => {',
      '  return [String((env as unknown as Record<string, unknown>).EDITION), ...Object.values(notes)].join(" ");',
      "});",
      "",
    ].join("\n"),
  );
  fs.writeFileSync(
    path.join(app, "src", "routes", "index.tsx"),
    [
      'import { createRoute, useLoaderData } from "@tanstack/react-router";',
      'import { useEffect, useState } from "react";',
      'import { rootRoute } from "./__root";',
      'import { getEdition } from "../server/data";',
      'import { Banner } from "../modules/dashboard/components/Banner";',
      "",
      'const catalogs = import.meta.glob<{ title: string }>("../modules/*/messages/*.json", { eager: true, import: "default" });',
      "",
      "export const indexRoute = createRoute({",
      "  getParentRoute: () => rootRoute,",
      '  path: "/",',
      "  loader: async () => await getEdition(),",
      "  component: Index,",
      "});",
      "",
      "function Index() {",
      "  const edition = useLoaderData({ from: indexRoute.id });",
      "  const [count, setCount] = useState(0);",
      "  const [mounted, setMounted] = useState(false);",
      "  useEffect(() => setMounted(true), []);",
      "  return (",
      "    <main>",
      '      <p data-testid="edition">{edition}</p>',
      '      <p data-testid="catalogs">{Object.values(catalogs).map((c) => c.title).join(",")}</p>',
      "      <Banner />",
      '      {mounted ? <span data-testid="mounted">mounted</span> : null}',
      '      <button data-testid="counter" onClick={() => setCount((c) => c + 1)}>count: {count}</button>',
      "    </main>",
      "  );",
      "}",
      "",
    ].join("\n"),
  );
}

// The Start watcher's verdict on a batch: its last line per run.
const VERDICT = /oj start: (hot updated|change outside[^\n]*|rebuilt[^\n]*|updated[^\n]*|client updates[^\n]*)/g;

// Boots `oj dev` on the app, waits for `/` to answer with `marker`, runs
// `phase(base, log, browser)`, and tears the server down. With
// `beforeFirstDocument`, the log must match it before any document is asked.
async function served(label, marker, phase, { beforeFirstDocument } = {}) {
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    cwd: app,
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1", NO_COLOR: "1", FORCE_COLOR: "0" },
  });
  ojGroup = srv.pid;
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  const exited = new Promise((r) => srv.once("exit", r));
  const browser = await chromium.launch();
  const pageErrors = [];
  try {
    let port = null;
    await settles(
      () => {
        port = parseBoundPort(log);
        return srv.exitCode != null || Boolean(port);
      },
      { timeoutMs: 60000, pollMs: 250 },
    );
    must(port, `${label}: oj never bound a port:\n${log.slice(-4000)}`);
    if (beforeFirstDocument) {
      must(
        await settles(() => beforeFirstDocument.test(log), { timeoutMs: 120000, pollMs: 200 }),
        `${label}: no ${beforeFirstDocument} before the first document:\n${log.slice(-4000)}`,
      );
    }
    const base = `http://127.0.0.1:${port}`;
    let up = false;
    await settles(
      async () => {
        if (srv.exitCode != null) return true;
        try {
          const res = await fetch(`${base}/`);
          up = res.status === 200 && (await res.text()).includes(marker);
        } catch {}
        return up;
      },
      { timeoutMs: 120000, pollMs: 500 },
    );
    must(up, `${label}: / never rendered:\n${log.slice(-4000)}`);
    const page = await browser.newPage();
    page.on("pageerror", (e) => pageErrors.push(e.message));
    page.on("console", (m) => m.type() === "error" && pageErrors.push(m.text()));
    await phase(base, () => log, page);
  } catch (e) {
    throw new Error(
      `${label}: ${e.message}\n--- browser errors ---\n${pageErrors.join("\n")}\n--- oj log (tail) ---\n${log.slice(-3000)}`,
    );
  } finally {
    await browser.close();
    try {
      process.kill(-srv.pid, "SIGTERM");
    } catch {}
    setTimeout(() => {
      try {
        process.kill(-srv.pid, "SIGKILL");
      } catch {}
    }, 2000).unref();
    await exited;
  }
}

// Whether a Start client bundler runs under this test's oj (its process
// group, `detached` above).
let ojGroup = null;
function bundlerRunning() {
  const rows = execSync("ps -A -o pgid=,command=", { encoding: "utf8" }).split("\n");
  return rows.some((r) => {
    const [pgid, ...cmd] = r.trim().split(/\s+/);
    return Number(pgid) === ojGroup && cmd.join(" ").includes("bundle-client");
  });
}

async function hydrate(page, base) {
  await page.goto(`${base}/`);
  await page.waitForSelector('[data-testid="mounted"]', { timeout: 60000 });
  await page.locator('[data-testid="counter"]').click();
  await page.waitForFunction(
    () => document.querySelector('[data-testid="counter"]')?.textContent === "count: 1",
    null,
    {
      timeout: 30000,
    },
  );
}

// The worker serves every document: the page runs the client module graph,
// edits apply in place, and the Start client is never bundled.
async function workerDocuments(base, log, page) {
  await hydrate(page, base);
  const scripts = await page.$$eval("script[src]", (s) => s.map((x) => x.getAttribute("src")));
  must(
    scripts.some((s) => s.includes("virtual:tanstack-start-dev-client-entry")),
    `the worker document should load TanStack's dev client entry: ${scripts.join(" ")}`,
  );
  must((await page.textContent('[data-testid="catalogs"]')) === "catalog-ok", "the glob catalogs did not render");
  const origin = await page.evaluate(() => performance.timeOrigin);

  // One edit, then one written through a sed temp under the glob's scan base.
  const banner = path.join(app, BANNER);
  for (const [from, to, withTemp] of [
    ["banner-before", "banner-first", false],
    ["banner-first", "banner-second", true],
  ]) {
    const mark = log().length;
    const verdicts = () => [...log().slice(mark).matchAll(VERDICT)].map((m) => m[0]);
    if (withTemp) {
      const temp = path.join(path.dirname(banner), "sedf1aa6r");
      fs.writeFileSync(temp, fs.readFileSync(banner, "utf8").replaceAll(from, to));
      fs.renameSync(temp, banner);
    } else {
      fs.writeFileSync(banner, fs.readFileSync(banner, "utf8").replaceAll(from, to));
    }
    await page.waitForFunction((t) => document.querySelector('[data-testid="banner"]')?.textContent === t, to, {
      timeout: 30000,
    });
    // The edit is on screen; let the Start watcher finish its run.
    await settles(() => verdicts().length > 0, { timeoutMs: 60000, pollMs: 200 });
    must(verdicts().length > 0, `no Start watcher verdict after ${to}:\n${log().slice(mark)}`);
    await sleep(1500);
    const after = log().slice(mark);
    must(!/reloading/.test(after), `${to}: the edit reloaded the page:\n${after}`);
    must(
      (await page.evaluate(() => performance.timeOrigin)) === origin,
      `${to}: the page reloaded (verdicts: ${verdicts().join(" | ")})`,
    );
    must((await page.textContent('[data-testid="counter"]')) === "count: 1", `${to}: the page lost its state`);
    console.log(`start-cloudflare-hmr: ${to} hot-updated in place (${verdicts().join(" | ")})`);
  }
  // A note renamed into place under the worker's glob (an atomic write) is
  // an add, as chokidar calls a path it did not know: the worker's
  // import-glob hotUpdate re-expands the module, and its next document
  // renders the note. Only the worker reads this glob, so nothing else
  // re-expands it.
  const staged = path.join(app, "src", ".b.txt.tmp");
  fs.writeFileSync(staged, "note-renamed");
  fs.renameSync(staged, path.join(app, "src", "server", "notes", "b.txt"));
  const rendered = async () => (await (await fetch(`${base}/`)).text()).includes("note-renamed");
  await settles(rendered, { timeoutMs: 30000, pollMs: 300 });
  must(await rendered(), `the worker's document lacks a note renamed into its glob:\n${log().slice(-3000)}`);
  console.log("start-cloudflare-hmr: a note renamed into the worker's glob reached its document");
  must(!/client bundl(ing|ed)/.test(log()), `the Start client was bundled though no page loads it:\n${log()}`);
  // Nor is the bundler process started, which holds the bundle's memory.
  must(!bundlerRunning(), "a Start client bundler process is running though no page loads its bundle");
  console.log("start-cloudflare-hmr: worker documents, the Start client never bundled");
}

// The config names the plugin but no worker environment renders: oj's runner
// serves the document, whose page loads the Start bundle. It is built once
// the serve info says so, after boot and before any document asks for it, as
// Vite's bundled dev builds at startup (`served` waits for that with no
// document requested).
async function runnerDocuments(base, log, page) {
  await hydrate(page, base);
  const scripts = await page.$$eval("script[src]", (s) => s.map((x) => x.getAttribute("src")));
  must(
    scripts.some((s) => s.includes("/@oj-start/client-entry.js")),
    `the runner document should load the Start bundle: ${scripts.join(" ")}`,
  );
  must(
    (await page.textContent('[data-testid="banner"]')) === "banner-second",
    "the runner page's bundle missed edits made while it was deferred",
  );
  console.log(
    "start-cloudflare-hmr: runner documents, the Start bundle built after boot, before any document, and hydrates",
  );
}

try {
  installCloudflareDeps();
  makeApp();
  await served("worker documents", "fixture-edition", workerDocuments);
  const cfg = path.join(app, "vite.config.ts");
  fs.writeFileSync(
    cfg,
    "// Deployed with @cloudflare/vite-plugin; dev renders in Node.\n" +
      fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8"),
  );
  fs.writeFileSync(
    path.join(app, "src", "server", "data.ts"),
    [
      'import { createServerFn } from "@tanstack/react-start";',
      "",
      'export const getEdition = createServerFn({ method: "GET" }).handler(async () => "node-edition");',
      "",
    ].join("\n"),
  );
  await served("runner documents", "node-edition", runnerDocuments, { beforeFirstDocument: /client bundl(ing|ed)/ });
  console.log("\nSTART CLOUDFLARE HMR PASSED");
} catch (e) {
  console.error(`\nSTART CLOUDFLARE HMR FAILED: ${e.message}`);
  process.exitCode = 1;
} finally {
  if (!process.env.OJ_E2E_KEEP) fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
