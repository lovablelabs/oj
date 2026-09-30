// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A monorepo app imports a workspace package that is symlinked into its
// node_modules and lives OUTSIDE the app root (`packages/shared`, served through
// /@fs). Editing that package must reach the page like any app source: Vite
// watches every served file outside the root. The watcher here watches the
// root's entries, so the linked package has to be watched as well.
// Run with a built target/debug/oj (or OJ_BIN).
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

const mono = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oj-linked-pkg-")));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(mono, rel)), { recursive: true });
  fs.writeFileSync(path.join(mono, rel), text);
};

write("package.json", JSON.stringify({ name: "mono", private: true, workspaces: ["app", "packages/*"] }));
write(
  "packages/shared/package.json",
  JSON.stringify({ name: "@e2e/shared", version: "1.0.0", type: "module", main: "src/index.ts" }),
);
const SHARED = path.join(mono, "packages", "shared", "src", "index.ts");
write("packages/shared/src/index.ts", `export const label: string = "v1";\n`);

write("app/package.json", JSON.stringify({ name: "app", version: "1.0.0", type: "module" }));
write(
  "app/index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.ts"></script></body></html>`,
);
write("app/src/main.ts", `import { label } from "@e2e/shared";\ndocument.title = label;\n`);
fs.mkdirSync(path.join(mono, "app", "node_modules", "@e2e"), { recursive: true });
fs.symlinkSync("../../../packages/shared", path.join(mono, "app", "node_modules", "@e2e", "shared"));

const port = 5232;
const app = path.join(mono, "app");
const server = spawn(oj, ["dev", app, "--port", String(port), "--host=127.0.0.1"], {
  stdio: ["ignore", "pipe", "pipe"],
});
let log = "";
server.stdout.on("data", (d) => (log += d));
server.stderr.on("data", (d) => (log += d));
let failed = false;
try {
  const base = `http://127.0.0.1:${port}`;
  await waitUp(`${base}/`);
  // The entry imports the linked package through /@fs.
  const main = await (await fetch(`${base}/src/main.ts`)).text();
  const sharedUrl = `/@fs${SHARED}`;
  assert.ok(main.includes(sharedUrl), `main.ts does not import the linked package via ${sharedUrl}:\n${main}`);
  assert.match(await (await fetch(`${base}${sharedUrl}`)).text(), /"v1"/);

  const ws = new WebSocket(`ws://127.0.0.1:${port}/__ws`, "vite-hmr");
  const frames = [];
  ws.addEventListener("message", (e) => {
    try {
      frames.push(JSON.parse(e.data));
    } catch {}
  });
  await new Promise((res, rej) => {
    ws.addEventListener("open", res);
    ws.addEventListener("error", rej);
  });

  fs.writeFileSync(SHARED, `export const label: string = "v2";\n`);
  const deadline = Date.now() + 10000;
  const reached = () =>
    frames.find(
      (m) =>
        m.type === "full-reload" ||
        (m.type === "update" && (m.updates ?? []).some((u) => String(u.path ?? u.acceptedPath).includes("/src/"))),
    );
  while (!reached() && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
  ws.close();
  assert.ok(reached(), `no HMR frame after editing the linked package; frames: ${JSON.stringify(frames)}`);
  assert.match(await (await fetch(`${base}${sharedUrl}?t=${Date.now()}`)).text(), /"v2"/);
  console.log("hmr-linked-workspace-package: ok");
} catch (err) {
  failed = true;
  console.error(log.slice(-4000));
  console.error("hmr-linked-workspace-package FAILED:", err && err.stack ? err.stack : err);
} finally {
  server.kill("SIGKILL");
  fs.rmSync(mono, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
