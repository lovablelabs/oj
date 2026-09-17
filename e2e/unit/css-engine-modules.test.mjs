// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The CSS engine modules (loaded in-process by oj's embedded JS engine) are
// plain ES modules exporting `compile(request)`; here they run under Node
// against stub toolchain packages, so the compile flow itself (postcss config
// loading, preprocessor options threading, the missing-package marker) is
// covered without a Rust build or real package installs.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const assets = fileURLToPath(new URL("../../crates/oj_server/src/assets/", import.meta.url));
const load = (name) => import(new URL(name, `file://${assets}`).href);

function writePkg(base, name, source, extra = {}) {
  const dir = join(base, "node_modules", name);
  mkdirSync(dir, { recursive: true });
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify({ name, main: "index.cjs", exports: { ".": "./index.cjs", ...extra } }),
  );
  writeFileSync(join(dir, "index.cjs"), source);
}

test("css-tailwind.mjs runs the postcss config named on the request", async () => {
  const base = mkdtempSync(join(tmpdir(), "oj-css-engine-"));
  try {
    writeFileSync(join(base, "package.json"), "{}");
    const cfg = join(base, "custom.postcss.config.mjs");
    writeFileSync(cfg, "export default { plugins: [] };\n");
    writePkg(
      base,
      "postcss",
      `module.exports = () => ({
        async process(css) {
          await new Promise((resolve) => setTimeout(resolve, 10));
          return { css: css + "/*postcss-ran*/" };
        },
      });`,
    );
    const { compile } = await load("css-tailwind.mjs");
    const css = await compile({
      base,
      css: ".a { color: red }",
      from: join(base, "input.css"),
      postcssConfig: cfg,
    });
    assert.equal(css, ".a { color: red }/*postcss-ran*/");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("css-tailwind.mjs marks a missing tailwind toolchain for the install hint", async () => {
  const base = mkdtempSync(join(tmpdir(), "oj-css-engine-miss-"));
  try {
    writeFileSync(join(base, "package.json"), "{}");
    const { compile } = await load("css-tailwind.mjs");
    await assert.rejects(
      compile({ base, css: '@import "tailwindcss";', from: join(base, "input.css") }),
      /OJ_MISSING_PACKAGE tailwindcss/,
    );
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("css-preprocess.mjs compiles by extension and threads options", async () => {
  const base = mkdtempSync(join(tmpdir(), "oj-css-preprocess-"));
  try {
    writeFileSync(join(base, "package.json"), "{}");
    writePkg(
      base,
      "less",
      `module.exports = {
        FileManager: class {},
        async render(css, opts) {
          return { css: css + "/*less:" + (opts.math || "") + "*/" };
        },
      };`,
    );
    const { compile } = await load("css-preprocess.mjs");
    const out = await compile({
      base,
      css: ".x { width: 1px }",
      from: join(base, "a.less"),
      options: { math: "strict" },
    });
    assert.equal(out, ".x { width: 1px }/*less:strict*/");

    // An extension neither less nor stylus passes through untouched.
    const untouched = await compile({ base, css: ".y {}", from: join(base, "a.css") });
    assert.equal(untouched, ".y {}");

    await assert.rejects(
      compile({ base, css: "d = 1", from: join(base, "b.styl") }),
      /OJ_MISSING_PACKAGE stylus/,
    );
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("svelte-compile.mjs compiles through svelte/compiler with the dev flag", async () => {
  const base = mkdtempSync(join(tmpdir(), "oj-css-svelte-"));
  try {
    writeFileSync(join(base, "package.json"), "{}");
    writePkg(
      base,
      "svelte",
      'exports.compile = (source, opts) => ({ js: { code: source + "/*dev:" + opts.dev + "*/" } });',
      { "./compiler": "./index.cjs" },
    );
    const { compile } = await load("svelte-compile.mjs");
    const js = await compile({ base, css: "<button/>", from: join(base, "App.svelte"), dev: false });
    assert.equal(js, "<button/>/*dev:false*/");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("svelte-compile.mjs marks a missing svelte package for the install hint", async () => {
  const base = mkdtempSync(join(tmpdir(), "oj-css-svelte-miss-"));
  try {
    writeFileSync(join(base, "package.json"), "{}");
    const { compile } = await load("svelte-compile.mjs");
    await assert.rejects(
      compile({ base, css: "<button/>", from: join(base, "App.svelte") }),
      /OJ_MISSING_PACKAGE svelte/,
    );
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
