// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// CSS module class names are Vite's, in dev and in the build: postcss-modules'
// default `_${local}_${hash}_${line}`, the hash over the sheet Vite hands
// PostCSS (a Sass module's over the Sass output), the line of the first
// `.${local}` in it. The expected names were recorded from Vite 8.3.5; when
// the start-app fixture's Vite is installed, the plain-CSS modules are also
// built with it here and must agree.

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
const PORT = 6372;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const SHEETS = {
  "counter.module.css": ".button { color: red; }\n",
  "multi.module.css":
    ".a { color: red; }\n\n.b { color: blue; }\n.c { composes: a; }\n#main { color: green; }\n" +
    "@keyframes spin { to { opacity: 1 } }\n.d { animation: spin 1s; }\n",
  "crlf.module.css": ".x {\r\n  color: red;\r\n}\r\n.y { color: blue; }\r\n",
  "prefix.module.css": ".btn-primary { color: red; }\n.btn { color: blue; }\n",
  "sass.module.scss": "$c: red;\n.s { color: $c; }\n",
};
const VITE = {
  "counter.module.css": { button: "_button_hwshl_1" },
  "multi.module.css": {
    a: "_a_hn2ce_1",
    b: "_b_hn2ce_3",
    c: "_c_hn2ce_4 _a_hn2ce_1",
    main: "_main_hn2ce_1",
    d: "_d_hn2ce_7",
    spin: "_spin_hn2ce_1",
  },
  "crlf.module.css": { x: "_x_1kb7v_1", y: "_y_1kb7v_7" },
  "prefix.module.css": { "btn-primary": "_btn-primary_n9bj6_1", btn: "_btn_n9bj6_1" },
  "sass.module.scss": { s: "_s_76bxl_1" },
};

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-css-module-names-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "css-module-names", type: "module" }));
for (const [file, css] of Object.entries(SHEETS)) fs.writeFileSync(path.join(app, "src", file), css);
const names = Object.keys(SHEETS);
fs.writeFileSync(
  path.join(app, "src", "maps.js"),
  names.map((f, i) => `import m${i} from "./${f}";\n`).join("") +
    `export default { ${names.map((f, i) => `${JSON.stringify(f)}: m${i}`).join(", ")} };\n`,
);
fs.writeFileSync(path.join(app, "src", "main.js"), `import maps from "./maps.js";\nwindow.__MAPS = maps;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
let srv;
try {
  // Vite itself, when the fixture has it: the plain-CSS modules' maps from
  // an SSR build of the same files.
  const viteBin = path.join(repo, "e2e", "fixtures", "start-app", "node_modules", "vite", "bin", "vite.js");
  if (fs.existsSync(viteBin)) {
    const live = path.join(app, "vite-live");
    fs.mkdirSync(path.join(live, "src"), { recursive: true });
    fs.symlinkSync(
      path.join(repo, "e2e", "fixtures", "start-app", "node_modules"),
      path.join(live, "node_modules"),
      "dir",
    );
    fs.writeFileSync(path.join(live, "package.json"), JSON.stringify({ name: "live", type: "module" }));
    const plain = names.filter((f) => f.endsWith(".css"));
    for (const f of plain) fs.writeFileSync(path.join(live, "src", f), SHEETS[f]);
    fs.writeFileSync(
      path.join(live, "src", "maps.js"),
      plain.map((f, i) => `import m${i} from "./${f}";\n`).join("") +
        `export default { ${plain.map((f, i) => `${JSON.stringify(f)}: m${i}`).join(", ")} };\n`,
    );
    execSync(`node ${viteBin} build --ssr src/maps.js --outDir out --logLevel error`, { cwd: live, stdio: "inherit" });
    const { default: maps } = await import(path.join(live, "out", "maps.js"));
    for (const f of plain) assert.deepEqual(maps[f], VITE[f], `${f}: the recorded names are what this Vite gives`);
    console.log("[vite] recorded names match a live Vite build");
  } else {
    console.log("[vite] fixture Vite not installed; using the recorded names only");
  }

  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  for (const f of names) {
    const mod = await (await fetch(`http://localhost:${PORT}/src/${f}`)).text();
    const map = JSON.parse(/export default (\{.*?\});/s.exec(mod)?.[1] ?? "null");
    assert.deepEqual(map, VITE[f], `[dev] ${f} class map:\n${mod}`);
  }
  console.log("[dev] class maps are Vite's");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assets = path.join(app, "dist", "assets");
  const read = (ext) =>
    fs
      .readdirSync(assets)
      .filter((f) => f.endsWith(ext))
      .map((f) => fs.readFileSync(path.join(assets, f), "utf8"))
      .join("\n");
  const css = read(".css");
  const js = read(".js");
  for (const [f, map] of Object.entries(VITE)) {
    for (const [local, value] of Object.entries(map)) {
      const own = value.split(" ")[0];
      assert.ok(js.includes(value), `[build] ${f}: ${local} -> "${value}" in the bundle`);
      // `.c` only composes: its rule is empty and minified away, as in Vite.
      if (local !== "c") assert.ok(css.includes(own), `[build] ${f}: ${own} in the stylesheet:\n${css}`);
    }
  }
  assert.ok(!/__oj_scoped__/.test(css + js), "no unrenamed marker left");
  console.log("[build] class maps and selectors are Vite's");
  console.log("CSS-MODULES-VITE-NAMES E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-MODULES-VITE-NAMES E2E FAILED:", err.message);
} finally {
  srv?.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
