// SPDX-License-Identifier: MIT

// The `declare module` block the framework's Vite plugin appends to every
// generated route tree (moduleDeclaration in start-plugin-core's
// start-router-plugin). The app's Register interface is keyed off it, and so is
// every createFileRoute in the app -- so a tree generated without it is a
// different file from the one the framework's own tooling produces.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { asset, repo } from "./harness.mjs";

const fixture = join(repo, "e2e/fixtures/start-app");
const installed = existsSync(join(fixture, "node_modules/@tanstack/router-generator"));
const maybe = installed ? test : test.skip;

// The generator resolves @tanstack/router-generator from the app root, so the
// app needs a node_modules; nothing else about the fixture matters here.
function app(label, { pkg } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "oj-tree-footer-" + label + "-"));
  mkdirSync(join(dir, "src", "routes"), { recursive: true });
  symlinkSync(join(fixture, "node_modules"), join(dir, "node_modules"), "dir");
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "app", type: "module", ...pkg }));
  writeFileSync(
    join(dir, "src", "routes", "__root.tsx"),
    'import { createRootRoute } from "@tanstack/react-router";\n' + "export const Route = createRootRoute();\n",
  );
  writeFileSync(
    join(dir, "src", "routes", "index.tsx"),
    'import { createFileRoute } from "@tanstack/react-router";\n' + 'export const Route = createFileRoute("/")({});\n',
  );
  return dir;
}

const generate = (dir) =>
  execFileSync(process.execPath, [asset("start/generate.mjs")], {
    env: { ...process.env, OJ_APP_ROOT: dir },
    stdio: ["ignore", "pipe", "pipe"],
  });

maybe("the generated tree carries the framework's declare-module footer", () => {
  const dir = app("default");
  try {
    writeFileSync(join(dir, "src", "router.ts"), "export function getRouter() {}\n");
    generate(dir);
    const tree = readFileSync(join(dir, "src", "routeTree.gen.ts"), "utf8");
    assert.match(tree, /import type \{ getRouter \} from '\.\/router\.ts'/);
    assert.match(tree, /import type \{ createStart \} from '@tanstack\/react-start'/);
    assert.match(tree, /declare module '@tanstack\/react-start'/);
    assert.match(tree, /router: Awaited<ReturnType<typeof getRouter>>/);
    assert.doesNotMatch(tree, /startInstance/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

maybe("the footer names the router entry the app declared", () => {
  const dir = app("declared", {
    pkg: { imports: { "#tanstack-router-entry": "./src/lib/router.ts" } },
  });
  try {
    mkdirSync(join(dir, "src", "lib"), { recursive: true });
    writeFileSync(join(dir, "src", "lib", "router.ts"), "export function getRouter() {}\n");
    generate(dir);
    const tree = readFileSync(join(dir, "src", "routeTree.gen.ts"), "utf8");
    assert.match(tree, /import type \{ getRouter \} from '\.\/lib\/router\.ts'/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// With a start entry the framework writes this exact block; a tree that differs
// from it is rewritten by the framework's own generator on the next run.
maybe("an app with a start entry gets the framework's startInstance footer", () => {
  const dir = app("start");
  try {
    writeFileSync(join(dir, "src", "router.tsx"), "export function getRouter() {}\n");
    writeFileSync(join(dir, "src", "start.ts"), "export const startInstance = undefined;\n");
    generate(dir);
    const tree = readFileSync(join(dir, "src", "routeTree.gen.ts"), "utf8");
    assert.ok(
      tree.endsWith(
        [
          "import type { getRouter } from './router.tsx'",
          "import type { startInstance } from './start.ts'",
          "declare module '@tanstack/react-start' {",
          "  interface Register {",
          "    ssr: true",
          "    router: Awaited<ReturnType<typeof getRouter>>",
          "    config: Awaited<ReturnType<typeof startInstance.getOptions>>",
          "  }",
          "}",
        ].join("\n") + "\n",
      ),
      tree.slice(-600),
    );
    assert.doesNotMatch(tree, /createStart/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// The footer start-plugin-core itself builds, against the installed package's
// internals: a TanStack upgrade that changes the footer or the start-entry
// lookup fails here instead of flipping committed route trees under `oj dev`.
// A moved internal file fails too, on purpose.
const spc = join(fixture, "node_modules/@tanstack/start-plugin-core/dist/esm");
async function upstreamFooter(dir, generatedRouteTree) {
  const { buildRouteTreeFileFooterFromConfig } = await import(join(spc, "start-router-plugin/route-tree-footer.js"));
  const { resolveStartEntryPlan } = await import(join(spc, "planning.js"));
  const plan = resolveStartEntryPlan({
    root: dir,
    startConfig: { srcDirectory: "src", start: {}, router: {}, client: {}, server: {} },
    defaultEntryPaths: {},
  });
  return buildRouteTreeFileFooterFromConfig({
    generatedRouteTreePath: join(dir, generatedRouteTree),
    corePluginOpts: { framework: "react" },
    getConfig: () => ({ startConfig: { router: {} }, resolvedStartConfig: plan }),
  }).join("\n");
}

const LAYOUTS = {
  "no start entry": {},
  "src/start.ts": { "src/start.ts": "" },
  "src/start.tsx": { "src/start.tsx": "" },
  ".js outranks .tsx": { "src/start.tsx": "", "src/start.js": "" },
  "src/start.mts": { "src/start.mts": "" },
  "a directory index is not an entry": { "src/start/index.ts": "" },
  "src/router.ts": { "src/start.ts": "", "src/router.ts": "export function getRouter() {}\n", "src/router.tsx": null },
  "a nested generatedRouteTree": {
    "src/start.ts": "",
    "src/app/.keep": "",
    "tsr.config.json": JSON.stringify({ generatedRouteTree: "./src/app/routeTree.gen.ts" }),
  },
};

for (const [name, files] of Object.entries(LAYOUTS)) {
  maybe(`the footer matches start-plugin-core's: ${name}`, async () => {
    const dir = realpathSync(app("upstream"));
    try {
      const all = { "src/router.tsx": "export function getRouter() {}\n", ...files };
      for (const [f, body] of Object.entries(all)) {
        if (body === null) continue;
        mkdirSync(dirname(join(dir, f)), { recursive: true });
        writeFileSync(join(dir, f), body);
      }
      const treePath = files["tsr.config.json"]
        ? JSON.parse(files["tsr.config.json"]).generatedRouteTree
        : "./src/routeTree.gen.ts";
      generate(dir);
      const tree = readFileSync(join(dir, treePath), "utf8");
      const want = await upstreamFooter(dir, treePath);
      assert.ok(tree.trimEnd().endsWith(want), `want tail:\n${want}\n\ngot tail:\n${tree.slice(-600)}`);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
}
