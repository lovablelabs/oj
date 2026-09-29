// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the SPA plugin-host exposes this.parse (Rollup/Vite parseAst) and
// this.meta on the plugin context. Without them any AST-aware Vite plugin
// throws ("this.parse is not a function") inside transform/resolveId.
// A probe plugin parses its input and reads this.meta; the served module must
// carry the success markers.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const OJ = path.join(repo, "target", "debug", "oj");
const FIXTURE_VITE = path.join(repo, "e2e", "fixtures", "start-app", "node_modules", "vite");
const PORT = 5331;

if (!fs.existsSync(FIXTURE_VITE)) {
  console.log("SKIP plugin-parse: fixture vite not installed (e2e/fixtures/start-app)");
  process.exit(0);
}

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-parse-"));
let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.mkdirSync(path.join(app, "node_modules"), { recursive: true });
  // Reuse a real vite so the host can resolve parseAst + loadConfigFromFile.
  fs.symlinkSync(FIXTURE_VITE, path.join(app, "node_modules", "vite"), "dir");
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"parseapp","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "main.js"), "export const v = 1;\n");
  fs.writeFileSync(
    path.join(app, "vite.config.mjs"),
    `export default {
      plugins: [{
        name: "ast-probe",
        transform(code, id) {
          if (!id.endsWith("main.js")) return null;
          const ast = this.parse(code);
          const parsedOk = !!ast && ast.type === "Program" && Array.isArray(ast.body);
          const metaOk = !!this.meta && typeof this.meta.rollupVersion === "string";
          return code + "\\nexport const __parsedOk = " + parsedOk + ";\\nexport const __metaOk = " + metaOk + ";\\n";
        },
      }],
    };\n`,
  );

  child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app, stdio: "ignore" });
  const up = async () => {
    return waitUp(`http://localhost:${PORT}/`).then(
      () => true,
      () => false,
    );
  };
  if (!(await up())) throw new Error("dev server did not start");

  const mod = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  if (!mod.includes("__parsedOk = true")) {
    throw new Error(`this.parse did not return a Program AST:\n${mod}`);
  }
  if (!mod.includes("__metaOk = true")) {
    throw new Error(`this.meta.rollupVersion missing:\n${mod}`);
  }
  console.log("this.parse -> Program AST: ok");
  console.log("this.meta.rollupVersion:   ok");
  console.log("\nPLUGIN this.parse / this.meta VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
