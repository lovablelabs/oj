// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..", "..");
const sidecar = path.join(repo, "crates/oj_server/src/assets/optimize-deps.mjs");

// Runs the module's exported optimize() the way oj's in-process engine calls
// it, in a fresh node child per run (isolation for module caches and cwd).
const OPTIMIZE_WRAPPER = `
import { writeSync } from "node:fs";
import { pathToFileURL } from "node:url";
const [script, cfg] = process.argv.slice(1);
const { optimize } = await import(pathToFileURL(script).href);
writeSync(1, JSON.stringify(await optimize(JSON.parse(cfg))));
process.exit(0);
`;
const runOptimize = (cfg) =>
  JSON.parse(
    execFileSync("node", ["--input-type=module", "-e", OPTIMIZE_WRAPPER, sidecar, typeof cfg === "string" ? cfg : JSON.stringify(cfg)], {
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
    }),
  );
const esbuildSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/esbuild");

// The pre-bundler shells out to esbuild via the start-app fixture's install;
// skip (rather than hard-fail) where that fixture has no node_modules, matching
// the rolldown-fixture convention in asset-routing.test.mjs.
const it = fs.existsSync(esbuildSrc)
  ? test
  : (name, fn) => test(name, { skip: "fixture esbuild not installed" }, () => {});

const pkg = (nm, root, main, files) => {
  const dir = path.join(root, "node_modules", nm);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify({ name: nm, version: "1.0.0", main }));
  for (const [f, c] of Object.entries(files)) fs.writeFileSync(path.join(dir, f), c);
};

function fixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdeps-"));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(root, "node_modules", "@esbuild"));
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({ name: "fx" }));

  pkg("defprop", root, "index.js", {
    "index.js":
      `"use strict";\n` +
      `Object.defineProperty(exports, "__esModule", { value: true });\n` +
      `Object.defineProperty(exports, "greet", { enumerable: true, get: function () { return greet; } });\n` +
      `function greet(n) { return "hi " + n; }\n`,
  });
  pkg("babeldefault", root, "index.js", {
    "index.js":
      `"use strict";\n` +
      `Object.defineProperty(exports, "__esModule", { value: true });\n` +
      `exports.default = void 0;\n` +
      `var _default = function () { return 42; };\n` +
      `exports.default = _default;\n`,
  });
  pkg("plaincjs", root, "index.js", {
    "index.js": `exports.a = 1;\nexports.b = 2;\n`,
  });

  fs.writeFileSync(
    path.join(root, "entry.js"),
    `import { greet } from "defprop";\n` +
      `import fortytwo from "babeldefault";\n` +
      `import { a, b } from "plaincjs";\n` +
      `export const out = greet("x") + "|" + fortytwo() + "|" + a + "|" + b;\n`,
  );
  return root;
}

it("optimize-deps: scans + pre-bundles CJS deps with correct interop", async () => {
  const root = fixture();
  const outDir = path.join(root, ".oj-cache", "deps");
  const cfg = JSON.stringify({ root, outDir, entries: [path.join(root, "entry.js")], autoDiscover: true });
  const { metadata } = runOptimize(cfg);

  assert.deepEqual(Object.keys(metadata).sort(), ["babeldefault", "defprop", "plaincjs"]);
  for (const m of Object.values(metadata)) {
    assert.ok(fs.existsSync(path.join(outDir, m.file)), `missing ${m.file}`);
    assert.equal(m.needsInterop, true, "CJS dep flagged needsInterop");
  }

  const load = (dep) => import(pathToFileURL(path.join(outDir, metadata[dep].file)).href);

  const defprop = await load("defprop");
  assert.equal(defprop.default.greet("x"), "hi x", "Object.defineProperty export preserved through CJS->ESM");
  const babel = await load("babeldefault");
  assert.equal(babel.default.__esModule, true, "Babel __esModule flag preserved (consumer interop unwraps .default)");
  assert.equal(babel.default.default(), 42);
  const plain = await load("plaincjs");
  assert.equal(plain.default.a, 1);
  assert.equal(plain.default.b, 2);

  fs.rmSync(root, { recursive: true, force: true });
});

