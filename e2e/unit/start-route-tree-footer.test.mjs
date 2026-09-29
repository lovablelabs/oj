// SPDX-License-Identifier: MIT

// The `declare module` block the framework's Vite plugin appends to every
// generated route tree (moduleDeclaration in start-plugin-core's
// start-router-plugin). The app's Register interface is keyed off it, and so is
// every createFileRoute in the app -- so a tree generated without it is a
// different file from the one the framework's own tooling produces.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
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
