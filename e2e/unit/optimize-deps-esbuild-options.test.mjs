// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { runSidecar, tmpProject, testWithRolldown, linkRolldown } from "./harness.mjs";

const it = testWithRolldown(test);

// `optimizeDeps.esbuildOptions` is deprecated but still honored, the way Vite 8
// maps it onto the rolldown run. Concatenate every emitted chunk so assertions
// don't depend on which chunk the code landed in.
const emittedBundleText = (outDir) =>
  fs
    .readdirSync(outDir)
    .filter((f) => f.endsWith(".mjs"))
    .map((f) => fs.readFileSync(path.join(outDir, f), "utf8"))
    .join("\n");

it("optimize-deps applies user esbuildOptions (define + target) to the pre-bundle", () => {
  const fx = tmpProject({ prefix: "oj-esbopts-" });
  linkRolldown(fx.root);
  try {
    // The dep reads a define'd global and uses ES2016 `**` on a non-constant
    // operand (so the bundler can't fold it away), letting us prove both `define`
    // substitution and `target` down-leveling reached the emitted bytes.
    fx.pkg("flagdep", "index.js", {
      "index.js": `export const flag = __OJ_FLAG__;\nexport const pow = globalThis.__oj_base__ ** 10;\n`,
    });
    fx.write("entry.js", `import { flag, pow } from "flagdep";\nexport const out = flag + pow;\n`);

    const outDir = path.join(fx.root, ".oj-cache", "deps");
    const { metadata } = runSidecar("optimize-deps.mjs", {
      root: fx.root,
      outDir,
      entries: [path.join(fx.root, "entry.js")],
      include: ["flagdep"],
      esbuildOptions: {
        define: { __OJ_FLAG__: JSON.stringify("injected-by-define") },
        target: "es2015",
      },
    });

    assert.deepEqual(Object.keys(metadata), ["flagdep"], "the included dep is pre-bundled");
    const code = emittedBundleText(outDir);

    // define took effect: the global was replaced with the configured literal.
    assert.match(code, /injected-by-define/, `define not applied:\n${code}`);
    assert.doesNotMatch(code, /__OJ_FLAG__/, "the define'd global should be gone");

    // target took effect: for es2015 the ES2016 `**` operator is lowered away.
    assert.doesNotMatch(code, /\*\* 10/, `target es2015 should lower the ** operator:\n${code}`);
  } finally {
    fx.cleanup();
  }
});

it("optimize-deps keeps its NODE_ENV define when the user adds their own", () => {
  const fx = tmpProject({ prefix: "oj-esbopts2-" });
  linkRolldown(fx.root);
  try {
    fx.pkg("envdep", "index.js", {
      "index.js": `export const mode = process.env.NODE_ENV;\nexport const extra = __EXTRA__;\n`,
    });
    fx.write("entry.js", `import { mode, extra } from "envdep";\nexport const out = mode + extra;\n`);

    const outDir = path.join(fx.root, ".oj-cache", "deps");
    runSidecar("optimize-deps.mjs", {
      root: fx.root,
      outDir,
      entries: [path.join(fx.root, "entry.js")],
      include: ["envdep"],
      esbuildOptions: { define: { __EXTRA__: JSON.stringify("extra-val") } },
    });

    const code = emittedBundleText(outDir);
    // The user's define is merged over oj's NODE_ENV base; both apply.
    assert.match(code, /"development"/, `oj's NODE_ENV define should survive:\n${code}`);
    assert.doesNotMatch(code, /process\.env\.NODE_ENV/, "NODE_ENV should be substituted, not left as a lookup");
    assert.match(code, /extra-val/, "the user's define also applied");
  } finally {
    fx.cleanup();
  }
});

it("optimize-deps drops non-esbuild option keys instead of crashing the pre-bundle", () => {
  const fx = tmpProject({ prefix: "oj-esbopts3-" });
  linkRolldown(fx.root);
  try {
    fx.pkg("mixdep", "index.js", { "index.js": `export const v = __MIX__;\n` });
    fx.write("entry.js", `import { v } from "mixdep";\nexport const out = v;\n`);

    const outDir = path.join(fx.root, ".oj-cache", "deps");
    // Foreign option keys (rolldown-shaped or bogus) must be filtered out of
    // the esbuildOptions compat channel while a valid `define` still applies.
    const { metadata } = runSidecar("optimize-deps.mjs", {
      root: fx.root,
      outDir,
      entries: [path.join(fx.root, "entry.js")],
      include: ["mixdep"],
      esbuildOptions: {
        output: { format: "cjs" },
        resolve: { conditionNames: ["x"] },
        transform: { define: { nope: "1" } },
        input: "should-be-ignored",
        define: { __MIX__: JSON.stringify("mixed-ok") },
      },
    });

    assert.deepEqual(Object.keys(metadata), ["mixdep"], "dep still pre-bundled despite foreign option keys");
    assert.match(emittedBundleText(outDir), /mixed-ok/, "the valid esbuild define still applied");
  } finally {
    fx.cleanup();
  }
});
