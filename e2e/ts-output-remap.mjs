// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite remaps a `.js`/`.jsx`/`.mjs`/`.cjs` import with no file on disk to its
// TypeScript source (resolve.ts tryCleanFsResolve), for EVERY filesystem path:
// relative, `resolve.alias` and tsconfig `paths` imports alike, in dev and build.
// An existing `.js` still wins over a sibling `.ts` (the exact file is tried first).

import { spawn, execSync } from "node:child_process";
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
const PORT = Number(process.env.OJ_E2E_PORT || 5533);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-tsremap-"));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write("package.json", JSON.stringify({ name: "tsremap-app", version: "1.0.0", type: "module" }));
write("tsconfig.json", JSON.stringify({ compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } } }));
write("oj.config.json", JSON.stringify({ resolve: { alias: { "~": "./src" } } }));
write("src/utils/a.ts", `export const a = "A_FROM_TS";\n`);
write("src/utils/comp.tsx", `export const comp = "COMP_FROM_TSX";\n`);
write("src/utils/m.mts", `export const m = "M_FROM_MTS";\n`);
write("src/utils/both.js", `export const both = "BOTH_FROM_JS";\n`);
write("src/utils/both.ts", `export const both = "BOTH_FROM_TS";\n`);
write(
  "src/main.ts",
  [
    `import { a } from "@/utils/a.js";`,
    `import { comp } from "~/utils/comp.js";`,
    `import { m } from "./utils/m.mjs";`,
    `import { both } from "@/utils/both.js";`,
    `document.body.textContent = [a, comp, m, both].join(" ");`,
    ``,
  ].join("\n"),
);
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.ts"></script></body></html>`,
);

let failed = false;
let srv = null;
try {
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const res = await fetch(`http://localhost:${PORT}/src/main.ts`);
  const main = await res.text();
  assert.equal(res.status, 200, `main.ts did not compile:\n${main.slice(0, 400)}`);
  const imports = [...main.matchAll(/from\s+"([^"]+)"/g)].map((m) => m[1].replace(/\?.*$/, ""));
  assert.deepEqual(
    imports,
    ["/src/utils/a.ts", "/src/utils/comp.tsx", "/src/utils/m.mts", "/src/utils/both.js"],
    `dev import rewrites:\n${main}`,
  );
  for (const u of imports) {
    const r = await fetch(`http://localhost:${PORT}${u}`);
    assert.equal(r.status, 200, `${u} not served`);
  }
  srv.kill("SIGKILL");
  srv = null;
  await sleep(300);

  execSync(`${oj} build ${app}`, { stdio: "pipe" });
  const assets = path.join(app, "dist", "assets");
  const built = fs
    .readdirSync(assets)
    .filter((f) => f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(assets, f), "utf8"))
    .join("\n");
  for (const marker of ["A_FROM_TS", "COMP_FROM_TSX", "M_FROM_MTS", "BOTH_FROM_JS"]) {
    assert.match(built, new RegExp(marker), `build output lacks ${marker}`);
  }
  assert.doesNotMatch(built, /BOTH_FROM_TS/, "build picked .ts over the existing .js");
  console.log("TS-OUTPUT-REMAP E2E PASSED");
} catch (err) {
  failed = true;
  console.error("TS-OUTPUT-REMAP E2E FAILED:", err.stack || err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
