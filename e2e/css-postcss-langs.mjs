// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `.sss`, `.pcss` and `.postcss` are stylesheets, as in Vite's CSS_LANGS_RE,
// in dev and in the build. SugarSS follows Vite's playground/css case: the
// app's `sugarss` parses the sheet and the `.sss` files it `@import`s (one
// through a `resolve.alias`), each imported `url()` resolved against its own
// file. When the start-app fixture's Vite is installed, the same app is built
// with it and the stylesheets must be byte-identical.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6374;
const SUGARSS = "sugarss@5.0.1";

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-css-langs-"));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write("package.json", JSON.stringify({ name: "css-langs", type: "module" }));
try {
  execSync(`npm install ${SUGARSS} --no-audit --no-fund --loglevel=error`, { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP css postcss langs: could not install sugarss (offline?)");
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}
write(
  "vite.config.js",
  `import { fileURLToPath } from "node:url";\n` +
    `export default { resolve: { alias: [{ find: "=", replacement: fileURLToPath(new URL("./src", import.meta.url)) }] } };\n`,
);
// Vite's playground/css sugarss.sss and nested/nested.sss.
write("src/sugarss.sss", `@import '=/nested/nested.sss'\n\n.sugarss\n  color: blue\n`);
write(
  "src/nested/nested.sss",
  `.sugarss-at-import\n  color: darkslateblue\n  background: url(./icon.png) 10px no-repeat\n\n\n` +
    `.sugarss-at-import-alias\n  color: darkslateblue\n  background: url(=/nested/icon.png) 10px no-repeat\n`,
);
fs.writeFileSync(path.join(app, "src/nested/icon.png"), Buffer.alloc(300, 7));
write("src/a.pcss", `.pc { color: pink; }\n`);
write("src/b.postcss", `.po { color: peru; }\n`);
write("src/c.module.pcss", `.card { color: red; }\n`);
// PostCSS parses SugarSS itself: postcss-modules hashes this text, not its CSS.
write("src/d.module.sss", `.tile\n  color: red\n\n.label\n  color: blue\n`);
write(
  "src/main.js",
  `import "./sugarss.sss";\nimport "./a.pcss";\nimport "./b.postcss";\nimport c from "./c.module.pcss";\nimport d from "./d.module.sss";\nwindow.__C = c;\nwindow.__D = d;\n`,
);
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
let srv;
try {
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const sss = await (await fetch(`http://localhost:${PORT}/src/sugarss.sss?direct`)).text();
  assert.match(sss, /\.sugarss\s*\{\s*color:\s*(blue|#00f)/, `[dev] the sheet parsed with sugarss:\n${sss}`);
  for (const cls of ["sugarss-at-import", "sugarss-at-import-alias"]) {
    assert.match(
      sss,
      new RegExp(`\\.${cls}\\s*\\{[^}]*url\\("?/src/nested/icon\\.png`),
      `[dev] .${cls}: imported .sss parsed, its url() resolved against its own file:\n${sss}`,
    );
  }
  assert.match(await (await fetch(`http://localhost:${PORT}/src/a.pcss?direct`)).text(), /\.pc\s*\{/);
  assert.match(await (await fetch(`http://localhost:${PORT}/src/b.postcss?direct`)).text(), /\.po\s*\{/);
  assert.match(
    await (await fetch(`http://localhost:${PORT}/src/c.module.pcss`)).text(),
    /"card":"_card_[a-z0-9]{5}_1"/,
    "[dev] a .module.pcss is a CSS module",
  );
  const dev = await (await fetch(`http://localhost:${PORT}/src/d.module.sss`)).text();
  const devNames = [...dev.matchAll(/"(tile|label)":"([^"]+)"/g)].map((m) => m[2]);
  assert.equal(devNames.length, 2, `[dev] a .module.sss is a CSS module:\n${dev}`);
  console.log("[dev] .sss, .pcss and .postcss served as stylesheets");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const sheet = (dir) =>
    fs
      .readdirSync(path.join(app, dir, "assets"))
      .filter((f) => f.endsWith(".css"))
      .map((f) => [f, fs.readFileSync(path.join(app, dir, "assets", f), "utf8")]);
  const built = sheet("dist");
  const css = built.map(([, c]) => c).join("\n");
  assert.match(
    css,
    /\.sugarss-at-import,\.sugarss-at-import-alias\{color:#483d8b;background:url\(data:image\/png;base64,/,
    css,
  );
  assert.match(css, /\.sugarss\{color:#00f\}/, css);
  assert.match(css, /\.pc\{color:pink\}/, css);
  assert.match(css, /\.po\{color:peru\}/, css);
  for (const name of devNames) assert.ok(css.includes(`.${name}{`), `[build] ${name} as in dev:\n${css}`);
  console.log("[build] .sss, .pcss and .postcss compiled into the stylesheet; .module.sss names agree with dev");

  const viteBin = path.join(repo, "e2e", "fixtures", "start-app", "node_modules", "vite", "bin", "vite.js");
  if (fs.existsSync(viteBin)) {
    execSync(`node ${viteBin} build --outDir dist-vite --logLevel error`, { cwd: app, stdio: "inherit" });
    assert.deepEqual(built, sheet("dist-vite"), "the stylesheet Vite writes, name and bytes");
    console.log("[vite] byte-identical to a live Vite build");
  } else {
    console.log("[vite] fixture Vite not installed; skipped the live comparison");
  }
  console.log("CSS-POSTCSS-LANGS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-POSTCSS-LANGS E2E FAILED:", err.message);
} finally {
  srv?.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
