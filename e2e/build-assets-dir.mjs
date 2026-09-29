// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// build.assetsDir: hashed chunks, stylesheets and url() assets land under the
// configured directory (Vite's default output patterns are
// `${assetsDir}/[name]-[hash]...`), and a relative base still references
// assets from the stylesheet as siblings. Run with a built target/debug/oj.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

function scaffold(config) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-assets-dir-"));
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "assets-dir", type: "module" }));
  fs.writeFileSync(path.join(app, "oj.config.ts"), config);
  fs.writeFileSync(
    path.join(app, "index.html"),
    `<!doctype html><html><head><link rel="stylesheet" href="/src/page.css"></head><body><script type="module" src="/src/main.js"></script></body></html>`,
  );
  fs.writeFileSync(
    path.join(app, "src", "main.js"),
    `import "./app.css";\nimport("./lazy.js").then((m) => m.run());\n`,
  );
  fs.writeFileSync(path.join(app, "src", "lazy.js"), `export function run() { document.body.textContent = "lazy"; }\n`);
  fs.writeFileSync(path.join(app, "src", "app.css"), `.a { background: url(./big.png); }\n`);
  fs.writeFileSync(path.join(app, "src", "page.css"), `.p { color: red; }\n`);
  // Above the 4 KiB inline limit so it is emitted as a file.
  fs.writeFileSync(path.join(app, "src", "big.png"), Buffer.alloc(8192, 1));
  return app;
}

function walk(dir, prefix = "") {
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) out.push(...walk(path.join(dir, entry.name), rel));
    else out.push(rel);
  }
  return out;
}

function build(app) {
  const r = spawnSync(oj, ["build", app], { cwd: repo, encoding: "utf8" });
  if (r.status !== 0) throw new Error(`build failed:\n${r.stdout}\n${r.stderr}`);
  return walk(path.join(app, "dist")).filter((f) => !f.startsWith(".vite/"));
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

// 1. assetsDir "static" with an absolute base.
{
  const app = scaffold(`export default { build: { assetsDir: "static", manifest: true } };\n`);
  try {
    const files = build(app);
    const hashed = files.filter((f) => f !== "index.html");
    check(
      "every hashed output is under static/",
      hashed.length > 0 && hashed.every((f) => f.startsWith("static/")),
      files.join(", "),
    );
    check(
      "lazy chunk under static/",
      hashed.some((f) => /^static\/lazy-[^/]+\.js$/.test(f)),
      files.join(", "),
    );
    check("stylesheets under static/", hashed.filter((f) => f.endsWith(".css")).length >= 1, files.join(", "));
    check(
      "url() asset under static/",
      hashed.some((f) => /^static\/big-[^/]+\.png$/.test(f)),
      files.join(", "),
    );
    check("nothing under assets/", !files.some((f) => f.startsWith("assets/")), files.join(", "));
    const html = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
    check(
      "html references /static/",
      /\/static\/main-[^"']+\.js/.test(html) && /\/static\/[^"']+\.css/.test(html),
      html,
    );
    const css = hashed
      .filter((f) => f.endsWith(".css"))
      .map((f) => fs.readFileSync(path.join(app, "dist", f), "utf8"))
      .join("\n");
    check("css url() points at /static/", /url\("?\/static\/big-[^)"]+\.png"?\)/.test(css), css);
    const manifest = JSON.parse(fs.readFileSync(path.join(app, "dist", ".vite", "manifest.json"), "utf8"));
    check(
      "manifest file paths start with static/",
      Object.values(manifest).every((e) => e.file.startsWith("static/")),
      JSON.stringify(manifest),
    );
  } finally {
    fs.rmSync(app, { recursive: true, force: true });
  }
}

// 2. assetsDir "" (outDir root) with a relative base: sibling url() reference.
{
  const app = scaffold(`export default { base: "./", build: { assetsDir: "" } };\n`);
  try {
    const files = build(app);
    check(
      "hashed outputs at the outDir root",
      files.every((f) => !f.includes("/")),
      files.join(", "),
    );
    const css = files
      .filter((f) => f.endsWith(".css"))
      .map((f) => fs.readFileSync(path.join(app, "dist", f), "utf8"))
      .join("\n");
    check("relative css url() is a sibling", /url\("?\.\/big-[^)"]+\.png"?\)/.test(css), css);
  } finally {
    fs.rmSync(app, { recursive: true, force: true });
  }
}

// 3. Default stays Vite's assets/.
{
  const app = scaffold(`export default {};\n`);
  try {
    const files = build(app);
    check(
      "default assetsDir is assets/",
      files.filter((f) => f !== "index.html").every((f) => f.startsWith("assets/")),
      files.join(", "),
    );
  } finally {
    fs.rmSync(app, { recursive: true, force: true });
  }
}

if (failed) process.exit(1);
console.log("BUILD-ASSETS-DIR E2E PASSED");
