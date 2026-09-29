// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies oj substitutes %VITE_*% / %MODE% etc. in index.html (Vite's
// htmlEnvHook) in BOTH the dev server and the production build. Known keys are
// replaced with their env value; unknown placeholders are left untouched.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5332;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-htmlenv-"));

const INDEX =
  "<!doctype html><html><head><title>%VITE_APP_TITLE%</title>" +
  '<meta name="mode" content="%MODE%"><meta name="missing" content="%VITE_MISSING%">' +
  '</head><body><script type="module" src="/src/main.js"></script></body></html>';

let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"htmlenv","private":true}');
  fs.writeFileSync(path.join(app, ".env"), "VITE_APP_TITLE=Hello OJ\n");
  fs.writeFileSync(path.join(app, "index.html"), INDEX);
  fs.writeFileSync(path.join(app, "src", "main.js"), "export const v = 1;\n");

  const check = (html, label, mode) => {
    if (!html.includes("<title>Hello OJ</title>"))
      throw new Error(`${label}: %VITE_APP_TITLE% not substituted:\n${html}`);
    if (!html.includes(`content="${mode}"`)) throw new Error(`${label}: %MODE% not substituted to ${mode}:\n${html}`);
    if (!html.includes('content="%VITE_MISSING%"'))
      throw new Error(`${label}: unknown %VITE_MISSING% should be left as-is:\n${html}`);
    console.log(`${label}: %VITE_APP_TITLE% + %MODE%=${mode} substituted, unknown left`);
  };

  // dev
  child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app, stdio: "ignore" });
  const up = async () => {
    return waitUp(`http://localhost:${PORT}/`).then(
      () => true,
      () => false,
    );
  };
  if (!(await up())) throw new Error("dev server did not start");
  check(await (await fetch(`http://localhost:${PORT}/`)).text(), "dev", "development");
  child.kill("SIGKILL");
  child = null;

  // build
  execSync(`${OJ} build ${app}`, { cwd: app, stdio: ["ignore", "ignore", "inherit"] });
  check(fs.readFileSync(path.join(app, "dist", "index.html"), "utf8"), "build", "production");

  console.log("\nHTML ENV SUBSTITUTION VERIFIED (dev + build)");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
