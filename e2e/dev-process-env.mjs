// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies oj defines process.env.NODE_ENV in the dev server (Vite parity:
// nodeEnv = NODE_ENV || mode). Without it, library code reading
// process.env.NODE_ENV throws a ReferenceError in dev while working in build.
// The served module must have process.env.NODE_ENV replaced with "development".

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5330;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-nodeenv-"));

let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"nodeenv","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "main.js"), "export const mode = process.env.NODE_ENV;\n");

  child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app, stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`, { proc: child });

  const mod = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  if (/process\.env\.NODE_ENV/.test(mod)) {
    throw new Error(`process.env.NODE_ENV left raw (not defined in dev):\n${mod}`);
  }
  if (!mod.includes('"development"')) {
    throw new Error(`process.env.NODE_ENV not replaced with "development":\n${mod}`);
  }
  console.log('process.env.NODE_ENV -> "development" in dev: ok');
  console.log("\nDEV process.env.NODE_ENV VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
