// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-rolldownopts-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "ro-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "oj.config.json"),
  JSON.stringify({
    build: {
      rolldownOptions: {
        output: { entryFileNames: "custom/[name].js", chunkFileNames: "custom/[name]-[hash].js" },
        external: ["cdn-lib"],
      },
    },
  }),
);
fs.writeFileSync(path.join(app, "src", "main.js"), `import { thing } from "cdn-lib";\nwindow.__X = thing;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });

  assert.ok(fs.existsSync(path.join(app, "dist", "custom", "main.js")), "entryFileNames template applied");
  assert.ok(!fs.existsSync(path.join(app, "dist", "assets")), "default assets dir not used");

  const html = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
  assert.match(html, /src="\/custom\/main\.js"/, "html references the custom entry path");

  const entry = fs.readFileSync(path.join(app, "dist", "custom", "main.js"), "utf8");
  assert.match(entry, /["']cdn-lib["']/, "external module left as a bare import, not bundled");

  // Vite spreads the rest of `rollupOptions.output` into rolldown: format,
  // banner/footer, umd/iife name, inlineDynamicImports, paths for externals;
  // `treeshake: false` reaches rolldown; an output array is warned about.
  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  fs.writeFileSync(path.join(app, "src", "lazy.js"), `export const lazy = "LAZY_MARKER";\n`);
  fs.writeFileSync(
    path.join(app, "src", "main.js"),
    `import { thing } from "cdn-lib";\nwindow.__X = thing;\nexport const load = () => import("./lazy.js");\n`,
  );
  fs.writeFileSync(
    path.join(app, "oj.config.json"),
    JSON.stringify({
      build: {
        // comments (the banner/footer here) do not survive minification, as in Vite
        minify: false,
        rolldownOptions: {
          external: ["cdn-lib"],
          treeshake: false,
          output: [
            {
              format: "iife",
              name: "App",
              banner: "/* BANNER */",
              footer: "/* FOOTER */",
              inlineDynamicImports: true,
              globals: { "cdn-lib": "CdnLib" },
            },
            { format: "cjs" },
          ],
        },
      },
    }),
  );
  const stderr = execSync(`${oj} build ${app} 2>&1 1>/dev/null`, { encoding: "utf8", shell: true });
  assert.match(
    stderr,
    /output lists 2 outputs; oj builds the first one only/,
    "array output is warned about, not silently truncated",
  );
  const files = fs.readdirSync(path.join(app, "dist", "assets"));
  const mainFile = files.find((f) => f.startsWith("main-") && f.endsWith(".js"));
  assert.ok(mainFile, `iife entry under assets/: ${files.join(", ")}`);
  assert.ok(
    !files.some((f) => f.startsWith("lazy-")),
    "inlineDynamicImports: true folds the lazy chunk into the entry",
  );
  const iife = fs.readFileSync(path.join(app, "dist", "assets", mainFile), "utf8");
  assert.ok(iife.startsWith("/* BANNER */"), `banner is the first thing in the file: ${iife.slice(0, 40)}`);
  assert.ok(iife.trimEnd().endsWith("/* FOOTER */"), "footer is last");
  assert.match(iife, /var App\s*=|App\s*=\s*\(function|\bApp\b/, "iife exposes output.name");
  assert.match(iife, /CdnLib/, "output.globals names the external's global");
  assert.ok(iife.includes("LAZY_MARKER"), "the dynamic import was inlined");
  assert.ok(!iife.includes("__vitePreload"), "no es-only preload helper in an iife bundle");

  // paths: an external specifier rewritten to a URL in the es output.
  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  fs.writeFileSync(
    path.join(app, "oj.config.json"),
    JSON.stringify({
      build: {
        rolldownOptions: {
          external: ["cdn-lib"],
          output: { paths: { "cdn-lib": "https://cdn.example.test/lib.js" }, banner: "/*! license */" },
        },
      },
    }),
  );
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const esFiles = fs.readdirSync(path.join(app, "dist", "assets"));
  const esMain = fs.readFileSync(
    path.join(
      app,
      "dist",
      "assets",
      esFiles.find((f) => f.startsWith("main-") && f.endsWith(".js")),
    ),
    "utf8",
  );
  assert.match(esMain, /["']https:\/\/cdn\.example\.test\/lib\.js["']/, "output.paths rewrites the external specifier");
  assert.ok(esMain.startsWith("/*! license */"), "banner precedes oj's preload helper in es output");
  assert.ok(esMain.includes("__vitePreload"), "es output keeps the preload helper");

  console.log("ROLLDOWN-OPTIONS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("ROLLDOWN-OPTIONS E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
