// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A Start dependency entered through its ESM `module` entry re-exports from
// EXTENSIONLESS relative paths (the @supabase/functions-js shape: no `exports`
// map, `module` points at a bundler-only ESM build). Plain Node semantics die
// with ERR_MODULE_NOT_FOUND ("Did you mean to import with the .js
// extension?") and SSR 500s on every request; the host must finish those
// relative imports with the same Vite-style extension inference that picked
// the entry. Run with a built target/debug/oj; installs a copy of the
// start-app fixture (skips when offline).
import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const fixture = path.join(repo, "e2e", "fixtures", "start-app");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-dep-esm-relative-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true });
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/\/(node_modules|\.oj-cache|dist)(\/|$)/.test(src),
});
try {
  execSync("npm install --no-audit --no-fund --no-package-lock --loglevel=error", { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP dep-esm-relative-import: could not install the fixture (offline?)");
  cleanup();
  process.exit(0);
}

// esm-lib: no `exports`, no `type`, dual entry — the resolver picks `module`,
// a bundler-only ESM build whose relative imports carry no extension (both a
// `./` sibling and a `../` parent hop). The CJS `main` answers "greet:hi-cjs"
// so a wrong-entry resolution is visible.
const esmLib = path.join(app, "node_modules", "esm-lib");
fs.mkdirSync(path.join(esmLib, "dist", "module"), { recursive: true });
fs.mkdirSync(path.join(esmLib, "dist", "main"), { recursive: true });
fs.writeFileSync(
  path.join(esmLib, "package.json"),
  JSON.stringify({ name: "esm-lib", version: "1.0.0", main: "dist/main/index.js", module: "dist/module/index.js" }),
);
fs.writeFileSync(
  path.join(esmLib, "dist", "module", "index.js"),
  `export { greet } from './greet';\nexport { helper } from './helpers';\nexport { which } from './helpers-exports';\nexport { broken } from './broken';\nexport { sugar } from './sugar';\n`,
);
fs.writeFileSync(
  path.join(esmLib, "dist", "module", "greet.js"),
  `import { LIB_VERSION } from '../version';\nexport const greet = () => "hi-esm-" + LIB_VERSION;\n`,
);
// A stray TS source next to its published output (packages ship these):
// extension inference must stay JS-first here, like Vite's
// DEFAULT_EXTENSIONS, or the source outranks the build.
fs.writeFileSync(path.join(esmLib, "dist", "module", "greet.ts"), `export const greet = (): string => "hi-ts";\n`);
fs.writeFileSync(path.join(esmLib, "dist", "version.js"), `export const LIB_VERSION = 4;\n`);
fs.writeFileSync(
  path.join(esmLib, "dist", "main", "index.js"),
  `exports.greet = () => "hi-cjs";\nexports.helper = "hi-cjs";\nexports.which = "hi-cjs";\nexports.broken = "hi-cjs";\nexports.sugar = "hi-cjs";\n`,
);

// A relative DIRECTORY import whose subdir carries its own package.json and
// no index.* (Vite consults the manifest before index probing; Node rejects
// directory imports outright). `module` must win over `main` here too.
const helpers = path.join(esmLib, "dist", "module", "helpers");
fs.mkdirSync(helpers, { recursive: true });
fs.writeFileSync(path.join(helpers, "package.json"), JSON.stringify({ module: "./m.js", main: "./c.js" }));
fs.writeFileSync(path.join(helpers, "m.js"), `export const helper = "dir-esm";\n`);
fs.writeFileSync(path.join(helpers, "c.js"), `exports.helper = "dir-cjs";\n`);

// And a directory manifest carrying BOTH exports["."] and module: Vite's
// resolvePackageEntry takes the exports entry first, even path-reached.
const helpersExports = path.join(esmLib, "dist", "module", "helpers-exports");
fs.mkdirSync(helpersExports, { recursive: true });
fs.writeFileSync(
  path.join(helpersExports, "package.json"),
  JSON.stringify({ exports: { ".": { import: "./e.js" } }, module: "./m.js" }),
);
fs.writeFileSync(path.join(helpersExports, "e.js"), `export const which = "exp-entry";\n`);
fs.writeFileSync(path.join(helpersExports, "m.js"), `export const which = "mod-entry";\n`);

// A manifest whose exports name a MISSING file: Vite throws in
// resolvePackageEntry, catches, and probes index.* — never the module pick.
const broken = path.join(esmLib, "dist", "module", "broken");
fs.mkdirSync(broken, { recursive: true });
fs.writeFileSync(
  path.join(broken, "package.json"),
  JSON.stringify({ exports: { ".": { import: "./missing.js" } }, module: "./m.js" }),
);
fs.writeFileSync(path.join(broken, "index.js"), `export const broken = "idx-entry";\n`);
fs.writeFileSync(path.join(broken, "m.js"), `export const broken = "mod2-entry";\n`);

// Node's exports sugar: a bare string is the "." target.
const sugar = path.join(esmLib, "dist", "module", "sugar");
fs.mkdirSync(sugar, { recursive: true });
fs.writeFileSync(path.join(sugar, "package.json"), JSON.stringify({ exports: "./s.js", module: "./m.js" }));
fs.writeFileSync(path.join(sugar, "s.js"), `export const sugar = "sugar-entry";\n`);
fs.writeFileSync(path.join(sugar, "m.js"), `export const sugar = "mod3-entry";\n`);

// Reach esm-lib from a route so SSR links the chain (the fixture's route
// tree is code-based, so the new route registers in routeTree.ts).
fs.writeFileSync(
  path.join(app, "src", "routes", "esm-relative.tsx"),
  `import { createRoute } from "@tanstack/react-router";
import { greet, helper, which, broken, sugar } from "esm-lib";

import { rootRoute } from "./__root";

export const esmRelativeRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/esm-relative",
  component: () => <main data-testid="esm-relative">{"greet:" + greet() + ":" + helper + ":" + which + ":" + broken + ":" + sugar}</main>,
});
`,
);
const routeTreePath = path.join(app, "src", "routeTree.ts");
const routeTree = fs
  .readFileSync(routeTreePath, "utf8")
  .replace('import { rootRoute }', 'import { esmRelativeRoute } from "./routes/esm-relative";\nimport { rootRoute }')
  .replace("requestUrlRoute]", "requestUrlRoute, esmRelativeRoute]");
if (!routeTree.includes("esmRelativeRoute]")) throw new Error("could not register the test route");
fs.writeFileSync(routeTreePath, routeTree);

const port = 5219;
const server = spawn(oj, ["dev", ".", "--port", String(port), "--host=127.0.0.1"], {
  cwd: app,
  stdio: ["ignore", "pipe", "pipe"],
});
let log = "";
server.stdout.on("data", (d) => (log += d));
server.stderr.on("data", (d) => (log += d));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
try {
  let body;
  let status = 0;
  for (let i = 0; i < 120; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/esm-relative`);
      status = res.status;
      body = await res.text();
      if (status === 200 || status === 500) break;
    } catch {}
    await sleep(500);
  }
  if (status !== 200 || !body.includes("greet:hi-esm-4:dir-esm:exp-entry:idx-entry:sugar-entry")) {
    console.error(log.slice(-4000));
    throw new Error(
      `SSR of an ESM dependency's extensionless relative import failed: status ${status}` +
        (log.includes("ERR_MODULE_NOT_FOUND") ? " (Node semantics rejected the extensionless path)" : "") +
        (log.includes("ERR_UNSUPPORTED_DIR_IMPORT") ? " (Node semantics rejected the directory import)" : "") +
        (body?.includes("hi-cjs") ? " (fell back to the CJS main)" : "") +
        (body?.includes("dir-cjs") ? " (directory entry picked main over module)" : "") +
        (body?.includes("mod-entry") ? " (directory entry picked module over exports)" : "") +
        (body?.includes("mod2-entry") ? " (broken exports target fell to module instead of index)" : "") +
        (body?.includes("mod3-entry") ? " (string-sugar exports lost to the module field)" : "") +
        (body?.includes("hi-ts") || log.includes("greet.ts") ? " (a stray .ts source outranked the .js build)" : ""),
    );
  }
} finally {
  server.kill("SIGKILL");
  cleanup();
}
console.log("dep-esm-relative-import: ok");
