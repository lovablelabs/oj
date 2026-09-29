// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Every way of importing a stylesheet ships the COMPILED css (Vite parity):
// `?url` is the URL of a compiled `.css` (dev: `?direct` text, build: an emitted
// asset), `?inline` is the compiled text in both, and an aliased plain `.svg`
// import is a URL module even with no svgr plugin configured.

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
const PORT = 5540;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cssurl-"));
fs.mkdirSync(path.join(app, "src", "assets"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "cssurl-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.js"),
  `export default { resolve: { alias: { "@": ${JSON.stringify(path.join(app, "src"))} } } };\n`,
);
fs.writeFileSync(path.join(app, "src", "a.scss"), `$c: red;\n.a { color: $c; }\n`);
fs.writeFileSync(path.join(app, "src", "b.css"), `.b {\n  color: blue;\n}\n`);
fs.writeFileSync(
  path.join(app, "src", "assets", "logo.svg"),
  `<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4"/></svg>`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import aUrl from "./a.scss?url";\nimport bInline from "./b.css?inline";\nimport logo from "@/assets/logo.svg";\n` +
    `window.__A = aUrl; window.__B = bInline; window.__LOGO = logo;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const main = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  assert.match(main, /"\/src\/assets\/logo\.svg\?url"/, `aliased svg not marked as an asset:\n${main}`);

  const aUrlMod = await (await fetch(`http://localhost:${PORT}/src/a.scss?url`)).text();
  assert.match(aUrlMod, /export default "\/src\/a\.scss\?direct"/, `scss ?url module:\n${aUrlMod}`);
  const direct = await fetch(`http://localhost:${PORT}/src/a.scss?direct`);
  assert.match(direct.headers.get("content-type"), /text\/css/, "?direct is css text");
  const directCss = await direct.text();
  assert.match(directCss, /color:\s*red/, `?direct is compiled sass:\n${directCss}`);
  assert.doesNotMatch(directCss, /\$c/, "sass variable left in ?direct output");
  // A <link href="/src/a.scss"> request (sec-fetch-dest: style) also gets compiled css.
  const linked = await (
    await fetch(`http://localhost:${PORT}/src/a.scss`, { headers: { "sec-fetch-dest": "style" } })
  ).text();
  assert.match(linked, /color:\s*red/, `link request not compiled:\n${linked}`);

  const bInline = await (await fetch(`http://localhost:${PORT}/src/b.css?inline`)).text();
  assert.match(bInline, /export default "\.b\{color:blue\}|export default ".b \{/, `css ?inline in dev:\n${bInline}`);

  // The svg module fetched the way the browser fetches an import (no image dest).
  const logoMod = await (
    await fetch(`http://localhost:${PORT}/src/assets/logo.svg`, { headers: { "sec-fetch-dest": "script" } })
  ).text();
  assert.match(logoMod, /export default "\/src\/assets\/logo\.svg"/, `svg import is not a URL module:\n${logoMod}`);
  // ...while an <img> request still gets the image.
  const img = await fetch(`http://localhost:${PORT}/src/assets/logo.svg`, { headers: { "sec-fetch-dest": "image" } });
  assert.match(img.headers.get("content-type"), /image\/svg\+xml/, "img request still serves the svg");
  console.log("[dev] compiled ?url/?direct/?inline and svg URL module OK");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assets = path.join(app, "dist", "assets");
  const files = fs.readdirSync(assets);
  const js = fs.readFileSync(
    path.join(
      assets,
      files.find((f) => f.startsWith("main-") && f.endsWith(".js")),
    ),
    "utf8",
  );
  const cssFile = files.find((f) => f.startsWith("a-") && f.endsWith(".css"));
  assert.ok(cssFile, `compiled a.css asset emitted for ?url: ${files}`);
  const css = fs.readFileSync(path.join(assets, cssFile), "utf8");
  assert.match(css, /color:\s*red/, `?url asset is compiled sass:\n${css}`);
  assert.doesNotMatch(css, /\$c/, "sass source shipped as the ?url asset");
  assert.match(
    js,
    new RegExp(cssFile.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")),
    "main chunk references the css asset url",
  );
  assert.match(js, /\.b\{color:(blue|#00f)\}/, `?inline is compiled css text in the build:\n${js}`);
  assert.doesNotMatch(js, /application\/octet-stream/, "css ?inline shipped as an octet-stream data URI");
  assert.match(js, /data:image\/svg\+xml|assets\/logo-/, "aliased svg is an asset url in the build");
  console.log("[build] compiled ?url asset and ?inline text OK");
  console.log("CSS-URL-INLINE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-URL-INLINE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
