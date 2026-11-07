// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { makeResolver, importPkg } from "../../crates/oj_server/src/assets/start/resolve-pkg.mjs";

function pkg(dir, name, main = "index.js") {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name, main, version: "1.0.0" }));
  writeFileSync(join(dir, main), "module.exports = {};");
}

test("makeResolver resolves a direct dep from the app root", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    pkg(join(root, "node_modules", "foo"), "foo");
    const resolve = makeResolver(root);
    assert.match(resolve("foo"), /node_modules[/\\]foo[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver reaches a transitive dep through a direct-dep anchor", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-anchor-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    pkg(join(root, "node_modules", "anchor"), "anchor");
    pkg(join(root, "node_modules", "anchor", "node_modules", "dep"), "dep");
    const resolve = makeResolver(root);
    assert.throws(() => resolve("dep"));
    assert.match(resolve("dep", ["anchor"]), /anchor[/\\]node_modules[/\\]dep[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver reaches a dep two hops deep (transitive through a transitive)", () => {
  // Mirrors the real TanStack Start layout under pnpm:
  //   app -> @tanstack/react-start -> @tanstack/start-plugin-core -> @tanstack/router-generator
  // The generator is two hops from the app root, resolvable only by walking the
  // dependency graph, not by anchoring a single level down. The old single-hop
  // resolver threw here; the breadth-first walk finds it.
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-2hop-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app","dependencies":{"anchor":"1.0.0"}}');
    const anchorDir = join(root, "node_modules", "anchor");
    mkdirSync(anchorDir, { recursive: true });
    writeFileSync(
      join(anchorDir, "package.json"),
      '{"name":"anchor","main":"index.js","dependencies":{"mid":"1.0.0"}}',
    );
    writeFileSync(join(anchorDir, "index.js"), "module.exports = {};");
    const midDir = join(anchorDir, "node_modules", "mid");
    mkdirSync(midDir, { recursive: true });
    writeFileSync(join(midDir, "package.json"), '{"name":"mid","main":"index.js","dependencies":{"deep":"1.0.0"}}');
    writeFileSync(join(midDir, "index.js"), "module.exports = {};");
    pkg(join(midDir, "node_modules", "deep"), "deep");
    const resolve = makeResolver(root);
    assert.match(resolve("deep"), /mid[/\\]node_modules[/\\]deep[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver walks past an intermediate whose exports omit ./package.json", () => {
  // Some packages expose an exports map that does not list "./package.json", so
  // "<name>/package.json" is not resolvable; the walk must still find the
  // package's own manifest to read its dependencies and continue.
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-exports-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app","dependencies":{"anchor":"1.0.0"}}');
    const anchorDir = join(root, "node_modules", "anchor");
    mkdirSync(anchorDir, { recursive: true });
    writeFileSync(
      join(anchorDir, "package.json"),
      '{"name":"anchor","exports":{".":"./index.js"},"dependencies":{"deep":"1.0.0"}}',
    );
    writeFileSync(join(anchorDir, "index.js"), "module.exports = {};");
    pkg(join(anchorDir, "node_modules", "deep"), "deep");
    assert.throws(() => makeResolver(root)("anchor/package.json"));
    assert.match(makeResolver(root)("deep"), /anchor[/\\]node_modules[/\\]deep[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver throws a clear error for a missing package", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-miss-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    const resolve = makeResolver(root);
    assert.throws(() => resolve("does-not-exist"), /cannot resolve/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

function writePkg(root, name, files) {
  const dir = join(root, "node_modules", name);
  mkdirSync(dir, { recursive: true });
  for (const [f, contents] of Object.entries(files)) writeFileSync(join(dir, f), contents);
}

test("importPkg imports a CJS package and unwraps module.exports", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-cjs-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    writePkg(root, "cjspkg", {
      "package.json": '{"name":"cjspkg","main":"index.js"}',
      "index.js": 'module.exports = { version: "1.0", build: () => "built" };',
    });
    const mod = await importPkg(root, "cjspkg");
    assert.equal(mod.version, "1.0");
    assert.equal(typeof mod.build, "function");
    assert.equal(mod.build(), "built");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("importPkg imports an ESM package and returns its namespace", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-esm-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    writePkg(root, "esmpkg", {
      "package.json": '{"name":"esmpkg","main":"index.mjs"}',
      "index.mjs": 'export const value = 42; export function greet() { return "hi"; }',
    });
    const mod = await importPkg(root, "esmpkg");
    assert.equal(mod.value, 42);
    assert.equal(mod.greet(), "hi");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("importPkg reaches a transitive dep through a preferred anchor", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-anchor-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    writePkg(root, "anchor", { "package.json": '{"name":"anchor"}' });
    writePkg(root, join("anchor", "node_modules", "dep"), {
      "package.json": '{"name":"dep","main":"index.js"}',
      "index.js": "module.exports = { ok: true };",
    });
    const mod = await importPkg(root, "dep", ["anchor"]);
    assert.equal(mod.ok, true);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("tailwind engine module resolves Tailwind v4 dependencies beneath the Vite plugin", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-tailwind-strict-layout-"));
  try {
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({
        name: "synthetic-app",
        dependencies: { "@tailwindcss/vite": "1.0.0" },
      }),
    );
    writePkg(root, "@tailwindcss/vite", {
      "package.json": JSON.stringify({
        name: "@tailwindcss/vite",
        main: "index.js",
        dependencies: { "@tailwindcss/node": "1.0.0", "@tailwindcss/oxide": "1.0.0" },
      }),
      "index.js": "module.exports = {};",
    });
    const anchor = join(root, "node_modules", "@tailwindcss", "vite");
    writePkg(anchor, "@tailwindcss/node", {
      "package.json": '{"name":"@tailwindcss/node","type":"module","main":"index.mjs"}',
      "index.mjs":
        "export async function compile(source) { return { build(tokens) { return source + tokens.join(','); } }; }",
    });
    writePkg(anchor, "@tailwindcss/oxide", {
      "package.json": '{"name":"@tailwindcss/oxide","type":"module","main":"index.mjs"}',
      "index.mjs": "export class Scanner { files = []; globs = []; scan() { return ['synthetic-tailwind-token']; } }",
    });

    const requireFromApp = createRequire(join(root, "package.json"));
    assert.throws(() => requireFromApp.resolve("@tailwindcss/node"), { code: "MODULE_NOT_FOUND" });
    assert.throws(() => requireFromApp.resolve("@tailwindcss/oxide"), { code: "MODULE_NOT_FOUND" });

    const stylesheet = join(root, "styles.css");
    writeFileSync(stylesheet, '@import "tailwindcss";');
    const { compile } = await import(
      new URL("../../crates/oj_server/src/assets/css-tailwind.mjs", import.meta.url).href
    );
    const { css } = await compile({ base: root, css: '@import "tailwindcss";', from: stylesheet });
    assert.equal(css, '@import "tailwindcss";synthetic-tailwind-token');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("tailwind v4 engine scans every @source from the sheet's dir and reports what it read", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-tailwind-sources-"));
  try {
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ name: "synthetic-app", dependencies: { tailwindcss: "1.0.0" } }),
    );
    writePkg(root, "@tailwindcss/node", {
      "package.json": '{"name":"@tailwindcss/node","type":"module","main":"index.mjs"}',
      "index.mjs": `export async function compile(source, opts) {
        globalThis.__twBase = opts.base;
        opts.onDependency("/ui/theme.css");
        return {
          root: null,
          sources: [{ base: "/ui/src", pattern: "**/*", negated: false }],
          build(tokens) { return tokens.join(","); },
        };
      }`,
    });
    writePkg(root, "@tailwindcss/oxide", {
      "package.json": '{"name":"@tailwindcss/oxide","type":"module","main":"index.mjs"}',
      "index.mjs": `export class Scanner {
        constructor({ sources }) { globalThis.__twSources = sources; }
        files = ["/ui/src/Button.tsx", "${join(root, "src", "index.css")}"];
        globs = [{ base: "${root}", pattern: "*" }, { base: "/ui/src", pattern: "**/*" }];
        scan() { return ["underline"]; }
      }`,
    });
    const { compile } = await import(
      new URL("../../crates/oj_server/src/assets/css-tailwind.mjs", import.meta.url).href
    );
    const from = join(root, "src", "index.css");
    const out = await compile({ base: root, css: '@import "tailwindcss";', from });
    assert.equal(out.css, "underline");
    assert.equal(globalThis.__twBase, join(root, "src"), "@source resolves from the sheet's dir");
    assert.deepEqual(globalThis.__twSources, [
      { base: root, pattern: "**/*", negated: false },
      { base: "/ui/src", pattern: "**/*", negated: false },
    ]);
    assert.deepEqual(out.deps, ["/ui/theme.css", "/ui/src/Button.tsx"]);
    assert.deepEqual(out.dirDeps, [{ dir: "/ui/src", glob: "**/*" }]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("importPkg rejects when the package cannot be resolved", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-miss-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    await assert.rejects(importPkg(root, "not-installed"), /cannot resolve/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
