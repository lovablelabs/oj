// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies oj does NOT keep a plugin-host Node process resident when the config
// has no plugins oj needs (e.g. React-only, where oj does JSX in oxc). The host
// may spawn briefly to report zero active plugins, then must be killed. Big
// memory + dev-loop win for the common React-only case.

import { spawn } from "node:child_process";
import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5323;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-pluginskip-"));

let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"pskip","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "main.js"), 'document.body.dataset.ok = "1";\n');
  // A vite.config with zero plugins -> plugin host has nothing to do -> dropped.
  fs.writeFileSync(path.join(app, "vite.config.mjs"), "export default { plugins: [] };\n");

  let out = "";
  child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app });
  child.stdout.on("data", (d) => (out += d.toString()));
  child.stderr.on("data", (d) => (out += d.toString()));

  const up = async () => {
    return waitUp(`http://localhost:${PORT}/`).then(() => true, () => false);
  };
  if (!(await up())) throw new Error("server did not start:\n" + out);
  await new Promise((r) => setTimeout(r, 1500)); // let the host spawn+die settle

  if (!/served natively|none active/i.test(out)) throw new Error("expected the 'served natively' skip log:\n" + out);
  console.log("skip log:           yes");

  // No plugin-host.mjs process should be resident for this app.
  let resident = "";
  try { resident = execSync(`pgrep -fl "plugin-host.mjs" || true`, { encoding: "utf8" }); } catch {}
  if (resident.includes(app) || new RegExp(path.basename(app)).test(resident)) {
    throw new Error("plugin-host still resident for the app:\n" + resident);
  }
  console.log("plugin-host gone:   yes");

  // App still serves + is transformed natively.
  const home = await (await fetch(`http://localhost:${PORT}/`)).text();
  if (!home.includes("main.js")) throw new Error("app not served");
  console.log("served natively:    yes");
  console.log("\nPLUGIN-HOST SKIP VERIFIED: no resident host for a no-op plugin config");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
