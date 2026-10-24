// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// UnoCSS's global build mode and a plugin-served virtual stylesheet, built as
// Vite builds them: UnoCSS collects the page's classes in an `order: "pre"`
// transformIndexHtml hook (after its buildStart reset, before renderChunk),
// serves placeholder layer modules and hands their CSS to vite:css-post,
// which replaces the placeholder; a virtual `.css` a plugin loads is compiled
// like a file. When the start-app fixture's Vite is installed, the same app
// is built with it and the stylesheets must be byte-identical.

import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const UNOCSS = "unocss@66.6.0";

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-unocss-"));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write("package.json", JSON.stringify({ name: "unocss-app", type: "module" }));
try {
  execSync(`npm install ${UNOCSS} --no-audit --no-fund --loglevel=error`, { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP unocss: could not install unocss (offline?)");
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}
write(
  "vite.config.js",
  `import UnoCSS from "unocss/vite";\nimport { presetWind3 } from "unocss";\n` +
    `const virtualCss = {\n  name: "virtual-css",\n` +
    `  resolveId(id) { if (id === "virtual:theme.css") return "\\0virtual:theme.css"; },\n` +
    `  load(id) { if (id === "\\0virtual:theme.css") return ".from-plugin { color: rebeccapurple; }\\n"; },\n};\n` +
    `export default { plugins: [UnoCSS({ presets: [presetWind3()] }), virtualCss] };\n`,
);
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><div class="p-4 text-red-500 m-2">hi</div><script type="module" src="/src/main.js"></script></body></html>`,
);
write("src/main.js", `import "virtual:uno.css";\nimport "virtual:theme.css";\n`);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const sheet = (dir) =>
    fs
      .readdirSync(path.join(app, dir, "assets"))
      .filter((f) => f.endsWith(".css"))
      .map((f) => [f, fs.readFileSync(path.join(app, dir, "assets", f), "utf8")]);
  const built = sheet("dist");
  const css = built.map(([, c]) => c).join("\n");
  for (const rule of [".m-2{margin:.5rem}", ".p-4{padding:1rem}", ".text-red-500{", ".from-plugin{color:#639}"]) {
    assert.ok(css.includes(rule), `${rule} in the stylesheet:\n${css}`);
  }
  assert.ok(!css.includes("--unocss--"), `UnoCSS's layer placeholder was replaced:\n${css}`);
  console.log("[build] page classes, the virtual sheet and no placeholder");

  const viteBin = path.join(repo, "e2e", "fixtures", "start-app", "node_modules", "vite", "bin", "vite.js");
  if (fs.existsSync(viteBin)) {
    execSync(`node ${viteBin} build --outDir dist-vite --logLevel error`, { cwd: app, stdio: "inherit" });
    assert.deepEqual(built, sheet("dist-vite"), "the stylesheet Vite writes, name and bytes");
    console.log("[vite] byte-identical to a live Vite build");
  } else {
    console.log("[vite] fixture Vite not installed; skipped the live comparison");
  }
  console.log("CSS-PLUGIN-UNOCSS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-PLUGIN-UNOCSS E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
