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
const fixtureModules = path.join(repo, "e2e/fixtures/start-app/node_modules");

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
    execFileSync("node", ["--input-type=module", "-e", OPTIMIZE_WRAPPER, sidecar, JSON.stringify(cfg)], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      maxBuffer: 64 * 1024 * 1024,
    }),
  );

// One app shape per bundler Vite has used for its optimizer: a Vite 8 app
// (its vite brings rolldown, the app has no esbuild) and an app that only has
// esbuild. Both borrow the start-app fixture's install.
const BUNDLERS = {
  rolldown: fs.existsSync(path.join(fixtureModules, "vite", "package.json")),
  esbuild: fs.existsSync(path.join(fixtureModules, "esbuild")),
};
const skipFor = (bundler) => (BUNDLERS[bundler] ? false : `fixture ${bundler} not installed`);
const each = (name, fn) => {
  for (const bundler of Object.keys(BUNDLERS)) {
    test(`[${bundler}] ${name}`, { skip: skipFor(bundler) }, () => fn(bundler));
  }
};
const only = (bundler, name, fn) => test(`[${bundler}] ${name}`, { skip: skipFor(bundler) }, fn);

function makeRoot(bundler, prefix = "oj-optdeps-") {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  const nm = path.join(root, "node_modules");
  fs.mkdirSync(nm, { recursive: true });
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({ name: "fx" }));
  if (bundler === "rolldown") {
    fs.symlinkSync(path.join(fixtureModules, "vite"), path.join(nm, "vite"));
  } else if (bundler === "esbuild") {
    fs.symlinkSync(path.join(fixtureModules, "esbuild"), path.join(nm, "esbuild"));
    const scoped = path.join(fixtureModules, "@esbuild");
    if (fs.existsSync(scoped)) fs.symlinkSync(scoped, path.join(nm, "@esbuild"));
  }
  return root;
}

const pkg = (nm, root, main, files, extra = {}) => {
  const dir = path.join(root, "node_modules", nm);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify({ name: nm, version: "1.0.0", main, ...extra }));
  for (const [f, c] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(dir, f)), { recursive: true });
    fs.writeFileSync(path.join(dir, f), c);
  }
};
const esmPkg = (nm, root, files, extra = {}) => pkg(nm, root, "index.js", files, { type: "module", ...extra });
const write = (root, rel, content) => {
  fs.mkdirSync(path.dirname(path.join(root, rel)), { recursive: true });
  fs.writeFileSync(path.join(root, rel), content);
};
const outDirOf = (root) => path.join(root, ".oj-cache", "deps");
const cleanup = (dir) => fs.rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });

each("scans + pre-bundles CJS deps with correct interop", async (bundler) => {
  const root = makeRoot(bundler);
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
  pkg("plaincjs", root, "index.js", { "index.js": `exports.a = 1;\nexports.b = 2;\n` });
  write(
    root,
    "entry.js",
    `import { greet } from "defprop";\n` +
      `import fortytwo from "babeldefault";\n` +
      `import { a, b } from "plaincjs";\n` +
      `export const out = greet("x") + "|" + fortytwo() + "|" + a + "|" + b;\n`,
  );
  const outDir = outDirOf(root);
  const result = runOptimize({ root, outDir, entries: [path.join(root, "entry.js")] });
  assert.equal(result.bundler, bundler);
  const { metadata } = result;

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
  cleanup(root);
});

