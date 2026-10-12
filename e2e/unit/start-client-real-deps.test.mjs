// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { cpSync, existsSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const fixture = fileURLToPath(new URL("../fixtures/solid-start-app/", import.meta.url));
const assets = fileURLToPath(new URL("../../crates/oj_server/src/assets/start/", import.meta.url));

test("installed Solid Start lifecycle and client bundle preserve split components and one entry URL", {
  skip: !existsSync(join(fixture, "node_modules/@tanstack/solid-start/package.json")) && "install Solid Start fixture dependencies",
}, () => {
  const root = mkdtempSync(join(tmpdir(), "oj-client-real-deps-"));
  try {
    for (const name of ["src", "public", "package.json", "tsconfig.json"]) cpSync(join(fixture, name), join(root, name), { recursive: true });
    symlinkSync(join(fixture, "node_modules"), join(root, "node_modules"));
    const embedded = join(root, "embedded");
    cpSync(assets, embedded, { recursive: true });
    writeFileSync(join(root, "vite.config.mjs"), `
      import { tanstackStart } from '@tanstack/solid-start/plugin/vite';
      import solid from 'vite-plugin-solid';
      export default {
        environments: { client: { define: { INITIAL: '1' } } },
        plugins: [tanstackStart(), solid({ ssr: true }), {
          name: 'merge-probe',
          config() { return { environments: { client: { define: { CONFIG: '2' } } } }; },
          configEnvironment(name) { if (name === 'client') return { define: { ENVIRONMENT: '3' } }; }
        }]
      };
    `);
    writeFileSync(join(root, "verify.mjs"), `
      import assert from 'node:assert/strict';
      import { readFileSync } from 'node:fs';
      import { loadPluginContainer } from './embedded/vite-plugin-bridge.mjs';
      import { run } from './embedded/bundle-client.mjs';
      const container = await loadPluginContainer(process.cwd(), { routerTransforms: true, environment: 'client' });
      assert.equal(container.config.appType, 'custom');
      assert.ok(container.config.environments.ssr);
      assert.deepEqual(container.config.environments.client.define, { INITIAL: '1', CONFIG: '2', ENVIRONMENT: '3' });
      await container.buildStart();
      const id = process.cwd() + '/src/routes/index.tsx';
      const source = readFileSync(id, 'utf8');
      assert.equal(await container.load(process.cwd() + '/src/components/SolidStartHero.tsx'), null);
      assert.match(await container.load(process.cwd() + '/src/routeTree.gen.ts'), /routeTree/);
      const reference = await container.transformUserCode(source, id);
      assert.match(reference, /lazyRouteComponent/);
      assert.match(reference, /tsr-split=component/);
      const split = await container.transformUserCode(source, id + '?tsr-split=component');
      assert.match(split, /export.*component/s);
      assert.doesNotMatch(split, /createFileRoute/);
      assert.match(split, /<main>/);
      await run();
    `);
    const result = spawnSync(process.execPath, [join(root, "verify.mjs")], {
      cwd: root,
      encoding: "utf8",
      timeout: 30000,
      env: {
        ...process.env,
        OJ_APP_ROOT: root,
        OJ_START_PKG: "@tanstack/solid-start",
        OJ_ROUTER_TARGET: "solid",
        OJ_CLIENT_ENTRY: "client-entry-solid.tsx",
        OJ_VENDORED_ROLLDOWN: "",
        NODE_ENV: "development",
      },
    });
    assert.equal(result.status, 0, `${result.error ?? ""}\n${result.stdout}\n${result.stderr}`);
    const index = JSON.parse(readFileSync(join(embedded, "client-chunks.json"), "utf8"));
    assert.equal(index.entry, "client-entry.js");
    const entry = readFileSync(join(embedded, "client-chunks", index.entry), "utf8");
    assert.match(entry, /component: lazyRouteComponent\(\$\$splitComponentImporter/);
    const chunks = index.files.filter(({ name }) => name !== index.entry).map(({ name }) => readFileSync(join(embedded, "client-chunks", name), "utf8"));
    const home = chunks.find((code) => code.includes("solid-start-home-marker"));
    assert.ok(home, "home component must be split out of the entry");
    assert.match(home, /export \{ Index as component \}/);
    assert.match(home, /getNextElement/);
    assert.doesNotMatch(home, /return\s*\(?\s*<main>/);
    assert.match(home, /from "\.\/client-entry\.js"/);
    assert.ok(chunks.every((code) => !code.includes('from "./client-entry-solid.js"')));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
