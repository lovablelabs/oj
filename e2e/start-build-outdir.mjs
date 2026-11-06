// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A TanStack Start build empties its outDir by Vite's emptyOutDir rule, as a
// plain `oj build` does: an outDir outside the project root is left alone
// (Vite warns and writes over it) unless `--emptyOutDir` asks, and a `.git`
// inside an emptied outDir is kept. Deleting an outside outDir outright
// destroys whatever else lives there.

import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start-build-outdir: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-outdir-"));
const out = path.join(tmp, "out");
const marker = path.join(out, "keep-me.txt");
const build = (...flags) =>
  execSync(`"${oj}" build "${app}" --out "${out}" ${flags.join(" ")}`, { stdio: ["ignore", "ignore", "inherit"] });

let failed = false;
try {
  fs.mkdirSync(path.join(out, ".git"), { recursive: true });
  fs.writeFileSync(marker, "not oj's to delete\n");
  build();
  must(fs.existsSync(marker), "an outDir outside the root was emptied without --emptyOutDir");
  must(fs.existsSync(path.join(out, "server-bundle.mjs")), "the build did not write the outDir");

  build("--emptyOutDir");
  must(!fs.existsSync(marker), "--emptyOutDir did not empty the outside outDir");
  must(fs.existsSync(path.join(out, ".git")), "emptying the outDir removed its .git");
  must(fs.existsSync(path.join(out, "server-bundle.mjs")), "the --emptyOutDir build did not write the outDir");
  console.log("START-BUILD-OUTDIR E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-BUILD-OUTDIR E2E FAILED:", err && err.stack ? err.stack : err);
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
