// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// `oj build` on a vite 8 app loads the app's native rolldown binding once in
// the config-extraction engine and once in the build engine. When both ran in
// one process, the second registration ran against the state the addon kept
// from its first, torn-down engine; a napi-rs 3.9-era binding (rolldown
// 1.0.3, the exact pin of vite 8.0.16) corrupts itself then and SIGSEGVs the
// whole build with no output — Node dies the same way when a second worker_thread
// requires such an addon after the first worker exited. Guards the fix:
// one-shot engine jobs run in `oj engine-job` child processes, so each
// registration owns its process. Run with a built target/debug/oj; installs
// vite 8.0.16 into a temp app (skips when offline).
import { execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-napi-rereg-"));
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "napi-rereg", version: "1.0.0", type: "module" }),
);
try {
  execSync("npm install vite@8.0.16 --no-audit --no-fund --loglevel=error", { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP native-addon-reregister: could not install vite@8.0.16 (offline?)");
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}
const rolldown = JSON.parse(
  fs.readFileSync(path.join(app, "node_modules", "rolldown", "package.json"), "utf8"),
);
if (rolldown.version !== "1.0.3") {
  // vite 8.0.16 pins rolldown 1.0.3 exactly, a napi-rs 3.9-era binding that
  // dies on re-registration (so does 1.1.0; 1.1.5 survives). A different
  // resolution would make a pass here prove nothing.
  console.log(`SKIP native-addon-reregister: vite resolved rolldown ${rolldown.version}, not 1.0.3`);
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}

fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><body><script type="module" src="/src/main.ts"></script></body></html>\n`,
);
fs.writeFileSync(path.join(app, "src", "main.ts"), `console.log("hi");\n`);
// Any vite.config makes extraction import the vite package, which registers
// the rolldown binding into the extraction engine.
fs.writeFileSync(path.join(app, "vite.config.ts"), `export default { plugins: [] };\n`);

const out = spawnSync(oj, ["build"], { cwd: app, encoding: "utf8", timeout: 180_000 });
if (out.status !== 0) {
  console.error(out.stdout ?? "");
  console.error(out.stderr ?? "");
  throw new Error(
    `oj build died re-registering the rolldown binding: status ${out.status}, signal ${out.signal}`,
  );
}
if (!fs.existsSync(path.join(app, "dist", "index.html"))) {
  throw new Error("oj build reported success but wrote no dist/index.html");
}

fs.rmSync(app, { recursive: true, force: true });
console.log("native-addon-reregister: ok");