each("resolves tsconfig `paths` with /* and externalizes a dep's CSS/font", (bundler) => {
  const root = makeRoot(bundler, "oj-optdeps2-");
  // A tsconfig `paths` value contains `/*`, the exact shape a naive JSONC
  // comment stripper corrupts; `defprop` is reachable ONLY through the alias.
  write(root, "tsconfig.json", JSON.stringify({ compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } } }));
  pkg("defprop", root, "index.js", {
    "index.js":
      `"use strict";\nObject.defineProperty(exports, "__esModule", { value: true });\n` +
      `Object.defineProperty(exports, "greet", { enumerable: true, get: function () { return greet; } });\n` +
      `function greet(n) { return "hi " + n; }\n`,
  });
  // A JS dep whose CSS pulls a .woff2: bundling either would fail the whole
  // pre-bundle, so both stay external.
  pkg("uikit", root, "index.js", {
    "index.js": `import "./style.css";\nexport const ok = 1;\n`,
    "style.css": `@font-face { font-family: x; src: url(./f.woff2) format("woff2"); }\n`,
    "f.woff2": "not-a-real-font",
  });
  write(root, "src/aliased.js", `import { greet } from "defprop";\nexport const v = greet("z");\n`);
  write(root, "entry.js", `import { v } from "@/aliased";\nimport { ok } from "uikit";\nexport const out = v + ok;\n`);

  const outDir = outDirOf(root);
  const { metadata } = runOptimize({ root, outDir, entries: [path.join(root, "entry.js")] });
  const names = Object.keys(metadata).sort();
  assert.ok(names.includes("defprop"), `dep reached only through the tsconfig alias; got ${names.join(", ")}`);
  assert.ok(names.includes("uikit"), `JS dep with a CSS/font import still pre-bundles; got ${names.join(", ")}`);
  for (const m of Object.values(metadata)) assert.ok(fs.existsSync(path.join(outDir, m.file)), `missing ${m.file}`);
  cleanup(root);
});

each("never pre-bundles a queried specifier (?worker/?url)", (bundler) => {
  const root = makeRoot(bundler, "oj-optdeps3-");
  pkg("plaincjs", root, "index.js", { "index.js": `exports.a = 1;\n` });
  pkg("wk", root, "index.js", { "index.js": `export default 1;\n`, "worker.js": `self.onmessage = () => {};\n` });
  // Vite's SPECIAL_QUERY_RE: a pre-bundled `?worker` import would 404 at
  // /@oj-deps/wk_worker.js.
  write(
    root,
    "entry.js",
    `import { a } from "plaincjs";\nimport Worker from "wk/worker.js?worker";\nexport const out = a + typeof Worker;\n`,
  );
  const { metadata } = runOptimize({ root, outDir: outDirOf(root), entries: [path.join(root, "entry.js")] });
  const names = Object.keys(metadata);
  assert.ok(names.includes("plaincjs"), `plain dep still pre-bundled; got ${names.join(", ")}`);
  assert.ok(!names.some((n) => n.includes("?") || n.includes("worker")), `queried specifier pre-bundled: ${names}`);
  cleanup(root);
});

each("discovers deps by default; noDiscovery leaves only the include list", (bundler) => {
  const root = makeRoot(bundler, "oj-optdeps4-");
  pkg("plaincjs", root, "index.js", { "index.js": `exports.a = 1;\n` });
  pkg("other", root, "index.js", { "index.js": `exports.b = 2;\n` });
  write(root, "entry.js", `import { a } from "plaincjs";\nimport { b } from "other";\nexport const out = a + b;\n`);
  const outDir = outDirOf(root);
  const entries = [path.join(root, "entry.js")];

  const discovered = runOptimize({ root, outDir, entries }).metadata;
  assert.deepEqual(Object.keys(discovered).sort(), ["other", "plaincjs"], "Vite's default crawls the entries");

  const gated = runOptimize({ root, outDir, entries, autoDiscover: false }).metadata;
  assert.equal(Object.keys(gated).length, 0, `noDiscovery must not crawl; got ${Object.keys(gated)}`);

  const included = runOptimize({ root, outDir, entries, autoDiscover: false, include: ["plaincjs"] }).metadata;
  assert.deepEqual(Object.keys(included), ["plaincjs"], "the include list is pre-bundled either way");
  cleanup(root);
});