it("optimize-deps: resolves tsconfig `paths` with /* and externalizes a dep's CSS/font", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdeps2-"));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(root, "node_modules", "@esbuild"));
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({ name: "fx2" }));

  // A tsconfig `paths` value contains `/*` — the exact shape a naive JSONC
  // comment stripper corrupts. If stripJsonc mishandles it, the alias never
  // loads, `@/aliased` is treated as an external bare dep, and `defprop`
  // (reachable ONLY through the alias) is never discovered.
  fs.writeFileSync(
    path.join(root, "tsconfig.json"),
    JSON.stringify({ compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } } }),
  );
  pkg("defprop", root, "index.js", {
    "index.js":
      `"use strict";\nObject.defineProperty(exports, "__esModule", { value: true });\n` +
      `Object.defineProperty(exports, "greet", { enumerable: true, get: function () { return greet; } });\n` +
      `function greet(n) { return "hi " + n; }\n`,
  });
  // A JS dep whose (relative) CSS pulls a (relative) .woff2. If those are not
  // externalized, esbuild fails the whole pre-bundle with "No loader is
  // configured for .woff2" and nothing gets optimized.
  pkg("uikit", root, "index.js", {
    "index.js": `import "./style.css";\nexport const ok = 1;\n`,
    "style.css": `@font-face { font-family: x; src: url(./f.woff2) format("woff2"); }\n`,
    "f.woff2": "not-a-real-font",
  });
  fs.mkdirSync(path.join(root, "src"), { recursive: true });
  fs.writeFileSync(path.join(root, "src", "aliased.js"), `import { greet } from "defprop";\nexport const v = greet("z");\n`);
  fs.writeFileSync(
    path.join(root, "entry.js"),
    `import { v } from "@/aliased";\nimport { ok } from "uikit";\nexport const out = v + ok;\n`,
  );

  const outDir = path.join(root, ".oj-cache", "deps");
  const cfg = JSON.stringify({ root, outDir, entries: [path.join(root, "entry.js")], autoDiscover: true });
  const { metadata } = runOptimize(cfg);
  const names = Object.keys(metadata).sort();

  assert.ok(
    names.includes("defprop"),
    `dep reached only through the tsconfig \`@/\` alias must be discovered (stripJsonc + alias traversal); got ${names.join(", ")}`,
  );
  assert.ok(
    names.includes("uikit"),
    `JS dep with a CSS/font import must still pre-bundle (assets externalized); got ${names.join(", ")}`,
  );
  for (const m of Object.values(metadata)) {
    assert.ok(fs.existsSync(path.join(outDir, m.file)), `missing ${m.file}`);
  }

  fs.rmSync(root, { recursive: true, force: true });
});

it("optimize-deps: never pre-bundles a queried specifier (?worker/?url)", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdeps3-"));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(root, "node_modules", "@esbuild"));
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({ name: "fx3" }));

  pkg("plaincjs", root, "index.js", { "index.js": `exports.a = 1;\n` });
  pkg("wk", root, "index.js", { "index.js": `export default 1;\n`, "worker.js": `self.onmessage = () => {};\n` });

  // A `?worker` import must be externalized, not recorded as a dep (Vite's
  // SPECIAL_QUERY_RE does the same). If it were pre-bundled, the browser would
  // request /@oj-deps/wk_worker.js and 404, breaking the app (the twenty
  // monaco-graphql worker regression).
  fs.writeFileSync(
    path.join(root, "entry.js"),
    `import { a } from "plaincjs";\nimport Worker from "wk/worker.js?worker";\nexport const out = a + typeof Worker;\n`,
  );

  const outDir = path.join(root, ".oj-cache", "deps");
  const cfg = JSON.stringify({ root, outDir, entries: [path.join(root, "entry.js")], autoDiscover: true });
  const { metadata } = runOptimize(cfg);
  const names = Object.keys(metadata);

  assert.ok(names.includes("plaincjs"), `plain dep still pre-bundled; got ${names.join(", ")}`);
  assert.ok(
    !names.some((n) => n.includes("?") || n.includes("worker")),
    `queried/worker specifier must NOT be pre-bundled; got ${names.join(", ")}`,
  );

  fs.rmSync(root, { recursive: true, force: true });
});

it("optimize-deps: does NOT auto-discover by default; only the include list is pre-bundled", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdeps4-"));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(root, "node_modules", "@esbuild"));
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({ name: "fx4" }));

  pkg("plaincjs", root, "index.js", { "index.js": `exports.a = 1;\n` });
  pkg("other", root, "index.js", { "index.js": `exports.b = 2;\n` });
  fs.writeFileSync(
    path.join(root, "entry.js"),
    `import { a } from "plaincjs";\nimport { b } from "other";\nexport const out = a + b;\n`,
  );
  const outDir = path.join(root, ".oj-cache", "deps");

  // Default (no autoDiscover): the esbuild scan does not run, so a dep reached
  // only from the entry graph is NOT pre-bundled — it is served individually via
  // wrap_cjs. This gate keeps a dep's CJS/UMD interop quirks from breaking an app.
  const gated = runOptimize(JSON.stringify({ root, outDir, entries: [path.join(root, "entry.js")] })).metadata;
  assert.equal(Object.keys(gated).length, 0, `default must not auto-discover; got ${Object.keys(gated).join(", ")}`);

  // The explicit include list is always pre-bundled, even without autoDiscover.
  const included = runOptimize(JSON.stringify({ root, outDir, entries: [path.join(root, "entry.js")], include: ["plaincjs"] })).metadata;
  assert.deepEqual(Object.keys(included), ["plaincjs"], "explicit include is pre-bundled");

  fs.rmSync(root, { recursive: true, force: true });
});

