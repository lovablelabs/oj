// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A request path is percent-encoded, so a file with a space or a non-ASCII
// character in its name arrives as `%20`/`%C3%A9`. Serving it means decoding the
// path -- and decoding means the traversal guard has to run on the decoded form,
// not the raw one. Both halves are checked here against a real dev server, in
// dev and in preview.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-awkward-"));
const outside = fs.mkdtempSync(path.join(os.tmpdir(), "oj-outside-"));
const secret = "SECRET-OUTSIDE-THE-APP";
fs.writeFileSync(path.join(outside, "secret.txt"), secret);

fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "awkward-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "Cool Button.tsx"), `export const label = "spaced-module-loaded";\n`);
fs.writeFileSync(path.join(app, "src", "café.css"), `.unicode-name { color: rgb(1, 2, 3) }\n`);
fs.writeFileSync(path.join(app, "src", "100%.txt"), `percent-in-the-name\n`);
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { label } from "./Cool Button";\nimport "./café.css";\nwindow.__LABEL = label;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.tsx"></script></body></html>`,
);

async function get(port, urlPath) {
  const res = await fetch(`http://127.0.0.1:${port}${urlPath}`, { signal: AbortSignal.timeout(5000) });
  return { status: res.status, body: await res.text() };
}

async function reachable(port) {
  return waitUp(`http://127.0.0.1:${port}/`, { init: () => ({ signal: AbortSignal.timeout(1500) }) }).then(
    () => true,
    () => false,
  );
}

// Every spelling of "read a file outside the app" that a browser or a script
// can put on the wire.
const traversals = [
  "/../../../../etc/passwd",
  "/%2e%2e/%2e%2e/%2e%2e/%2e%2e/etc/passwd",
  "/%2E%2E%2F%2E%2E%2Fetc/passwd",
  "/src/../../../../etc/passwd",
  `/@fs${path.join(outside, "secret.txt")}`,
  `/@fs${path.join(outside, "secret.txt").replace(/\//g, "%2f")}`,
  `/..${outside}/secret.txt`,
];

let failed = false;
try {
  {
    const srv = spawn(oj, ["dev", app, "--port", "5486"], { stdio: "ignore" });
    try {
      assert.ok(await reachable(5486), "dev server reachable");

      const spaced = await get(5486, "/src/Cool%20Button.tsx");
      assert.equal(spaced.status, 200, "a spaced filename must be served");
      assert.match(spaced.body, /spaced-module-loaded/, "spaced module body");

      const unicode = await get(5486, "/src/caf%C3%A9.css");
      assert.equal(unicode.status, 200, "a non-ASCII filename must be served");
      assert.match(unicode.body, /unicode-name/, "unicode module body");

      const percent = await get(5486, "/src/100%25.txt");
      assert.equal(percent.status, 200, "a percent in a filename must be served");
      assert.match(percent.body, /percent-in-the-name/, "percent module body");

      for (const target of traversals) {
        const res = await get(5486, target);
        assert.ok(!res.body.includes(secret), `dev leaked a file outside the app: ${target}`);
        assert.ok(!res.body.includes("root:"), `dev leaked /etc/passwd: ${target}`);
      }
      console.log("[dev] awkward filenames served, traversal contained OK");
    } finally {
      srv.kill("SIGKILL");
      await sleep(300);
    }
  }

  {
    execSync(`${oj} build ${app}`, { stdio: "ignore" });
    fs.writeFileSync(path.join(app, "dist", "Cool Asset.txt"), "spaced-asset-served\n");
    const srv = spawn(oj, ["preview", app, "--port", "5487"], { stdio: "ignore" });
    try {
      assert.ok(await reachable(5487), "preview server reachable");

      const asset = await get(5487, "/Cool%20Asset.txt");
      assert.equal(asset.status, 200, "preview must serve a spaced filename");
      assert.match(asset.body, /spaced-asset-served/, "spaced asset body");

      for (const target of traversals) {
        const res = await get(5487, target);
        assert.ok(!res.body.includes(secret), `preview leaked a file outside the app: ${target}`);
        assert.ok(!res.body.includes("root:"), `preview leaked /etc/passwd: ${target}`);
      }
      console.log("[preview] awkward filenames served, traversal contained OK");
    } finally {
      srv.kill("SIGKILL");
      await sleep(300);
    }
  }

  console.log("AWKWARD-PATHS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("AWKWARD-PATHS E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(outside, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
