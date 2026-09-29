// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite ALWAYS resolves `server.fs` (resolveServerOptions): `strict: true`, the
// default deny list, and `allow` defaulting to the workspace root with every
// entry absolute. Plugins index into `config.server.fs.allow` inside
// resolveId-time checks (isServeableFile shapes, run per `?url` import), so a
// resolved config without it fails the whole client bundle with
// "Cannot read properties of undefined (reading 'some')".
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo, rpcSidecar, tmpProject } from "./harness.mjs";

const bridge = await import(
  pathToFileURL(join(repo, "crates/oj_server/src/assets/start/vite-plugin-bridge.mjs")).href
);

const VITE_DENY = [".env", ".env.*", "*.{crt,pem,key,p12,pfx,cer,der}", ".npmrc", ".yarnrc.yml", "**/.git/**"];

test("bundling container: resolveId reading server.fs.allow.some() survives and sees Vite defaults", async () => {
  // A workspace: pnpm-workspace.yaml at the root, the app one level down —
  // allow must default to the WORKSPACE root, not the app root.
  const ws = fs.mkdtempSync(path.join(os.tmpdir(), "oj-fs-ws-"));
  const app = path.join(ws, "web");
  fs.mkdirSync(app, { recursive: true });
  fs.writeFileSync(path.join(ws, "pnpm-workspace.yaml"), "packages:\n  - web\n");
  fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "web" }));

  try {
  let seenConfig = null;
  let seenAllow = null;
  const plugins = [
    {
      name: "fs-allow-reader",
      configResolved(config) {
        seenConfig = config.server.fs;
      },
      resolveId(id) {
        if (!id.endsWith("?url")) return null;
        // The incident shape: an allow-list membership check per ?url import.
        const allowed = this.environment.config.server.fs.allow.some((dir) =>
          id.startsWith(path.resolve(dir)),
        );
        seenAllow = { allowed, allow: this.environment.config.server.fs.allow };
        return null;
      },
    },
  ];
  const container = bridge.createPluginContainer({}, plugins, {
    command: "serve",
    environment: "client",
    config: { root: app },
  });
  await container.resolveId(path.join(app, "src/a.svg") + "?url", undefined);

  assert.ok(seenAllow, "resolveId ran without throwing on server.fs.allow");
  assert.equal(seenAllow.allowed, true, "a file under the workspace root is allowed");
  assert.deepEqual(seenAllow.allow, [ws], "allow defaults to the workspace root");
  assert.equal(seenConfig.strict, true);
  assert.deepEqual(seenConfig.deny, VITE_DENY);
  } finally {
    fs.rmSync(ws, { recursive: true, force: true });
  }
});

test("bundling container: a user allow list is kept, resolved absolute", async () => {
  let seen = null;
  const plugins = [
    {
      name: "reader",
      configResolved(config) {
        seen = config.server.fs;
      },
    },
  ];
  const container = bridge.createPluginContainer({}, plugins, {
    command: "serve",
    environment: "client",
    config: { root: repo, server: { fs: { allow: ["..", "/abs"], strict: false } } },
  });
  await container.resolveId("virtual:probe", undefined);
  assert.deepEqual(seen.allow, [path.resolve(repo, ".."), "/abs"]);
  assert.equal(seen.strict, false, "an explicit strict:false is kept");
  assert.deepEqual(seen.deny, VITE_DENY, "deny still gets the Vite default");
});

test("bundling container: an EXPLICIT empty allow list stays empty (Vite's ?? semantics)", async () => {
  let seen = null;
  const container = bridge.createPluginContainer({}, [
    { name: "reader", configResolved(config) { seen = config.server.fs; } },
  ], {
    command: "serve",
    environment: "client",
    config: { root: repo, server: { fs: { allow: [] } } },
  });
  await container.resolveId("virtual:probe", undefined);
  assert.deepEqual(seen.allow, [], "allow: [] must not be broadened to the workspace root");
});

test("dev plugin host: configResolved sees the resolved server.fs", async () => {
  const fx = tmpProject({ prefix: "oj-fs-host-" });
  fx.write(
    "oj.plugins.mjs",
    `let seen = {};
     export default [{
       name: "fs-reader",
       configResolved(config) {
         seen = {
           strict: config.server.fs?.strict,
           allow: config.server.fs?.allow,
           denyHasEnv: (config.server.fs?.deny ?? []).includes(".env"),
         };
       },
       transform(code, id) {
         if (id.endsWith("probe.js")) return "export default " + JSON.stringify(seen) + ";";
         return null;
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "transform",
      args: ["", path.join(fx.root, "probe.js")],
    });
    const seen = JSON.parse(JSON.parse(res.result).code.replace(/^export default /, "").replace(/;$/, ""));
    assert.equal(seen.strict, true);
    assert.equal(seen.denyHasEnv, true);
    assert.ok(Array.isArray(seen.allow) && seen.allow.length > 0, `allow present: ${JSON.stringify(seen)}`);
    assert.ok(path.isAbsolute(seen.allow[0]), "allow entries are absolute");
  } finally {
    host.close();
    fx.cleanup();
  }
});
