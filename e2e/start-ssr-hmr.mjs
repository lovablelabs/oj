// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Start dev SSR renders an edit on the next request, as Vite's does: its
// module runner evaluates the framework's `noExternal` packages itself and
// clears them with the app on every SSR reload (hmrHandler full-reload). The
// framework's request handler caches the app's router entry in module state
// (start-server-core `entriesPromise`), so a host that loads those packages
// once keeps rendering the first router: the browser reloads into a fresh
// client bundle and hydrates against stale server HTML. Each edit here must
// reach the server render and the client bundle:
// - the route module itself;
// - a module the route imports;
// - a CSS module (a new class in its map);
// and the re-evaluated handler keeps serving the rest of the app.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5400);

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start ssr hmr: fixture deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const rm = (p) => {
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(p, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) throw e;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};
const filesUnder = (dir, ext) => {
  if (!fs.existsSync(dir)) return [];
  return fs
    .readdirSync(dir, { withFileTypes: true, recursive: true })
    .filter((e) => e.isFile() && e.name.endsWith(ext))
    .map((e) => fs.readFileSync(path.join(e.parentPath ?? e.path, e.name), "utf8"));
};
const clientCarries = (text) => filesUnder(path.join(app, ".oj-cache"), ".js").some((c) => c.includes(text));
const marker = (html, mark) => new RegExp(`data-mark="${mark}"[^>]*>([^<]*)<`).exec(html)?.[1] ?? "";
const classOf = (html, mark) => new RegExp(`<p class="([^"]*)" data-mark="${mark}">`).exec(html)?.[1] ?? "";

const routes = path.join(app, "src", "routes");
const aboutFile = path.join(routes, "about.tsx");
const originalAbout = fs.readFileSync(aboutFile, "utf8");
const labelFile = path.join(routes, "hmr-label.ts");
const moduleFile = path.join(routes, "hmr.module.css");
const about = (version) => `import { createRoute } from "@tanstack/react-router";

import hmr from "./hmr.module.css";
import { label } from "./hmr-label";
import { rootRoute } from "./__root";

export const aboutRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/about",
  component: About,
});

function About() {
  return (
    <main>
      <h1 className="fixture-heading">about-page-marker</h1>
      <p data-mark="route">route-${version}</p>
      <p data-mark="label">{label}</p>
      <p className={hmr["hmr-extra"] ?? "missing-extra"} data-mark="extra">css-hmr-extra</p>
    </main>
  );
}
`;
const files = {
  [labelFile]: `export const label = "label-v1";\n`,
  [moduleFile]: `.hmr { color: red; }\n`,
  [aboutFile]: about("v1"),
};

let srv;
let log = "";
const reloads = () => log.split("reloading").length;
const page = async () => (await fetch(`http://localhost:${PORT}/about`)).text();
// Edits `file`, waits for the dev server's reload, then for the server
// render to satisfy `rendered`; returns that page.
async function edit(file, source, what, rendered) {
  const before = reloads();
  fs.writeFileSync(file, source);
  await settles(async () => reloads() > before, { timeoutMs: 30000 });
  must(reloads() > before, `${what} did not reload the page; log tail:\n${log.slice(-2500)}`);
  let html = "";
  await settles(
    async () => {
      html = await page();
      return rendered(html);
    },
    { timeoutMs: 15000 },
  );
  must(rendered(html), `${what}: the server render is stale:\n${html.slice(0, 1500)}\n---\n${log.slice(-1500)}`);
  return html;
}

try {
  for (const [file, source] of Object.entries(files)) fs.writeFileSync(file, source);
  rm(path.join(app, ".oj-cache"));
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);
  let html = "";
  await settles(async () => {
    try {
      html = await page();
      return html.includes("route-v1");
    } catch {
      return false;
    }
  });
  must(
    marker(html, "route") === "route-v1" && marker(html, "label") === "label-v1",
    `boot render:\n${html.slice(0, 1500)}`,
  );

  await edit(aboutFile, about("v2"), "a route edit", (h) => marker(h, "route") === "route-v2");
  must(clientCarries("route-v2"), "the rebundled client does not carry the route edit");
  console.log("start dev: route edit rendered by SSR");

  await edit(
    labelFile,
    `export const label = "label-v2";\n`,
    "an imported module edit",
    (h) => marker(h, "label") === "label-v2",
  );
  must(clientCarries("label-v2"), "the rebundled client does not carry the imported module edit");
  console.log("start dev: imported module edit rendered by SSR");

  html = await edit(moduleFile, `.hmr { color: red; }\n.hmr-extra { color: blue; }\n`, "a CSS module edit", (h) =>
    classOf(h, "extra").includes("hmr-extra"),
  );
  const extra = classOf(html, "extra");
  must(clientCarries(JSON.stringify(extra).slice(1, -1)), `the rebundled client does not carry "${extra}"`);
  console.log(`start dev: CSS module edit rendered by SSR as ${extra}`);

  // A second round on the same files: invalidation keeps working after the
  // first re-evaluation.
  await edit(aboutFile, about("v3"), "a second route edit", (h) => marker(h, "route") === "route-v3");
  console.log("start dev: a second route edit rendered by SSR");
  // The re-evaluated request handler still serves the rest of the app: the
  // index route renders through a server function during SSR.
  const index = await (await fetch(`http://localhost:${PORT}/`)).text();
  must(index.includes("server-fn-marker"), `/ no longer renders its server function:\n${index.slice(0, 1200)}`);
  console.log("start dev: server functions still render after the edits");
  console.log("\nSTART SSR HMR PASSED");
} catch (e) {
  console.error("\nSTART SSR HMR FAILED:", e.message);
  process.exitCode = 1;
} finally {
  for (const file of Object.keys(files)) if (file !== aboutFile) rm(file);
  fs.writeFileSync(aboutFile, originalAbout);
  srv?.kill("SIGKILL");
}
