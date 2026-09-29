// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite's `build` CLI flags and the build options behind them:
// --base, --outDir (alias --out), --assetsDir, --assetsInlineLimit, --minify,
// --sourcemap, --manifest [name], -m/-c short flags, --app (no-op), --watch
// (a clear error). Plus the options those flags reach: the manifest is only
// written when enabled and carries asset/CSS rows and each chunk's `assets`;
// `minify: false` still drops dead code and leaves CSS unminified; `cssMinify`
// is independent of `minify`; `import.meta.hot` is `undefined` in a build;
// unsupported `build.*` options warn instead of being silently ignored.

import { execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const base = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cliflags-"));
const app = path.join(base, "app");
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "cliflags", version: "1.0.0", type: "module" }),
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><link rel="stylesheet" href="/src/linked.css"></head><body><div id="root"></div><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  [
    `import "./style.css";`,
    `import big from "./big.png";`,
    `import { used } from "./lib.js";`,
    `if (import.meta.hot) { console.log("HMR_ONLY_CODE"); }`,
    `function neverCalled() { return "DEAD_CODE_MARKER"; }`,
    `document.getElementById("root").innerHTML = '<img src="' + big + '">' + usedFunction();`,
    `function usedFunction() { return used(); }`,
    `export const lazy = () => import("./lazy.js");`,
    ``,
  ].join("\n"),
);
fs.writeFileSync(
  path.join(app, "src", "lib.js"),
  `export function used() { return "u"; }\nexport function unusedExport() { return "UNUSED_EXPORT_MARKER"; }\n`,
);
fs.writeFileSync(path.join(app, "src", "lazy.js"), `export const lazy = "lazy-chunk-marker";\n`);
fs.writeFileSync(path.join(app, "src", "style.css"), `.a {\n  color: red;\n  margin: 0px;\n}\n`);
fs.writeFileSync(path.join(app, "src", "linked.css"), `body {\n  padding: 0px;\n}\n`);
fs.writeFileSync(path.join(app, "src", "big.png"), Buffer.alloc(9000, 7));

function run(args, config) {
  fs.rmSync(path.join(app, "oj.config.json"), { force: true });
  if (config) fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify(config));
  const r = spawnSync(oj, ["build", app, ...args], { encoding: "utf8" });
  return r;
}
function ok(args, config) {
  const r = run(args, config);
  assert.equal(r.status, 0, `oj build ${args.join(" ")} failed:\n${r.stderr}\n${r.stdout}`);
  return r;
}
function files(dir) {
  const out = [];
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else out.push(path.relative(dir, p).split(path.sep).join("/"));
    }
  };
  walk(dir);
  return out.sort();
}
function readMain(dir, assetsDir = "assets") {
  const js = files(dir).filter((f) => f.startsWith(`${assetsDir}/main-`) && f.endsWith(".js"));
  assert.equal(js.length, 1, `one main chunk under ${assetsDir}/: ${files(dir).join(", ")}`);
  return fs.readFileSync(path.join(dir, js[0]), "utf8");
}
function readCss(dir, assetsDir = "assets") {
  const css = files(dir).filter((f) => f.startsWith(`${assetsDir}/main-`) && f.endsWith(".css"));
  assert.equal(css.length, 1, `one main stylesheet under ${assetsDir}/`);
  return fs.readFileSync(path.join(dir, css[0]), "utf8");
}

