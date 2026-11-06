// SPDX-License-Identifier: MIT

// The route generator's configuration surface. `@tanstack/router-generator`
// reads tsr.config.json and merges it as { ...file, ...inline }, so every key
// the adapter passes inline is a key the app cannot set. Start passes its
// routes directory and route tree inline (from tanstackStart's `srcDirectory`
// and `router`), so tsr.config.json never moves them; oj passes the same two.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { asset, repo } from "./harness.mjs";

const fixture = join(repo, "e2e/fixtures/start-app");
const installed = existsSync(join(fixture, "node_modules/@tanstack/router-generator"));
const maybe = installed ? test : test.skip;

// The generator resolves @tanstack/router-generator from the app root, so the
// app needs a node_modules; everything else about the fixture is irrelevant here.
function app(label, { tsrConfig } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "oj-routegen-" + label + "-"));
  mkdirSync(join(dir, "src", "routes"), { recursive: true });
  symlinkSync(join(fixture, "node_modules"), join(dir, "node_modules"), "dir");
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "app", type: "module" }));
  writeFileSync(
    join(dir, "src", "routes", "__root.tsx"),
    'import { createRootRoute } from "@tanstack/react-router";\n' + "export const Route = createRootRoute();\n",
  );
  writeFileSync(
    join(dir, "src", "routes", "index.tsx"),
    'import { createFileRoute } from "@tanstack/react-router";\n' + 'export const Route = createFileRoute("/")({});\n',
  );
  if (tsrConfig) writeFileSync(join(dir, "tsr.config.json"), JSON.stringify(tsrConfig));
  return dir;
}

const generate = (dir, env = {}) =>
  execFileSync(process.execPath, [asset("start/generate.mjs")], {
    env: { ...process.env, OJ_APP_ROOT: dir, ...env },
    stdio: ["ignore", "pipe", "pipe"],
  });

maybe("the route tree lands at the default path when the app configures nothing", () => {
  const dir = app("default");
  try {
    generate(dir);
    assert.ok(existsSync(join(dir, "src", "routeTree.gen.ts")));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

maybe("tsr.config.json does not move the route tree, as under Start", () => {
  const dir = app("tsr-config", {
    tsrConfig: { generatedRouteTree: "./src/routes/routeTree.gen.ts" },
  });
  try {
    generate(dir);
    assert.ok(existsSync(join(dir, "src", "routeTree.gen.ts")), "no tree at Start's default path");
    assert.ok(
      !existsSync(join(dir, "src", "routes", "routeTree.gen.ts")),
      "tsr.config.json moved the tree, which Start never lets it do",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

maybe("the paths oj passes for Start decide where the routes and the tree are", () => {
  const dir = app("start-paths", {
    tsrConfig: { generatedRouteTree: "./src/elsewhere.gen.ts" },
  });
  try {
    generate(dir, {
      OJ_ROUTES_DIRECTORY: join(dir, "src", "routes"),
      OJ_GENERATED_ROUTE_TREE: join(dir, "src", "routes.gen.ts"),
    });
    assert.ok(existsSync(join(dir, "src", "routes.gen.ts")), "the tree is not where Start puts it");
    assert.ok(!existsSync(join(dir, "src", "routeTree.gen.ts")), "a second tree at the default path");
    assert.ok(!existsSync(join(dir, "src", "elsewhere.gen.ts")), "tsr.config.json's path was used");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
