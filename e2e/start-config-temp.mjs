// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start dev loads vite.config at boot and again in every client
// rebundle. Like Node's Vite, no load may write its bundled config
// (`vite.config.ts.timestamp-<ms>-<hash>.mjs`) into the app root, where
// formatters and linters walking the tree race its deletion.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 3099);
const ENTRY = `http://localhost:${PORT}/@oj-start/client-entry.js`;

const installed = fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start config temp: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

// Every name created in the app root while the server runs. The temp lives
// for one import, so only a watcher sees it; the probe below proves this
// watcher reports a create-then-delete.
const seen = new Set();
const watcher = fs.watch(app, (_, name) => name && seen.add(String(name)));
const probe = path.join(app, "oj-e2e-watch-probe.tmp");
fs.writeFileSync(probe, "");
fs.rmSync(probe);
must(await settles(() => seen.has(path.basename(probe)), { pollMs: 50 }), "the root watcher missed a short-lived file");

const about = path.join(app, "src", "routes", "about.tsx");
const original = fs.readFileSync(about, "utf8");
fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const first = await fetch(ENTRY);
  must(first.status === 200, `entry returned ${first.status}`);
  const etag = first.headers.get("etag");

  const edit = () => fs.writeFileSync(about, original.replace("about-page-marker", "about-page-config-temp"));
  edit();
  // Only a served entry with a new ETag proves a rebundle (and its config
  // load) ran, so an error response cannot pass this vacuously.
  const rebundled = await settles(
    async () => {
      const r = await fetch(ENTRY, { headers: { "if-none-match": etag } });
      const next = r.headers.get("etag");
      return (r.status === 200 || r.status === 304) && next !== null && next !== etag;
    },
    { pollMs: 200, touch: edit },
  );
  must(rebundled, "the entry ETag did not change after an edit rebundled it");

  const temps = [...seen].filter((n) => n.includes(".timestamp-"));
  must(temps.length === 0, `config bundles written into the app root: ${temps.join(", ")}`);
  console.log("start-dev: boot and rebundle config loads leave the app root untouched");
} finally {
  watcher.close();
  fs.writeFileSync(about, original);
  srv.kill("SIGKILL");
  await sleep(300);
}
