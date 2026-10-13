// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// An app's own CSS module under Start: the class the server renders, the
// class map the client bundle carries and the stylesheet the browser loads
// must name the same scoped class, in dev and in the production build (Vite's
// vite:css serves every consumer from one postcss-modules compile). A client
// map that differs from the server's breaks hydration, and an unscoped sheet
// styles nothing.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5397);

const installed = fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start css modules: fixture deps not installed");
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
  const out = [];
  if (!fs.existsSync(dir)) return out;
  for (const e of fs.readdirSync(dir, { withFileTypes: true, recursive: true })) {
    if (e.isFile() && e.name.endsWith(ext)) out.push(path.join(e.parentPath ?? e.path, e.name));
  }
  return out;
};
// The class the page renders on the marked element.
const renderedClass = (html) => /<p class="([^"]*)" data-css-module="card">/.exec(html)?.[1] ?? "";
// A string literal in any quote style (a minified bundle uses backticks).
const carries = (code, cls) => ['"', "'", "`"].some((q) => code.includes(q + cls + q));
const scopedSelector = (css, cls) => new RegExp(`\\.${cls.replace(/[$]/g, "\\$&")}\\s*\\{`).test(css);

const aboutFile = path.join(app, "src", "routes", "about.tsx");
const moduleFile = path.join(app, "src", "routes", "card.module.css");
const originalAbout = fs.readFileSync(aboutFile, "utf8");
const patchedAbout = `import { createRoute } from "@tanstack/react-router";

import styles from "./card.module.css";
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
      <p className={styles.card} data-css-module="card">css-module-card</p>
    </main>
  );
}
`;

let srv;
let log = "";
try {
  fs.writeFileSync(moduleFile, `.card { color: rgb(1, 2, 3); }\n`);
  fs.writeFileSync(aboutFile, patchedAbout);
  rm(path.join(app, ".oj-cache"));

  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);
  let html = "";
  await settles(async () => {
    try {
      const res = await fetch(`http://localhost:${PORT}/about`);
      html = await res.text();
      return res.status === 200 && html.includes("css-module-card");
    } catch {
      return false;
    }
  });
  const devClass = renderedClass(html);
  must(
    /card/.test(devClass) && devClass !== "card",
    `dev SSR rendered no scoped class: "${devClass}"\n${log.slice(-1200)}`,
  );

  // The client bundle carries the same class map.
  const chunks = filesUnder(path.join(app, ".oj-cache"), ".js").map((f) => fs.readFileSync(f, "utf8"));
  must(
    chunks.some((c) => carries(c, devClass)),
    `dev client bundle does not carry "${devClass}"`,
  );
  // The stylesheet the page links scopes the same class.
  const hrefs = [...html.matchAll(/<link[^>]*rel="stylesheet"[^>]*href="([^"]+)"/g)].map((m) => m[1]);
  const sheets = await Promise.all(
    hrefs.map(async (h) => (await fetch(new URL(h, `http://localhost:${PORT}/`))).text()),
  );
  must(
    sheets.some((css) => scopedSelector(css, devClass)),
    `no dev stylesheet scopes .${devClass}: ${hrefs.join(", ")}`,
  );
  must(!sheets.some((css) => /\.card\s*\{/.test(css)), "a dev stylesheet still carries the unscoped .card");
  console.log(`start dev: css module scoped as ${devClass} on server, client and sheet`);
  srv.kill("SIGKILL");

  const dist = path.join(app, "dist");
  rm(dist);
  rm(path.join(app, ".oj-cache"));
  execSync(`${oj} build ${app}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });
  const builtJs = filesUnder(path.join(dist, "client"), ".js").map((f) => fs.readFileSync(f, "utf8"));
  const builtCss = filesUnder(path.join(dist, "client"), ".css").map((f) => fs.readFileSync(f, "utf8"));
  const built = spawn("node", [path.join(dist, "server.mjs")], {
    cwd: app,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, PORT: String(PORT) },
  });
  let builtLog = "";
  built.stdout.on("data", (d) => (builtLog += d));
  built.stderr.on("data", (d) => (builtLog += d));
  try {
    await waitUp(`http://localhost:${PORT}/about`);
    const res = await fetch(`http://localhost:${PORT}/about`);
    const page = await res.text();
    must(
      res.status === 200,
      `built /about returned ${res.status}:\n${page.slice(0, 800)}\n---\n${builtLog.slice(-1200)}`,
    );
    const buildClass = renderedClass(page);
    must(buildClass === devClass, `built SSR rendered "${buildClass}", dev rendered "${devClass}"`);
    must(
      builtJs.some((c) => carries(c, buildClass)),
      `built client bundle does not carry "${buildClass}"`,
    );
    must(
      builtCss.some((css) => scopedSelector(css, buildClass)),
      `no built stylesheet scopes .${buildClass}`,
    );
    must(!builtCss.some((css) => /\.card\s*\{/.test(css)), "a built stylesheet still carries the unscoped .card");
  } finally {
    built.kill("SIGKILL");
    rm(dist);
  }
  console.log("\nSTART CSS MODULES PASSED");
} catch (e) {
  console.error("\nSTART CSS MODULES FAILED:", e.message);
  process.exitCode = 1;
} finally {
  fs.writeFileSync(aboutFile, originalAbout);
  rm(moduleFile);
  srv?.kill("SIGKILL");
}
