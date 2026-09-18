// SPDX-License-Identifier: MIT
//
// The Start one-shots (client bundle, production build) run rolldown on oj's
// EMBEDDED engine, not on spawned node: rolldown's napi binding must dlopen
// against the Node-API symbols the oj binary exports (crates/oj/build.rs).
// This probe drives the real binary's internal `js-eval` through a script that
// imports rolldown from the Start fixture's node_modules and bundles a tiny
// virtual graph. It is the canary for napi-symbol export regressions (a
// deno_napi bump, a linker-flag change): everything else in the Start
// pipeline fails confusingly when this breaks.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
const oj = join(repo, "target", "debug", "oj");
const fixture = join(repo, "e2e", "fixtures", "start-app");

const installed =
  fs.existsSync(oj) && fs.existsSync(join(fixture, "node_modules", "rolldown"));
const maybe = installed ? test : test.skip;

maybe("rolldown's napi binding loads and bundles under the embedded engine", () => {
  const dir = mkdtempSync(join(tmpdir(), "oj-rolldown-probe-"));
  try {
    const probe = join(dir, "probe.mjs");
    writeFileSync(probe, [
      // Resolved from the fixture root (the engine root's node_modules).
      `const { rolldown } = await import(${JSON.stringify(join(fixture, "node_modules", "rolldown", "dist", "index.mjs"))});`,
      "const bundle = await rolldown({",
      '  input: "entry.js",',
      "  plugins: [{",
      '    name: "virtual",',
      '    resolveId: (id) => (id === "entry.js" || id === "dep.js" ? "\\0" + id : null),',
      "    load(id) {",
      '      if (id === "\\0entry.js") return "import { x } from \'dep.js\'; export default x + 1;";',
      '      if (id === "\\0dep.js") return "export const x = 41;";',
      "    },",
      "  }],",
      "});",
      'const { output } = await bundle.generate({ format: "esm" });',
      "export default { code: output[0].code };",
    ].join("\n"));
    const out = execFileSync(oj, ["js-eval", probe, "--root", fixture], {
      encoding: "utf8",
      timeout: 120_000,
    });
    const result = JSON.parse(out);
    // 41 + 1, folded or not: the bundle evaluated the virtual graph.
    assert.match(result.code, /41 \+ 1|42/, `bundled code:\n${result.code}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