it("optimize-deps: expands include globs like Vite and honors needsInterop", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdeps5-"));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(root, "node_modules", "@esbuild"));
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({ name: "fx5" }));

  // No exports map: the glob runs over the package's files (Vite expandGlobIds).
  pkg("plainglob", root, "index.js", {
    "index.js": `exports.root = 1;\n`,
    "alpha.js": `exports.alpha = 1;\n`,
    "beta.js": `exports.beta = 2;\n`,
  });
  // Exports map with a subpath pattern: the glob is matched against the export
  // keys, resolved through the pattern's target files.
  const withExports = path.join(root, "node_modules", "exportsglob");
  fs.mkdirSync(path.join(withExports, "dist", "icons"), { recursive: true });
  fs.writeFileSync(
    path.join(withExports, "package.json"),
    JSON.stringify({
      name: "exportsglob",
      version: "1.0.0",
      exports: { ".": "./dist/index.js", "./icons/*": "./dist/icons/*.js", "./internal": null },
    }),
  );
  fs.writeFileSync(path.join(withExports, "dist", "index.js"), `exports.idx = 1;\n`);
  fs.writeFileSync(path.join(withExports, "dist", "icons", "sun.js"), `exports.sun = 1;\n`);
  fs.writeFileSync(path.join(withExports, "dist", "icons", "moon.js"), `exports.moon = 1;\n`);
  // A real ESM dep: interop is only forced through optimizeDeps.needsInterop.
  const esm = path.join(root, "node_modules", "esmlib");
  fs.mkdirSync(esm, { recursive: true });
  fs.writeFileSync(path.join(esm, "package.json"), JSON.stringify({ name: "esmlib", version: "1.0.0", type: "module", main: "index.js" }));
  fs.writeFileSync(path.join(esm, "index.js"), `export const named = 1;\nexport default { named };\n`);
  fs.writeFileSync(path.join(root, "entry.js"), `export const out = 1;\n`);
  const outDir = path.join(root, ".oj-cache", "deps");

  const run = (extra) =>
    runOptimize(JSON.stringify({ root, outDir, entries: [path.join(root, "entry.js")], ...extra })).metadata;

  const globbed = run({ include: ["plainglob/*.js", "exportsglob/icons/*"] });
  assert.deepEqual(
    Object.keys(globbed).sort(),
    ["exportsglob", "exportsglob/icons/moon", "exportsglob/icons/sun", "plainglob", "plainglob/alpha.js", "plainglob/beta.js", "plainglob/index.js"],
    "the package itself plus every subpath the glob matches",
  );
  for (const m of Object.values(globbed)) assert.ok(fs.existsSync(path.join(outDir, m.file)), `missing ${m.file}`);

  const plain = run({ include: ["esmlib"] });
  assert.equal(plain.esmlib.needsInterop, false, "an ESM dep with named exports needs no interop");
  const forced = run({ include: ["esmlib"], needsInterop: ["esmlib"] });
  assert.equal(forced.esmlib.needsInterop, true, "optimizeDeps.needsInterop forces it in the metadata");

  fs.rmSync(root, { recursive: true, force: true });
});

it("optimize-deps: resolve.dedupe bundles the root copy; entries are globs", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdeps6-"));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(root, "node_modules", "@esbuild"));
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({ name: "fx6" }));

  const esmPkg = (dir, name, code) => {
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify({ name, version: "1.0.0", type: "module", main: "index.js" }));
    fs.writeFileSync(path.join(dir, "index.js"), code);
  };
  // `shared` exists twice: at the root and nested under `consumer`.
  esmPkg(path.join(root, "node_modules", "shared"), "shared", `export const copy = "ROOT_COPY";\n`);
  esmPkg(path.join(root, "node_modules", "consumer"), "consumer", `import { copy } from "shared";\nexport const via = copy;\n`);
  esmPkg(path.join(root, "node_modules", "consumer", "node_modules", "shared"), "shared", `export const copy = "NESTED_COPY";\n`);
  fs.mkdirSync(path.join(root, "src", "pages"), { recursive: true });
  fs.writeFileSync(path.join(root, "src", "pages", "home.js"), `import { via } from "consumer";\nexport const out = via;\n`);
  const outDir = path.join(root, ".oj-cache", "deps");
  const run = (extra) =>
    runOptimize(JSON.stringify({ root, outDir, ...extra })).metadata;

  // Node resolution alone picks the nested copy; resolve.dedupe re-resolves the
  // bare import from the project root (Vite: dedupe -> basedir = root).
  const plain = run({ include: ["consumer"], entries: [path.join(root, "src/pages/home.js")] });
  assert.match(fs.readFileSync(path.join(outDir, plain.consumer.file), "utf8"), /NESTED_COPY/);
  const deduped = run({ include: ["consumer"], dedupe: ["shared"], entries: [path.join(root, "src/pages/home.js")] });
  const bundled = fs.readFileSync(path.join(outDir, deduped.consumer.file), "utf8");
  assert.match(bundled, /ROOT_COPY/, `dedupe must bundle the root copy:\n${bundled}`);
  assert.doesNotMatch(bundled, /NESTED_COPY/);

  // optimizeDeps.entries as a glob (relative to root) feeds the scan.
  const globbed = run({ entries: ["src/**/*.js"], autoDiscover: true });
  assert.deepEqual(Object.keys(globbed).sort(), ["consumer"], "the glob-matched entry was scanned");

  fs.rmSync(root, { recursive: true, force: true });
});
