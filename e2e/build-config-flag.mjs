// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `oj build --config <file>` uses that config (Vite's --config), and a `mode`
// named by the config file is the default mode when the CLI gives none
// (Vite: inlineConfig.mode || config.mode || "production"), while `--mode`
// still wins. Also: values only Vite knows (envPrefix, resolve.extensions,
// css.preprocessorOptions additionalData) now reach oj from vite.config.

import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5502;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cfgflag-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.mkdirSync(path.join(app, "configs"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "cfgflag", version: "1.0.0" }));
fs.writeFileSync(path.join(app, ".env"), "APP_TOKEN=from-env\nVITE_SEEN=vite\n");
fs.writeFileSync(path.join(app, ".env.staging"), "APP_TOKEN=staging-token\n");
fs.writeFileSync(path.join(app, "src", "styles.scss"), `.box { color: $brand; }\n`);
fs.writeFileSync(path.join(app, "src", "util.custom.js"), `export const U = "custom-ext";\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import { U } from "./util";\nwindow.__MODE = import.meta.env.MODE; window.__TOKEN = import.meta.env.APP_TOKEN; window.__U = U;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
// The root has a decoy config that must NOT be used when --config points elsewhere.
fs.writeFileSync(path.join(app, "vite.config.js"), `export default { build: { outDir: "decoy-out" } };\n`);
fs.writeFileSync(
  path.join(app, "configs", "vite.alt.js"),
  `export default ({ mode }) => ({
  mode: "staging",
  envPrefix: ["VITE_", "APP_"],
  resolve: { extensions: [".custom.js", ".js"] },
  css: { preprocessorOptions: { scss: { additionalData: "$brand: rgb(1, 2, 3);" } } },
  build: { outDir: "alt-out-" + mode },
});\n`,
);

const assets = (dir) => {
  const d = path.join(app, dir, "assets");
  return fs
    .readdirSync(d)
    .map((f) => fs.readFileSync(path.join(d, f), "utf8"))
    .join("\n");
};

let failed = false;
let srv = null;
try {
  execSync(`${oj} build ${app} --config configs/vite.alt.js`, { stdio: "ignore" });
  assert.ok(!fs.existsSync(path.join(app, "decoy-out")), "--config must not use the root vite.config");
  assert.ok(
    fs.existsSync(path.join(app, "alt-out-staging")),
    "config-file `mode` is the default mode and the function config saw it",
  );
  const js = assets("alt-out-staging");
  assert.match(js, /[`"]staging[`"]/, "import.meta.env.MODE is the config-file mode");
  assert.match(js, /staging-token/, ".env.staging loaded under the config-file mode with the APP_ envPrefix");
  assert.match(js, /custom-ext/, "resolve.extensions from vite.config resolved ./util to util.custom.js");

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app} --config configs/vite.alt.js --mode production`, { stdio: "ignore" });
  assert.ok(fs.existsSync(path.join(app, "alt-out-production")), "CLI --mode wins over the config-file mode");
  assert.match(assets("alt-out-production"), /from-env/, "production build reads .env only");

  // Dev with --config: envPrefix applies too.
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  srv = spawn(oj, ["dev", app, "--port", String(PORT), "--config", "configs/vite.alt.js"], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const main = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  assert.match(main, /from-env/, `dev honors the APP_ envPrefix from vite.config:\n${main}`);
  assert.match(main, /util\.custom\.js/, `dev honors resolve.extensions from vite.config:\n${main}`);
  // Sass additionalData (string form) from vite.config reaches the dev compiler.
  const css = await (await fetch(`http://localhost:${PORT}/src/styles.scss?import`)).text();
  assert.match(css, /rgb\(1,\s*2,\s*3\)|#010203/, `scss additionalData from vite.config applied in dev:\n${css}`);
  console.log("BUILD-CONFIG-FLAG E2E PASSED");
} catch (err) {
  failed = true;
  console.error("BUILD-CONFIG-FLAG E2E FAILED:", err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