each("expands include globs like Vite and honors needsInterop", (bundler) => {
  const root = makeRoot(bundler, "oj-optdeps5-");
  // No exports map: the glob runs over the package's files (Vite expandGlobIds).
  pkg("plainglob", root, "index.js", {
    "index.js": `exports.root = 1;\n`,
    "alpha.js": `exports.alpha = 1;\n`,
    "beta.js": `exports.beta = 2;\n`,
  });
  // Exports map with a subpath pattern: the glob matches the export keys.
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
  esmPkg("esmlib", root, { "index.js": `export const named = 1;\nexport default { named };\n` });
  write(root, "entry.js", `export const out = 1;\n`);
  const run = (extra) =>
    runOptimize({ root, outDir: outDirOf(root), entries: [path.join(root, "entry.js")], ...extra }).metadata;

  const globbed = run({ include: ["plainglob/*.js", "exportsglob/icons/*"] });
  assert.deepEqual(
    Object.keys(globbed).sort(),
    [
      "exportsglob",
      "exportsglob/icons/moon",
      "exportsglob/icons/sun",
      "plainglob",
      "plainglob/alpha.js",
      "plainglob/beta.js",
      "plainglob/index.js",
    ],
    "the package itself plus every subpath the glob matches",
  );
  for (const m of Object.values(globbed)) assert.ok(fs.existsSync(path.join(outDirOf(root), m.file)));

  assert.equal(run({ include: ["esmlib"] }).esmlib.needsInterop, false, "an ESM dep needs no interop");
  assert.equal(
    run({ include: ["esmlib"], needsInterop: ["esmlib"] }).esmlib.needsInterop,
    true,
    "optimizeDeps.needsInterop forces it in the metadata",
  );
  cleanup(root);
});

each("resolve.dedupe bundles the root copy; entries are globs", (bundler) => {
  const root = makeRoot(bundler, "oj-optdeps6-");
  esmPkg("shared", root, { "index.js": `export const copy = "ROOT_COPY";\n` });
  esmPkg("consumer", root, {
    "index.js": `import { copy } from "shared";\nexport const via = copy;\n`,
    "node_modules/shared/package.json": JSON.stringify({ name: "shared", type: "module", main: "index.js" }),
    "node_modules/shared/index.js": `export const copy = "NESTED_COPY";\n`,
  });
  write(root, "src/pages/home.js", `import { via } from "consumer";\nexport const out = via;\n`);
  const outDir = outDirOf(root);
  const run = (extra) => runOptimize({ root, outDir, ...extra }).metadata;
  const entries = [path.join(root, "src/pages/home.js")];

  const plain = run({ include: ["consumer"], entries, autoDiscover: false });
  assert.match(fs.readFileSync(path.join(outDir, plain.consumer.file), "utf8"), /NESTED_COPY/);
  const deduped = run({ include: ["consumer"], dedupe: ["shared"], entries, autoDiscover: false });
  const bundled = fs.readFileSync(path.join(outDir, deduped.consumer.file), "utf8");
  assert.match(bundled, /ROOT_COPY/, `dedupe must bundle the root copy:\n${bundled}`);
  assert.doesNotMatch(bundled, /NESTED_COPY/);

  // Vite's nested-dependency syntax: "consumer > shared" pre-bundles the copy
  // nested under consumer and registers it under what the app imports.
  const nested = run({ include: ["consumer > shared"], entries, autoDiscover: false });
  assert.deepEqual(Object.keys(nested), ["shared"]);
  assert.match(fs.readFileSync(path.join(outDir, nested.shared.file), "utf8"), /NESTED_COPY/);

  const globbed = run({ entries: ["src/**/*.js"] });
  assert.deepEqual(Object.keys(globbed).sort(), ["consumer"], "the glob-matched entry was scanned");
  cleanup(root);
});

each("a CJS react-family dep gets a named-export facade over its bundle", async (bundler) => {
  const root = makeRoot(bundler, "oj-optdeps7-");
  pkg("react", root, "index.js", { "index.js": `exports.useState = function () { return "state"; };\n` });
  write(root, "entry.js", `import { useState } from "react";\nexport const out = useState();\n`);
  const outDir = outDirOf(root);
  const { metadata } = runOptimize({ root, outDir, entries: [path.join(root, "entry.js")] });
  assert.equal(metadata.react.needsInterop, false, "the facade links as ESM");
  const facade = await import(pathToFileURL(path.join(outDir, metadata.react.file)).href);
  assert.equal(facade.useState(), "state");
  assert.equal(facade.default.useState, facade.useState, "default is module.exports");
  cleanup(root);
});

