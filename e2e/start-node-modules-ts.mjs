// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A dependency that ships raw TypeScript (an exports map pointing straight at
// .ts/.tsx sources) must render under Start dev. The runtime refuses to
// type-strip under node_modules (ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING),
// and Vite never externalizes a resolved file it cannot hand to Node
// (canExternalizeFile allows only js/mjs/cjs), so such files go through the
// transform pipeline instead — including the package's own extensionless
// relative imports and .tsx files.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { writeTsSourceDep } from "./lib/ts-source-dep.mjs";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5396);

const installed = fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start node_modules ts: fixture deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
// Retried, as in start.mjs: SIGKILLed servers orphan node children for a beat,
// and their NODE_COMPILE_CACHE flush into .oj-cache/v8 races the removal
// (ENOTEMPTY).
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

const aboutFile = path.join(app, "src", "routes", "about.tsx");
const originalAbout = fs.readFileSync(aboutFile, "utf8");
const patchedAbout = `import { createRoute } from "@tanstack/react-router";

import { BadgeIcon, Tone, describeBadge } from "ts-source-dep/badge";

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
      <p>{describeBadge({ label: "e2e", variant: "outline" })}</p>
      <p>{\`tone:\${Tone.Neutral}\`}</p>
      <BadgeIcon name="star" />
    </main>
  );
}
`;

// The fixture mutations live inside the try so an early throw still restores
// them: the finally below is safe to run against any partial state.
const dep = path.join(app, "node_modules", "ts-source-dep");
let srv;
let log = "";
try {
  writeTsSourceDep(path.join(app, "node_modules"));
  fs.writeFileSync(aboutFile, patchedAbout);
  rm(path.join(app, ".oj-cache"));

  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));

  await waitUp(`http://localhost:${PORT}/`);
  // The SSR render settles once the dep's transformed modules are all loaded.
  let about = { status: 0, body: "" };
  await settles(async () => {
    try {
      const res = await fetch(`http://localhost:${PORT}/about`);
      about = { status: res.status, body: await res.text() };
      return about.status === 200 && about.body.includes("badge-outline");
    } catch {
      return false;
    }
  });
  must(about.status === 200, `/about returned ${about.status}:\n${about.body.slice(0, 800)}\n---\n${log.slice(-1200)}`);
  for (const marker of ["badge-outline:bold:nodenext:e2e", "tone:neutral", "ts-dep-icon:star"]) {
    must(about.body.includes(marker), `SSR html missing "${marker}":\n${about.body.slice(0, 800)}`);
  }
  must(
    !log.includes("TYPE_STRIPPING") && !log.includes("ERR_MODULE_NOT_FOUND"),
    `server log shows a native dependency load failure:\n${log.slice(-1500)}`,
  );
  console.log("start dev: raw-TS dependency rendered through the pipeline");
  srv.kill("SIGKILL");

  // Production build: Vite resolves before externalizing (canExternalizeFile),
  // so the raw-TS dependency must be bundled into the server bundle, never a
  // bare import plain Node would have to type-strip.
  const dist = path.join(app, "dist");
  rm(dist);
  rm(path.join(app, ".oj-cache"));
  execSync(`${oj} build ${app}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });
  const bundle = fs.readFileSync(path.join(dist, "server-bundle.mjs"), "utf8");
  must(
    !/from\s*["']ts-source-dep(?:\/[^"']*)?["']/.test(bundle),
    "built server bundle still imports the raw-TS dependency bare",
  );
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
    const html = await res.text();
    must(
      res.status === 200,
      `built /about returned ${res.status}:\n${html.slice(0, 800)}\n---\n${builtLog.slice(-1200)}`,
    );
    for (const marker of ["badge-outline:bold:nodenext:e2e", "tone:neutral", "ts-dep-icon:star"]) {
      must(html.includes(marker), `built SSR html missing "${marker}":\n${html.slice(0, 800)}`);
    }
    must(
      !builtLog.includes("TYPE_STRIPPING") && !builtLog.includes("ERR_MODULE_NOT_FOUND"),
      `built server log shows a native dependency load failure:\n${builtLog.slice(-1500)}`,
    );
  } finally {
    built.kill("SIGKILL");
    rm(dist);
  }
  console.log("\nSTART NODE_MODULES TS PASSED");
} catch (e) {
  console.error("\nSTART NODE_MODULES TS FAILED:", e.message);
  process.exitCode = 1;
} finally {
  fs.writeFileSync(aboutFile, originalAbout);
  rm(dep);
  srv?.kill("SIGKILL");
}
