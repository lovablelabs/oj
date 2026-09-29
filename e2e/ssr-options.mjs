// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// SSR build options and chunking options oj used to ignore: `build.ssr: true`
// with `rollupOptions.input`, `ssr.noExternal` / `ssr.external`,
// `build.ssrManifest`, `output.advancedChunks`, path values in the
// `manualChunks` object, and a loud warning for function-valued rollup options
// (which cannot cross the config's JSON boundary).

import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-ssropts-"));
const pkg = (name, body) => {
  const dir = path.join(app, "node_modules", name);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    path.join(dir, "package.json"),
    JSON.stringify({ name, version: "1.0.0", type: "module", main: "index.js" }),
  );
  fs.writeFileSync(path.join(dir, "index.js"), body);
};
fs.mkdirSync(path.join(app, "src", "utils"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "ssropts", version: "1.0.0" }));
pkg("dep-ext", `export const ext = "DEP_EXT_MARKER";\n`);
pkg("dep-bundled", `export const bundled = "DEP_BUNDLED_MARKER";\n`);
fs.writeFileSync(
  path.join(app, "src", "entry-server.js"),
  `import { ext } from "dep-ext";\nimport { bundled } from "dep-bundled";\nexport function render() { return ext + bundled; }\n`,
);
fs.writeFileSync(path.join(app, "src", "utils", "index.js"), `export const util = "UTIL_MARKER";\n`);
fs.writeFileSync(path.join(app, "src", "lazy.css"), `.lazy { color: red }\n`);
fs.writeFileSync(path.join(app, "src", "lazy.js"), `import "./lazy.css";\nexport const lazy = "LAZY_MARKER";\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import { util } from "./utils/index.js";\nimport { ext } from "dep-ext";\nwindow.__U = util + ext;\nimport("./lazy.js").then((m) => { window.__L = m.lazy; });\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

function build(config, { configFile = "oj.config.json", args = "" } = {}) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  for (const f of ["oj.config.json", "oj.config.mjs", "vite.config.js"]) fs.rmSync(path.join(app, f), { force: true });
  fs.writeFileSync(path.join(app, configFile), typeof config === "string" ? config : JSON.stringify(config));
  return execSync(`${oj} build ${app} ${args} 2>&1 1>/dev/null`, { shell: "/bin/sh" }).toString();
}
const readAll = (dir) =>
  Object.fromEntries(
    fs
      .readdirSync(dir)
      .map((f) => [f, fs.statSync(path.join(dir, f)).isFile() ? fs.readFileSync(path.join(dir, f), "utf8") : ""]),
  );

let failed = false;
try {
  // build.ssr: true takes the entry from rollupOptions.input; deps external by default.
  build({ build: { ssr: true, rollupOptions: { input: "src/entry-server.js" } } });
  let server = fs.readFileSync(path.join(app, "dist", "entry-server.mjs"), "utf8");
  assert.match(server, /from\s*["']dep-ext["']/, "dep-ext stays external by default");
  assert.match(server, /from\s*["']dep-bundled["']/, "dep-bundled stays external by default");
  assert.doesNotMatch(server, /DEP_BUNDLED_MARKER/, "dep-bundled not inlined by default");

  // ssr.noExternal list bundles just those; ssr.external keeps its entries out.
  build({ build: { ssr: "src/entry-server.js" }, ssr: { noExternal: ["dep-bundled"] } });
  server = fs.readFileSync(path.join(app, "dist", "entry-server.mjs"), "utf8");
  assert.match(server, /DEP_BUNDLED_MARKER/, "ssr.noExternal inlines dep-bundled");
  assert.match(server, /from\s*["']dep-ext["']/, "dep-ext still external");
  build({ build: { ssr: "src/entry-server.js" }, ssr: { noExternal: true, external: ["dep-ext"] } });
  server = fs.readFileSync(path.join(app, "dist", "entry-server.mjs"), "utf8");
  assert.match(server, /DEP_BUNDLED_MARKER/, "noExternal: true inlines deps");
  assert.match(server, /from\s*["']dep-ext["']/, "ssr.external wins over noExternal: true");
  // ...and from vite.config.
  build(
    `export default { build: { ssr: true, rollupOptions: { input: "src/entry-server.js" } }, ssr: { noExternal: ["dep-bundled"] } };\n`,
    { configFile: "vite.config.js" },
  );
  server = fs.readFileSync(path.join(app, "dist", "entry-server.mjs"), "utf8");
  assert.match(server, /DEP_BUNDLED_MARKER/, "vite.config ssr.noExternal applied");

  // build.ssrManifest in the client build maps source modules to their chunk + css.
  build({ build: { ssrManifest: true } });
  const ssrManifest = JSON.parse(fs.readFileSync(path.join(app, "dist", ".vite", "ssr-manifest.json"), "utf8"));
  assert.ok(Array.isArray(ssrManifest["src/lazy.js"]), `lazy module keyed root-relative: ${Object.keys(ssrManifest)}`);
  assert.ok(
    ssrManifest["src/lazy.js"].some((u) => /^\/assets\/lazy-.*\.js$/.test(u)),
    "lazy chunk url listed",
  );
  assert.ok(
    ssrManifest["src/lazy.js"].some((u) => /^\/assets\/lazy-.*\.css$/.test(u)),
    "lazy css url listed",
  );
  assert.deepEqual(ssrManifest["src/main.js"], [], "entry module has no preload urls");
  // Vite's css deps map: the dynamically imported chunk, keyed by file name,
  // lists the stylesheets it brings in.
  const lazyChunk = Object.keys(ssrManifest).find((k) => /^lazy-.*\.js$/.test(k));
  assert.ok(lazyChunk, `dynamic import chunk keyed by file name: ${Object.keys(ssrManifest)}`);
  assert.ok(
    ssrManifest[lazyChunk].some((u) => /^\/assets\/lazy-.*\.css$/.test(u)),
    "css deps of the lazy chunk listed",
  );
  build({ build: { ssrManifest: "custom-ssr.json" } });
  assert.ok(fs.existsSync(path.join(app, "dist", "custom-ssr.json")), "ssrManifest file name honored");
  // importedAssets: a module's imported asset url is listed for its chunk (Vite).
  fs.writeFileSync(path.join(app, "src", "big.png"), Buffer.alloc(9000, 1));
  fs.writeFileSync(
    path.join(app, "src", "lazy.js"),
    `import "./lazy.css";\nimport big from "./big.png";\nexport const lazy = "LAZY_MARKER" + big;\n`,
  );
  build({ base: "/app/", build: { ssrManifest: true } });
  const withAssets = JSON.parse(fs.readFileSync(path.join(app, "dist", ".vite", "ssr-manifest.json"), "utf8"));
  assert.ok(
    withAssets["src/lazy.js"].some((u) => /^\/app\/assets\/big-.*\.png$/.test(u)),
    `imported asset url listed under base: ${withAssets["src/lazy.js"]}`,
  );
  assert.ok(
    withAssets["src/big.png"]?.some((u) => /^\/app\/assets\/big-.*\.png$/.test(u)),
    "the asset module itself maps to its url",
  );
  fs.writeFileSync(path.join(app, "src", "lazy.js"), `import "./lazy.css";\nexport const lazy = "LAZY_MARKER";\n`);

  // SSR app (server entry + client sibling): the client entry honors `base` for
  // import.meta.env.BASE_URL and the script/style urls the server injects.
  fs.writeFileSync(
    path.join(app, "src", "entry-client.js"),
    `import "./lazy.css";\nwindow.__B = import.meta.env.BASE_URL;\n`,
  );
  build({ base: "/app/", build: { ssr: "src/entry-server.js", assetsDir: "static" } });
  const serverMjs = fs.readFileSync(path.join(app, "dist", "server.mjs"), "utf8");
  assert.match(
    serverMjs,
    /"\/app\/static\/entry-client-[^"]+\.js"/,
    `server injects the client script under base + assetsDir: ${serverMjs.match(/CLIENT_JS = "[^"]*"/)?.[0]}`,
  );
  assert.match(
    serverMjs,
    /"\/app\/static\/style-[^"]+\.css"/,
    "server injects the client stylesheet under base + assetsDir",
  );
  const clientJs = fs
    .readdirSync(path.join(app, "dist", "static"))
    .find((f) => f.startsWith("entry-client-") && f.endsWith(".js"));
  assert.ok(
    fs.readFileSync(path.join(app, "dist", "static", clientJs), "utf8").match(/["'`]\/app\/["'`]/),
    "client BASE_URL define is the base",
  );
  fs.rmSync(path.join(app, "src", "entry-client.js"));

  // manualChunks object: bare names hit node_modules, ./paths hit source files.
  build({
    build: { rollupOptions: { output: { manualChunks: { vendor: ["dep-ext"], utils: ["./src/utils/index.js"] } } } },
  });
  let assets = readAll(path.join(app, "dist", "assets"));
  let vendor = Object.entries(assets).find(([f]) => f.startsWith("vendor-") && f.endsWith(".js"));
  let utils = Object.entries(assets).find(([f]) => f.startsWith("utils-") && f.endsWith(".js"));
  assert.ok(vendor && /DEP_EXT_MARKER/.test(vendor[1]), "vendor chunk from package name");
  assert.ok(utils && /UTIL_MARKER/.test(utils[1]), "utils chunk from a ./ path value");

  // advancedChunks groups (rolldown native form; RegExp test via oj.config.mjs).
  build(
    `export default { build: { rolldownOptions: { output: { advancedChunks: { groups: [{ name: "libs", test: /node_modules/ }] } } } } };\n`,
    { configFile: "oj.config.mjs" },
  );
  assets = readAll(path.join(app, "dist", "assets"));
  const libs = Object.entries(assets).find(([f]) => f.startsWith("libs-") && f.endsWith(".js"));
  assert.ok(libs && /DEP_EXT_MARKER/.test(libs[1]), `advancedChunks group emitted: ${Object.keys(assets)}`);

  // A function-valued option is reported instead of silently dropped (both config loaders).
  let stderr = build(
    `export default { build: { rollupOptions: { output: { manualChunks(id) { return "x"; }, chunkFileNames: () => "c.js" } } } };\n`,
    { configFile: "oj.config.mjs" },
  );
  assert.match(stderr, /output\.manualChunks is a function/, `oj.config warns about manualChunks fn:\n${stderr}`);
  assert.match(stderr, /output\.chunkFileNames is a function/, "oj.config warns about chunkFileNames fn");
  stderr = build(`export default { build: { rollupOptions: { output: { manualChunks(id) { return "x"; } } } } };\n`, {
    configFile: "vite.config.js",
  });
  assert.match(stderr, /output\.manualChunks is a function/, `vite.config warns about manualChunks fn:\n${stderr}`);
  assert.ok(fs.existsSync(path.join(app, "dist", "index.html")), "build still succeeds");

  console.log("SSR-OPTIONS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("SSR-OPTIONS E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
