// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Start dev SSR never resolves a dependency through its `browser` main field,
// as Vite's server environment never does: environments don't inherit the
// top-level resolve.mainFields (getDefaultEnvironmentOptions drops mainFields
// and conditions, config.ts), and a resolved config always carries Vite's
// CLIENT defaults there (`browser` first). Resolving the server graph with
// that list picks UMD builds whose named exports are assigned at runtime,
// invisible to CJS export analysis, and SSR dies at link with "The requested
// module … does not provide an export named …" (the react-simple-maps@3
// shape: browser = UMD, module = ESM, main = lexable CJS). The client bundle
// keeps the browser field, as Vite's client environment does.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5401);

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start ssr browser field: fixture deps not installed");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const rm = (p) => fs.rmSync(p, { recursive: true, force: true });
const filesUnder = (dir, ext) => {
  if (!fs.existsSync(dir)) return [];
  return fs
    .readdirSync(dir, { withFileTypes: true, recursive: true })
    .filter((e) => e.isFile() && e.name.endsWith(ext))
    .map((e) => fs.readFileSync(path.join(e.parentPath ?? e.path, e.name), "utf8"));
};
const clientCarries = (text) => filesUnder(path.join(app, ".oj-cache"), ".js").some((c) => c.includes(text));

// The react-simple-maps@3 entry shape: no `exports` map, no `type`, three
// entries naming the same binding with a different value each, so the
// rendered text says which file actually served the import.
const dep = path.join(app, "node_modules", "browser-field-dep");
const writeDep = () => {
  fs.mkdirSync(dep, { recursive: true });
  fs.writeFileSync(
    path.join(dep, "package.json"),
    JSON.stringify({
      name: "browser-field-dep",
      version: "1.0.0",
      main: "cjs.js",
      module: "esm.js",
      browser: "umd.js",
    }),
  );
  fs.writeFileSync(path.join(dep, "cjs.js"), `exports.entryName = "from-cjs-main";\n`);
  fs.writeFileSync(path.join(dep, "esm.js"), `export const entryName = "from-esm-module";\n`);
  // Minified UMD: exports assigned to a renamed factory parameter at runtime,
  // invisible to static CJS export analysis — a named import links to nothing.
  fs.writeFileSync(
    path.join(dep, "umd.js"),
    `!function(e,t){"object"==typeof exports&&"undefined"!=typeof module?t(exports):"function"==typeof define&&define.amd?define(["exports"],t):t((e="undefined"!=typeof globalThis?globalThis:e||self).BrowserFieldDep={})}(this,(function(e){"use strict";e.entryName="from-browser-umd"}));\n`,
  );
};

const aboutFile = path.join(app, "src", "routes", "about.tsx");
const originalAbout = fs.readFileSync(aboutFile, "utf8");
const about = `import { createRoute } from "@tanstack/react-router";
import { entryName } from "browser-field-dep";

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
      <p data-mark="entry">{entryName}</p>
    </main>
  );
}
`;

let srv;
let log = "";
try {
  writeDep();
  fs.writeFileSync(aboutFile, about);
  rm(path.join(app, ".oj-cache"));
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);

  let html = "";
  await settles(async () => {
    try {
      html = await (await fetch(`http://localhost:${PORT}/about`)).text();
      return html.includes("about-page-marker");
    } catch {
      return false;
    }
  });
  const entry = /data-mark="entry">([^<]*)</.exec(html)?.[1] ?? "";
  must(
    entry === "from-esm-module",
    `SSR served "${entry}" instead of the module entry (browser field must not apply to the server):\n` +
      `${html.slice(0, 1200)}\n---\n${log.slice(-1500)}`,
  );
  console.log("start dev: SSR renders the module entry, not the browser UMD");

  // The client bundle keeps Vite's client resolution: browser field first.
  await settles(async () => clientCarries("from-browser-umd"), { timeoutMs: 15000 });
  must(
    clientCarries("from-browser-umd"),
    "the client bundle does not carry the browser entry (client resolution must keep the browser field)",
  );
  must(!clientCarries("from-esm-module"), "the client bundle carries the module entry alongside the browser one");
  console.log("start dev: client bundle resolves the browser field");
  console.log("\nSTART SSR BROWSER FIELD PASSED");
} catch (e) {
  console.error("\nSTART SSR BROWSER FIELD FAILED:", e.message);
  if (log) console.error(`--- server log tail ---\n${log.slice(-2000)}`);
  process.exitCode = 1;
} finally {
  fs.writeFileSync(aboutFile, originalAbout);
  rm(dep);
  srv?.kill("SIGKILL");
}