only("rolldown", "a Vite 8 app without esbuild pre-bundles with its vite's rolldown", () => {
  const root = makeRoot("rolldown", "oj-optdeps-v8-");
  pkg("plaincjs", root, "index.js", { "index.js": `exports.a = 1;\n` });
  write(root, "index.html", `<script type="module" src="/main.js"></script>`);
  write(root, "main.js", `import { a } from "plaincjs";\nconsole.log(a);\n`);
  assert.throws(
    () => execFileSync("node", ["-e", "require.resolve('esbuild')"], { cwd: root, stdio: "ignore" }),
    "the app itself has no esbuild",
  );
  const outDir = outDirOf(root);
  const { metadata, bundler } = runOptimize({ root, outDir });
  assert.equal(bundler, "rolldown");
  assert.deepEqual(Object.keys(metadata), ["plaincjs"], "discovered from index.html");
  assert.match(
    fs.readFileSync(path.join(outDir, metadata.plaincjs.file), "utf8"),
    /export default require_plaincjs\(\)/,
  );
  cleanup(root);
});

only("esbuild", "a Vite <= 7 app keeps its vite's esbuild", () => {
  const root = makeRoot(null, "oj-optdeps-v7-");
  const vite = path.join(root, "node_modules", "vite");
  fs.mkdirSync(path.join(vite, "node_modules"), { recursive: true });
  fs.writeFileSync(
    path.join(vite, "package.json"),
    JSON.stringify({ name: "vite", version: "7.1.0", exports: { "./package.json": "./package.json" } }),
  );
  fs.symlinkSync(path.join(fixtureModules, "esbuild"), path.join(vite, "node_modules", "esbuild"));
  const scoped = path.join(fixtureModules, "@esbuild");
  if (fs.existsSync(scoped)) fs.symlinkSync(scoped, path.join(vite, "node_modules", "@esbuild"));
  pkg("plaincjs", root, "index.js", { "index.js": `exports.a = 1;\n` });
  write(root, "entry.js", `import { a } from "plaincjs";\nexport const out = a;\n`);
  const { metadata, bundler } = runOptimize({ root, outDir: outDirOf(root), entries: [path.join(root, "entry.js")] });
  assert.equal(bundler, "esbuild");
  assert.deepEqual(Object.keys(metadata), ["plaincjs"]);
  cleanup(root);
});

only("rolldown", "an app without vite uses the rolldown vendored next to oj", () => {
  const root = makeRoot(null, "oj-optdeps-vendor-");
  const vendor = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vendor-"));
  fs.mkdirSync(path.join(vendor, "node_modules"));
  fs.writeFileSync(path.join(vendor, "package.json"), JSON.stringify({ name: "oj-vendor", private: true }));
  fs.symlinkSync(path.join(fixtureModules, "rolldown"), path.join(vendor, "node_modules", "rolldown"));
  pkg("plaincjs", root, "index.js", { "index.js": `exports.a = 1;\n` });
  write(root, "entry.js", `import { a } from "plaincjs";\nexport const out = a;\n`);
  const run = (vendoredRolldown) =>
    runOptimize({ root, outDir: outDirOf(root), entries: [path.join(root, "entry.js")], vendoredRolldown });
  const { metadata, bundler } = run(vendor);
  assert.equal(bundler, "rolldown");
  assert.deepEqual(Object.keys(metadata), ["plaincjs"]);
  assert.throws(() => run(undefined), /no dependency bundler found/, "nothing to bundle with: a clear error");
  cleanup(root);
  cleanup(vendor);
});

