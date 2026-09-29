// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// css.modules options (Vite's postcss-modules options) in dev and build:
// localsConvention shapes the export map, generateScopedName sets the class
// pattern, globalModulePaths (RegExp) compiles matching module files unscoped,
// and `composes ... from "./other.module.css"` resolves through the other
// module. Run with a built target/debug/oj.
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-css-modules-options-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "css-modules-options", type: "module" }));
fs.writeFileSync(
  path.join(app, "oj.config.ts"),
  `export default {
  css: {
    modules: {
      localsConvention: "camelCase",
      generateScopedName: "[local]__[hash:base64:5]",
      globalModulePaths: [/\\.global\\.module\\.css$/],
    },
  },
};
`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head></head><body><div id="app"></div><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import styles from "./app.module.css";
import theme from "./theme.global.module.css";
window.__STYLES = styles;
window.__THEME = theme;
document.getElementById("app").textContent = JSON.stringify({ styles, theme });
`,
);
fs.writeFileSync(path.join(app, "src", "base.module.css"), `.base { padding: 1px }\n`);
fs.writeFileSync(
  path.join(app, "src", "app.module.css"),
  `.my-button { color: red }\n.wide { composes: base from "./base.module.css"; width: 100% }\n`,
);
fs.writeFileSync(path.join(app, "src", "theme.global.module.css"), `.theme-dark { color: white }\n`);

let failed = false;
function check(label, ok, detail) {
  if (!ok) {
    failed = true;
    console.error(`FAIL ${label}: ${detail}`);
  } else {
    console.log(`ok   ${label}`);
  }
}

function checkExports(mode, styles, theme, css) {
  check(`${mode}: camelCase adds the converted key next to the original`, styles["my-button"] && styles.myButton === styles["my-button"], JSON.stringify(styles));
  check(`${mode}: generateScopedName pattern`, /^my-button__[A-Za-z0-9_-]+$/.test(styles["my-button"] ?? ""), JSON.stringify(styles));
  check(`${mode}: composes from another module file`, typeof styles.wide === "string" && styles.wide.split(" ").length === 2 && /^base__/.test(styles.wide.split(" ")[1]), JSON.stringify(styles));
  check(`${mode}: globalModulePaths file exports no locals`, theme && Object.keys(theme).length === 0, JSON.stringify(theme));
  check(`${mode}: globalModulePaths file is unscoped in the CSS`, /\.theme-dark\s*{/.test(css), css);
}

// Dev: the JS wrappers of the two modules plus the served CSS.
{
  const port = 6304;
  const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: ["ignore", "pipe", "pipe"] });
  let log = "";
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  try {
    let up = false;
    up = await waitUp(`http://localhost:${port}/`).then(() => true, () => false);
    if (!up) throw new Error(`dev server did not start:\n${log}`);
    const wrapper = await (await fetch(`http://localhost:${port}/src/app.module.css?import`)).text();
    const map = wrapper.match(/export default (\{.*?\});?\s*$/m);
    const styles = map ? JSON.parse(map[1]) : null;
    const themeWrapper = await (await fetch(`http://localhost:${port}/src/theme.global.module.css?import`)).text();
    const themeMap = themeWrapper.match(/export default (\{.*?\});?\s*$/m);
    const theme = themeMap ? JSON.parse(themeMap[1]) : null;
    const css = await (await fetch(`http://localhost:${port}/src/theme.global.module.css?direct`)).text();
    check("dev: module wrapper carries a default export map", !!styles, wrapper);
    if (styles) checkExports("dev", styles, theme, css);
  } finally {
    srv.kill();
  }
}

// Build: the bundled chunk and the emitted stylesheet.
{
  const r = spawnSync(oj, ["build", app], { cwd: repo, encoding: "utf8" });
  check("build succeeds", r.status === 0, `${r.stdout}\n${r.stderr}`);
  if (r.status === 0) {
    const assets = path.join(app, "dist", "assets");
    const js = fs.readdirSync(assets).filter((f) => f.endsWith(".js")).map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("\n");
    const css = fs.readdirSync(assets).filter((f) => f.endsWith(".css")).map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("\n");
    check("build: myButton camelCase key is in the chunk", /myButton/.test(js), js.slice(0, 400));
    check("build: scoped name follows the pattern", /my-button__[A-Za-z0-9_-]+/.test(js) && /\.my-button__/.test(css), css);
    check("build: composes value has two classes", /wide"?:["'`]wide__[^"'` ]+ base__[^"'` ]+["'`]/.test(js), js.slice(-400));
    check("build: globalModulePaths file is unscoped", /\.theme-dark{/.test(css), css);
  }
}

fs.rmSync(app, { recursive: true, force: true });
if (failed) process.exit(1);
console.log("CSS-MODULES-OPTIONS E2E PASSED");