let failed = false;
try {
  // 1. Defaults: no manifest (Vite's build.manifest defaults to false), assets/.
  const dist = path.join(app, "dist");
  ok([]);
  assert.ok(!fs.existsSync(path.join(dist, ".vite", "manifest.json")), "manifest is not written unless enabled");
  const main = readMain(dist);
  assert.ok(!main.includes("HMR_ONLY_CODE"), "import.meta.hot is undefined in a build, so HMR-only code is dropped");
  assert.ok(!main.includes("DEAD_CODE_MARKER"), "dead code is dropped");
  assert.ok(!main.includes("usedFunction"), "the default build minifies (mangles) identifiers");
  assert.ok(!readCss(dist).includes("\n  color"), "CSS is minified by default");
  const html0 = fs.readFileSync(path.join(dist, "index.html"), "utf8");
  assert.match(
    html0,
    /<script type="module" src="\/assets\/main-[^"]+\.js" crossorigin>/,
    "entry script carries crossorigin (Vite)",
  );
  assert.match(
    html0,
    /<link rel="stylesheet" href="\/assets\/main-[^"]+\.css" crossorigin \/>/,
    "injected stylesheet link carries crossorigin",
  );
  console.log("ok: defaults (no manifest, minified js + css, import.meta.hot gone, crossorigin tags)");

  // Reporter: every output file with a gzip column; a low chunkSizeWarningLimit
  // triggers Vite's code-splitting hint, reportCompressedSize: false drops the column.
  const rep = ok([], { build: { chunkSizeWarningLimit: 0.1 } });
  assert.match(rep.stdout, /assets\/main-[^ ]+\.js\s+│ gzip: /, `gzip column: ${rep.stdout}`);
  assert.match(rep.stdout, /index\.html/, "html pages are listed too");
  assert.match(rep.stderr, /Some chunks are larger than 0.1 kB after minification/, "chunkSizeWarningLimit warning");
  const quiet = ok([], { build: { chunkSizeWarningLimit: 0.1, minify: false, reportCompressedSize: false } });
  assert.ok(!quiet.stdout.includes("gzip:"), "reportCompressedSize: false drops the gzip column");
  assert.ok(!quiet.stderr.includes("Some chunks are larger"), "no chunk warning without minification (Vite)");
  console.log("ok: reporter gzip column, chunkSizeWarningLimit, reportCompressedSize");

  // 2. Every build flag at once, with --outDir and --base.
  const out1 = path.join(base, "out1");
  ok([
    "--outDir",
    out1,
    "--base",
    "/app/",
    "--assetsDir",
    "static",
    "--minify",
    "false",
    "--sourcemap",
    "inline",
    "--manifest",
    "--assetsInlineLimit",
    "10",
  ]);
  const list = files(out1);
  assert.ok(
    list.some((f) => f.startsWith("static/main-") && f.endsWith(".js")),
    `--assetsDir static: ${list.join(", ")}`,
  );
  assert.ok(!list.some((f) => f.startsWith("assets/")), "nothing lands under the default assets/ dir");
  const html = fs.readFileSync(path.join(out1, "index.html"), "utf8");
  assert.match(html, /src="\/app\/static\/main-[^"]+\.js"/, "--base prefixes the script src");
  assert.match(html, /href="\/app\/static\/linked-[^"]+\.css"/, "--base prefixes the linked stylesheet");
  const main1 = readMain(out1, "static");
  assert.ok(main1.includes("usedFunction"), "--minify false keeps identifiers");
  assert.ok(!main1.includes("DEAD_CODE_MARKER"), "--minify false still drops dead code (Vite's dce-only)");
  assert.ok(!main1.includes("UNUSED_EXPORT_MARKER"), "--minify false still tree-shakes unused exports");
  assert.ok(!main1.includes("HMR_ONLY_CODE"), "import.meta.hot stays undefined without minification");
  assert.ok(main1.includes("sourceMappingURL=data:"), "--sourcemap inline embeds the map");
  assert.ok(readCss(out1, "static").includes("\n  color: red;"), "cssMinify follows minify: false");
  const manifest = JSON.parse(fs.readFileSync(path.join(out1, ".vite", "manifest.json"), "utf8"));
  const entry = manifest["src/main.js"];
  assert.ok(entry && entry.isEntry, "entry row keyed by source path");
  assert.match(entry.file, /^static\/main-/);
  assert.deepEqual(entry.css.length, 1, "entry lists its stylesheet");
  assert.ok(
    Array.isArray(entry.assets) && entry.assets.length === 1 && /^static\/big-.*\.png$/.test(entry.assets[0]),
    `entry.assets lists the imported png: ${JSON.stringify(entry.assets)}`,
  );
  assert.equal(manifest["src/big.png"]?.file, entry.assets[0], "asset row keyed by its original file name");
  assert.equal(manifest["src/big.png"]?.src, "src/big.png");
  assert.equal(manifest["src/style.css"]?.file, entry.css[0], "css row keyed by its source path");
  assert.match(manifest["src/linked.css"]?.file ?? "", /^static\/linked-/, "linked stylesheet has a row");
  console.log("ok: --outDir --base --assetsDir --minify false --sourcemap inline --manifest --assetsInlineLimit");

  // 3. --manifest <name>, --out alias, short -m/-c, --app accepted.
  const out2 = path.join(base, "out2");
  fs.writeFileSync(path.join(app, "custom.config.mjs"), "export default { build: { cssMinify: false } };\n");
  fs.writeFileSync(path.join(app, ".env.staging"), "VITE_STAGE=staging-env\n");
  fs.appendFileSync(path.join(app, "src", "main.js"), `window.__stage = import.meta.env.VITE_STAGE;\n`);
  ok(["--out", out2, "--manifest", "custom/m.json", "-m", "staging", "-c", "custom.config.mjs", "--app"]);
  assert.ok(fs.existsSync(path.join(out2, "custom", "m.json")), "--manifest <name> picks the file name");
  assert.ok(!fs.existsSync(path.join(out2, ".vite")), "no default manifest next to the custom one");
  const main2 = readMain(out2);
  assert.ok(main2.includes("staging-env"), "-m selects .env.staging");
  assert.ok(!main2.includes("usedFunction"), "js still minified");
  assert.ok(
    readCss(out2).includes("\n  color: red;"),
    "-c config's cssMinify: false leaves CSS readable while js minifies",
  );
  console.log("ok: --manifest <name>, --out alias, -m, -c, --app");

  // 4. --watch is a clear error, not a silent full build.
  const w = run(["--watch"]);
  assert.notEqual(w.status, 0, "--watch exits non-zero");
  assert.match(w.stderr, /--watch is not supported/);
  console.log("ok: --watch is rejected with a clear message");

  // 5. Unsupported build.* options warn once each.
  const u = ok([], { build: { write: false, license: true, commonjsOptions: {}, watch: {} } });
  for (const key of ["write: false", "watch", "license", "commonjsOptions"]) {
    assert.ok(u.stderr.includes(`build.${key} is not supported`), `warns about build.${key}:\n${u.stderr}`);
  }
  console.log("ok: unsupported build options warn");

  // 6. build.manifest / build.assetsDir from the config file.
  ok([], { build: { manifest: "meta/manifest.json", assetsDir: "s/t" } });
  assert.ok(fs.existsSync(path.join(dist, "meta", "manifest.json")), "build.manifest string names the file");
  assert.ok(
    files(dist).some((f) => f.startsWith("s/t/main-")),
    "build.assetsDir nests output directories",
  );
  console.log("ok: build.manifest / build.assetsDir from config");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  fs.rmSync(base, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