only("rolldown", "what a bundle keeps external is a URL oj serves (Vite runs importAnalysis there)", () => {
  const root = makeRoot("rolldown", "oj-optdeps-ext-");
  esmPkg("styled", root, {
    "index.js":
      `import "./s.css";\n` +
      `export const wasm = new URL("./w.wasm", import.meta.url).href;\n` +
      `export const remote = new URL("https://example.com/x.png", import.meta.url).href;\n` +
      `export { x } from "excluded";\n`,
    "s.css": "a{}",
    "w.wasm": "",
  });
  esmPkg("excluded", root, { "index.js": `export const x = 1;\n` });
  pkg("usesfs", root, "index.js", { "index.js": `exports.read = require("fs").readFileSync;\n` });
  esmPkg(
    "peery",
    root,
    { "index.js": `import m from "missing-peer";\nexport const p = m;\n` },
    { peerDependencies: { "missing-peer": "*" }, peerDependenciesMeta: { "missing-peer": { optional: true } } },
  );
  write(
    root,
    "entry.js",
    `import { wasm, x } from "styled";\nimport { read } from "usesfs";\nimport { p } from "peery";\nexport default [wasm, x, read, p];\n`,
  );
  const outDir = outDirOf(root);
  const { metadata } = runOptimize({ root, outDir, entries: [path.join(root, "entry.js")], exclude: ["excluded"] });
  assert.deepEqual(Object.keys(metadata).sort(), ["peery", "styled", "usesfs"]);

  const styled = fs.readFileSync(path.join(outDir, metadata.styled.file), "utf8");
  assert.match(styled, /import "\/node_modules\/styled\/s\.css";/, `CSS kept as its served URL:\n${styled}`);
  assert.match(styled, /from "\/node_modules\/excluded\/index\.js"/, "an excluded dep is its served URL");
  assert.match(styled, /new URL\("\/node_modules\/styled\/w\.wasm", import\.meta\.url\)/, "new URL rebased");
  assert.match(styled, /new URL\("https:\/\/example\.com\/x\.png", import\.meta\.url\)/, "remote URL untouched");

  const usesfs = fs.readFileSync(path.join(outDir, metadata.usesfs.file), "utf8");
  assert.match(usesfs, /has been externalized for browser compatibility/, "a node builtin is a browser stub");
  assert.doesNotMatch(usesfs, /from "fs"|require\("fs"\)/, "no bare builtin left for the browser");

  const peery = fs.readFileSync(path.join(outDir, metadata.peery.file), "utf8");
  assert.match(peery, /Could not resolve \\"missing-peer\\" imported by \\"peery\\"/, "optional peer stub");
  cleanup(root);
});

only("rolldown", "ESM entries need no interop; linked packages are crawled, not bundled", () => {
  const root = makeRoot("rolldown", "oj-optdeps-esm-");
  esmPkg("esmdefault", root, { "index.js": `export default function hi() { return 1; }\n` });
  esmPkg("reexports", root, { "index.js": `export * from "./inner.js";\n`, "inner.js": `export const z = 1;\n` });
  pkg("deep", root, "index.js", { "index.js": `exports.d = 1;\n` });
  // A workspace package symlinked into node_modules: Vite keeps crawling it and
  // pre-bundles what IT imports, never the linked source itself. Its own
  // import of `deep` resolves from the app root (a hoisted install).
  const ws = path.join(root, "packages", "linked");
  write(root, "packages/linked/package.json", JSON.stringify({ name: "linked", type: "module", main: "index.js" }));
  write(root, "packages/linked/index.js", `export { d } from "deep";\n`);
  fs.symlinkSync(ws, path.join(root, "node_modules", "linked"));
  write(
    root,
    "entry.js",
    `import hi from "esmdefault";\nimport { z } from "reexports";\nimport { d } from "linked";\nexport default [hi, z, d];\n`,
  );
  const { metadata } = runOptimize({ root, outDir: outDirOf(root), entries: [path.join(root, "entry.js")] });
  assert.deepEqual(Object.keys(metadata).sort(), ["deep", "esmdefault", "reexports"]);
  assert.equal(metadata.esmdefault.needsInterop, false, "a default-only ESM entry is ESM (Vite's hasModuleSyntax)");
  assert.equal(metadata.reexports.needsInterop, false);
  assert.equal(metadata.deep.needsInterop, true);
  cleanup(root);
});
