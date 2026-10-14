// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// build.cssMinify and build.cssTarget (Vite build.ts): cssMinify defaults to
// build.minify and can be set on its own; cssTarget defaults to build.target
// and decides what lightningcss lowers (CSS nesting here). css.lightningcss
// is spread into the build minify (css.ts minifyCSS), the only lightningcss
// pass under Vite's default transformer. Run with a built target/debug/oj.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

const NESTED = `.a {\n  color: red;\n  .b { color: blue }\n}\n`;

function buildCss(config, source = NESTED) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-css-options-"));
  try {
    fs.mkdirSync(path.join(app, "src"));
    fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "css-options", type: "module" }));
    fs.writeFileSync(path.join(app, "oj.config.ts"), config);
    fs.writeFileSync(
      path.join(app, "index.html"),
      `<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>`,
    );
    fs.writeFileSync(path.join(app, "src", "main.js"), `import "./app.css";\n`);
    fs.writeFileSync(path.join(app, "src", "app.css"), source);
    const r = spawnSync(oj, ["build", app], { cwd: repo, encoding: "utf8" });
    if (r.status !== 0) throw new Error(`build failed:\n${r.stdout}\n${r.stderr}`);
    const assets = path.join(app, "dist", "assets");
    const css = fs.readdirSync(assets).filter((f) => f.endsWith(".css"));
    if (css.length !== 1) throw new Error(`expected one stylesheet, got ${css.join(", ")}`);
    return fs.readFileSync(path.join(assets, css[0]), "utf8");
  } finally {
    fs.rmSync(app, { recursive: true, force: true });
  }
}

let failed = false;
function check(label, ok, detail) {
  if (!ok) {
    failed = true;
    console.error(`FAIL ${label}: ${detail}`);
  } else {
    console.log(`ok   ${label}`);
  }
}

const dflt = buildCss(`export default {};\n`);
check("default: minified", !/\n\s+color/.test(dflt) && /\.a{color:red}/.test(dflt), JSON.stringify(dflt));
check(
  "default: nesting lowered for the baseline target",
  /\.a \.b\s*{/.test(dflt) || /\.a \.b{/.test(dflt),
  JSON.stringify(dflt),
);

const noMinify = buildCss(`export default { build: { minify: false } };\n`);
check(
  "build.minify false leaves CSS unminified",
  /\n/.test(noMinify.trim()) && /color: red/.test(noMinify),
  JSON.stringify(noMinify),
);

const cssMinifyOnly = buildCss(`export default { build: { minify: false, cssMinify: true } };\n`);
check(
  "cssMinify true minifies even with build.minify false",
  /\.a{color:red}/.test(cssMinifyOnly),
  JSON.stringify(cssMinifyOnly),
);

const cssMinifyOff = buildCss(`export default { build: { cssMinify: false } };\n`);
check(
  "cssMinify false keeps CSS readable with build.minify on",
  /color: red/.test(cssMinifyOff),
  JSON.stringify(cssMinifyOff),
);

const modern = buildCss(`export default { build: { cssTarget: ["chrome120", "safari17.2", "firefox117"] } };\n`);
check(
  "cssTarget with nesting support keeps nesting",
  !/\.a \.b/.test(modern) && /\.b{/.test(modern),
  JSON.stringify(modern),
);

const viaTarget = buildCss(`export default { build: { target: ["chrome120", "safari17.2", "firefox117"] } };\n`);
check("cssTarget defaults to build.target", !/\.a \.b/.test(viaTarget), JSON.stringify(viaTarget));

const LIGHTNING_SRC = `.kept { color: red }\n.gone { color: blue }\n.c:hover { color: green }\n`;
const LIGHTNING = `css: { lightningcss: { unusedSymbols: ["gone"], pseudoClasses: { hover: "is-hover" } } }`;
const lightning = buildCss(`export default { ${LIGHTNING} };\n`, LIGHTNING_SRC);
check(
  "css.lightningcss applies to the build minify",
  /\.kept{/.test(lightning) && !/\.gone/.test(lightning) && /\.c\.is-hover{/.test(lightning),
  JSON.stringify(lightning),
);
const lightningUnminified = buildCss(`export default { build: { minify: false }, ${LIGHTNING} };\n`, LIGHTNING_SRC);
check(
  "css.lightningcss stays out of an unminified build",
  /\.gone/.test(lightningUnminified) && /:hover/.test(lightningUnminified),
  JSON.stringify(lightningUnminified),
);

if (failed) process.exit(1);
console.log("BUILD-CSS-OPTIONS E2E PASSED");
