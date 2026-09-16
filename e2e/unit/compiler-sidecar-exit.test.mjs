// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const assets = fileURLToPath(new URL("../../crates/oj_server/src/assets/", import.meta.url));

// Compiler stubs keep this a transport test with no package installation needed.
// PostCSS completes asynchronously, after stdin has already closed.
function fixtures(base) {
  writeFileSync(join(base, "package.json"), "{}");
  writeFileSync(join(base, "postcss.config.mjs"), "export default { plugins: [] };\n");
  for (const [name, source] of Object.entries({
    postcss: `module.exports = () => ({
      async process(css) {
        await new Promise(resolve => setTimeout(resolve, 25));
        return { css };
      }
    });`,
    svelte: "exports.compile = css => ({ js: { code: css } });",
  })) {
    const dir = join(base, "node_modules", name);
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "package.json"), JSON.stringify({
      name, main: "index.cjs", exports: { ".": "./index.cjs", "./compiler": "./index.cjs" },
    }));
    writeFileSync(join(dir, "index.cjs"), source);
  }
}

for (const script of ["tailwind-sidecar.mjs", "css-preprocess.mjs", "svelte-compile.mjs"]) {
  test(`${script} drains large responses after stdin closes`, { skip: process.platform === "win32" }, () => {
    const base = mkdtempSync(join(tmpdir(), "oj-sidecar-exit-"));
    try {
      fixtures(base);
      const css = "/*" + "x".repeat(4 * 1024 * 1024) + "*/\n.a { color: red; }\n";
      const requests = [css, ".b { color: blue; }\n"].map((css, i) => ({
        id: i + 1, base, from: join(base, "input.css"), css,
      }));
      // Node spawn's 'pipe' streams can be sockets. A shell pipeline gives the
      // sidecar actual FIFO stdin/stdout, as Rust does, and delays the reader
      // to exercise backpressure. pipefail preserves the sidecar's exit status.
      const result = spawnSync("bash", [
        "-o", "pipefail", "-c", 'cat | "$1" "$2" | { sleep 0.3; cat; }',
        "sidecar-test", process.execPath, join(assets, script),
      ], {
        input: requests.map(request => JSON.stringify(request) + "\n").join(""),
        encoding: "utf8",
        env: { ...process.env, OJ_POSTCSS_CONFIG: join(base, "postcss.config.mjs") },
        timeout: 10000,
        maxBuffer: 16 * 1024 * 1024,
      });
      assert.ifError(result.error);
      assert.equal(result.status, 0, result.stderr);
      assert.ok(result.stdout.length > 0, "sidecar exited without responding");
      const responses = result.stdout.trim().split("\n").map(line => JSON.parse(line));
      assert.equal(responses.length, requests.length);
      for (const request of requests) {
        const response = responses.find(response => response.id === request.id);
        assert.ok(response, `missing response ${request.id}`);
        assert.equal(response.css?.length, request.css.length, response.error);
        assert.equal(response.css, request.css);
      }
    } finally {
      rmSync(base, { recursive: true, force: true });
    }
  });
}
