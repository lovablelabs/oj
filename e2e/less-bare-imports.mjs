// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Bare Less imports resolve through node_modules as with Vite's Less file
// manager: a package's `less`/`style` field, a subpath with the `.less`
// extension added, and the `~pkg` prefix. Run with a built target/debug/oj;
// installs `less` into a temp app (skips when offline).
import { execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-less-bare-"));
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "less-bare", version: "1.0.0", type: "module" }),
);
try {
  execSync("npm install less --no-audit --no-fund --loglevel=error", { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP less-bare-imports: could not install less (offline?)");
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}

// A fake dependency shaped like bootstrap's less distribution.
const pkg = path.join(app, "node_modules", "fakeless");
fs.mkdirSync(path.join(pkg, "less", "mixins"), { recursive: true });
fs.writeFileSync(
  path.join(pkg, "package.json"),
  JSON.stringify({ name: "fakeless", version: "1.0.0", less: "less/entry.less", main: "index.js" }),
);
fs.writeFileSync(
  path.join(pkg, "less", "entry.less"),
  `@import "./mixins/vars";\n.from-entry { color: @entry-color; }\n`,
);
fs.writeFileSync(
  path.join(pkg, "less", "mixins", "vars.less"),
  `@entry-color: rgb(1, 2, 3);\n@sub-color: rgb(4, 5, 6);\n`,
);
const scoped = path.join(app, "node_modules", "@acme", "theme");
fs.mkdirSync(scoped, { recursive: true });
fs.writeFileSync(
  path.join(scoped, "package.json"),
  JSON.stringify({ name: "@acme/theme", version: "1.0.0", style: "theme.css" }),
);
fs.writeFileSync(path.join(scoped, "theme.css"), `.from-scoped { color: rgb(7, 8, 9); }\n`);

fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(
  path.join(app, "src", "a.less"),
  `@import "fakeless";\n@import "~fakeless/less/mixins/vars";\n@import "@acme/theme";\n.uses-sub { color: @sub-color; }\n`,
);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./a.less";\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
function check(label, ok, detail) {
  if (!ok) {
    failed = true;
    console.error(`FAIL ${label}: ${detail}`);
  } else {
    console.log(`ok   ${label}`);
  }
}

const r = spawnSync(oj, ["build", app], { cwd: repo, encoding: "utf8" });
check("build succeeds with bare less imports", r.status === 0, `${r.stdout}\n${r.stderr}`);
if (r.status === 0) {
  const assets = path.join(app, "dist", "assets");
  const css = fs
    .readdirSync(assets)
    .filter((f) => f.endsWith(".css"))
    .map((f) => fs.readFileSync(path.join(assets, f), "utf8"))
    .join("\n");
  check("package `less` field entry compiled", /\.from-entry\{color:#010203\}/.test(css), css);
  check("~pkg subpath with .less added compiled", /\.uses-sub\{color:#040506\}/.test(css), css);
  check("scoped package `style` field (css) compiled", /\.from-scoped\{color:#070809\}/.test(css), css);
}

fs.rmSync(app, { recursive: true, force: true });
if (failed) process.exit(1);
console.log("LESS-BARE-IMPORTS E2E PASSED");
