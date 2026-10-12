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
const PORT = Number(process.env.OJ_E2E_PORT || 5394);

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

const dep = writeTsSourceDep(path.join(app, "node_modules"));
fs.writeFileSync(aboutFile, patchedAbout);
rm(path.join(app, ".oj-cache"));

const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
let log = "";
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));

try {
  await waitUp(`http://localhost:${PORT}/`);
  // The SSR render settles once the dep's transformed modules are all loaded.
  let about = { status: 0, body: "" };
  await settles(async () => {
    const res = await fetch(`http://localhost:${PORT}/about`);
    about = { status: res.status, body: await res.text() };
    return about.status === 200 && about.body.includes("badge-outline");
  });
  must(about.status === 200, `/about returned ${about.status}:\n${about.body.slice(0, 800)}\n---\n${log.slice(-1200)}`);
  for (const marker of ["badge-outline:bold:e2e", "tone:neutral", "ts-dep-icon:star"]) {
    must(about.body.includes(marker), `SSR html missing "${marker}":\n${about.body.slice(0, 800)}`);
  }
  must(
    !log.includes("TYPE_STRIPPING") && !log.includes("ERR_MODULE_NOT_FOUND"),
    `server log shows a native dependency load failure:\n${log.slice(-1500)}`,
  );
  console.log("\nSTART NODE_MODULES TS PASSED");
} catch (e) {
  console.error("\nSTART NODE_MODULES TS FAILED:", e.message);
  process.exitCode = 1;
} finally {
  fs.writeFileSync(aboutFile, originalAbout);
  rm(dep);
  srv.kill("SIGKILL");
}
