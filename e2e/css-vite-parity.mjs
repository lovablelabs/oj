// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite parity for specifiers inside stylesheets, in dev and in the build:
// resolve.alias applies to @import / @use / url(); a root-absolute /src/x is
// resolved against the root (public dir first); legacy hacks the postcss
// pipeline tolerates never fail a stylesheet.

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
const PORT = 6350;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cssparity-"));
fs.mkdirSync(path.join(app, "src", "ui"), { recursive: true });
fs.mkdirSync(path.join(app, "public"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "cssparity-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.js"),
  `import { fileURLToPath } from "node:url";\n` +
    `export default {\n` +
    `  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },\n` +
    `  css: { preprocessorOptions: { scss: { additionalData: "$injected: #abcdef;" } } },\n` +
    `};\n`,
);
// A PostCSS config (postcss from e2e's own deps) whose plugin rewrites the
// marker value: rules of @imported files must reach it (postcss-import runs
// first in Vite's chain), and so must a `?inline` import.
fs.mkdirSync(path.join(app, "node_modules"), { recursive: true });
fs.symlinkSync(path.join(repo, "e2e", "node_modules", "postcss"), path.join(app, "node_modules", "postcss"), "dir");
fs.writeFileSync(
  path.join(app, "postcss.config.mjs"),
  `export default { plugins: [{ postcssPlugin: "e2e-mark", Declaration(d) { if (d.value === "MAGIC") d.value = "rgb(1, 2, 3)"; } }] };\n`,
);
fs.writeFileSync(path.join(app, "src", "ui", "imported.css"), `.imported { color: MAGIC; }\n`);
fs.writeFileSync(path.join(app, "src", "vars.css"), `:root { --v: 1; }\n`);
fs.writeFileSync(path.join(app, "src", "root.css"), `.root-abs { color: green; }\n`);
fs.writeFileSync(path.join(app, "src", "img.png"), Buffer.alloc(5000, 9));
fs.writeFileSync(path.join(app, "public", "pub.png"), Buffer.alloc(100, 1));
fs.writeFileSync(path.join(app, "src", "_tokens.scss"), `$c: #123456;\n`);
fs.writeFileSync(
  path.join(app, "src", "ui", "theme.scss"),
  `@use "@/tokens" as t;\n.theme { color: t.$c; background: url("@/img.png"); border-color: $injected; }\n`,
);
fs.writeFileSync(
  path.join(app, "src", "ui", "app.css"),
  `@import "@/vars.css";\n@import "/src/root.css";\n@import "./imported.css";\n` +
    `.a { background: url("@/img.png"); }\n.b { background: url(/src/img.png); }\n.c { background: url(/pub.png); }\n` +
    `.hack { *zoom: 1; color: red; }\n`,
);
fs.writeFileSync(
  path.join(app, "src", "ui", "btn.module.css"),
  `.button { color: red; }\n.my-class { color: blue; }\n.default { color: green; }\n`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import "./ui/app.css";\nimport "./ui/theme.scss";\nimport styles, { button } from "./ui/btn.module.css";\n` +
    `import inlined from "./ui/app.css?inline";\n` +
    `document.body.className = button + " " + styles["my-class"];\nwindow.__INLINE = inlined;\nwindow.__OK = true;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const res = await fetch(`http://localhost:${PORT}/src/ui/app.css?direct`);
  const css = await res.text();
  assert.equal(res.status, 200, `stylesheet with a legacy hack still serves:\n${css}`);
  assert.match(css, /--v:\s*1/, `aliased @import inlined:\n${css}`);
  assert.match(css, /\.root-abs/, `root-absolute @import inlined:\n${css}`);
  assert.match(css, /\.a\s*\{[^}]*url\(["']?\/src\/img\.png/, `aliased url() -> served url of the file:\n${css}`);
  assert.doesNotMatch(css, /\/src\/@\//, `alias treated as a relative segment:\n${css}`);
  assert.match(css, /\.b\s*\{[^}]*url\(["']?\/src\/img\.png/, "root-absolute url() kept");
  assert.match(css, /url\(["']?\/pub\.png/, "public url kept");
  assert.match(css, /\.hack\s*\{[^}]*color:\s*red/, `rule with a dropped hack keeps its other declarations:\n${css}`);
  assert.match(css, /\.imported\s*\{[^}]*(#010203|rgb\(1,\s*2,\s*3\))/, `@imported rules go through PostCSS:\n${css}`);
  assert.doesNotMatch(css, /MAGIC/, "postcss marker left in an imported rule");
  const scss = await (await fetch(`http://localhost:${PORT}/src/ui/theme.scss?direct`)).text();
  assert.match(scss, /#123456/, `sass @use through the alias:\n${scss}`);
  assert.match(scss, /url\(["']?\/src\/img\.png/, `aliased url() in sass output:\n${scss}`);
  // `?inline` is the same compiled stylesheet as a string: PostCSS, @import
  // inlining, url() rewriting, Sass additionalData, all applied.
  const inline = await (await fetch(`http://localhost:${PORT}/src/ui/app.css?inline`)).text();
  assert.match(inline, /^export default "/, `?inline exports a string:\n${inline}`);
  const inlineCss = JSON.parse(inline.replace(/^export default /, "").replace(/;\s*$/, ""));
  assert.match(inlineCss, /--v:\s*1/, `?inline has the aliased import inlined:\n${inlineCss}`);
  assert.match(inlineCss, /\.imported\s*\{[^}]*(#010203|rgb\(1,\s*2,\s*3\))/, `?inline went through PostCSS:\n${inlineCss}`);
  assert.doesNotMatch(inlineCss, /MAGIC|@import/, "?inline skipped the pipeline");
  assert.match(inlineCss, /url\(["']?\/src\/img\.png/, "?inline rewrites aliased urls like the wrapper");
  const inlineScss = await (await fetch(`http://localhost:${PORT}/src/ui/theme.scss?inline`)).text();
  assert.match(inlineScss, /#abcdef/, `?inline applies scss additionalData:\n${inlineScss}`);
  assert.match(inlineScss, /#123456/, "?inline resolves the sass alias");
  const inlineMod = await (await fetch(`http://localhost:${PORT}/src/ui/btn.module.css?inline`)).text();
  assert.match(inlineMod, /^export default "[^]*btn-module_button_/, `?inline of a css module is its css:\n${inlineMod}`);
  assert.doesNotMatch(inlineMod, /export const/, "?inline of a css module must not export the class map");
  const mod = await (await fetch(`http://localhost:${PORT}/src/ui/btn.module.css`)).text();
  assert.match(mod, /export const button = "btn-module_button_[A-Za-z0-9_-]+";/, `css module named export:\n${mod}`);
  assert.match(mod, /export default \{"button":"btn-module_button_/, `css module default map kept:\n${mod}`);
  assert.match(mod, /"my-class":"btn-module_my-class_/, "kebab class stays in the default map");
  assert.doesNotMatch(mod, /export const (my-class|default) /, "illegal identifiers are not named exports");
  console.log("[dev] alias + root-absolute + error recovery + css module named exports OK");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assets = path.join(app, "dist", "assets");
  const files = fs.readdirSync(assets);
  const cssFiles = files.filter((f) => f.endsWith(".css"));
  assert.ok(cssFiles.length > 0, `chunk stylesheet emitted: ${files}`);
  const built = cssFiles.map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("\n");
  const img = files.find((f) => /^img-[A-Za-z0-9_-]+\.png$/.test(f));
  assert.ok(img, `aliased / root-absolute url() asset emitted: ${files}`);
  assert.match(built, /--v:\s*1/, "aliased @import inlined in build");
  assert.match(built, /\.root-abs/, "root-absolute @import inlined in build");
  assert.match(built, /#123456/, "sass alias in build");
  assert.doesNotMatch(built, /@\/img\.png|url\(["']?\/src\/img\.png/, `alias and root-absolute urls rewritten to the emitted asset:\n${built}`);
  const refs = built.match(new RegExp(img.replace(/\./g, "\\."), "g")) || [];
  assert.ok(refs.length >= 1, `css references the emitted asset:\n${built}`);
  assert.match(built, /url\(["']?\/pub\.png/, "public url kept in build");
  assert.match(built, /\.hack\{color:red\}/, `hack dropped, rule kept in build:\n${built}`);
  // `import { button }` from a CSS module bundles (rolldown would reject a
  // missing named export) and carries the scoped class.
  const js = files.filter((f) => f.endsWith(".js")).map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("\n");
  assert.match(js, /btn-module_button_[A-Za-z0-9_-]+/, `named css module export bundled:\n${js.slice(0, 400)}`);
  assert.match(built, /\.btn-module_button_[A-Za-z0-9_-]+\{color:red\}/, "css module rules in the stylesheet");
  assert.match(built, /\.imported\{color:#010203\}/, `@imported rules went through PostCSS in the build:\n${built}`);
  assert.match(built, /border-color:#abcdef/, "scss additionalData in build");
  assert.doesNotMatch(built, /MAGIC/, "postcss marker left in the build css");
  assert.match(js, /\.imported\{color:#010203\}/, `?inline in the build carries the postcss-processed css:\n${js.slice(0, 400)}`);
  console.log("[build] alias + root-absolute + error recovery + css module named exports OK");
  console.log("CSS-VITE-PARITY E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-VITE-PARITY E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
