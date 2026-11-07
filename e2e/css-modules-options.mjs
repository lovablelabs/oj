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
import { rmrf, waitUp } from "./util.mjs";

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
  check(
    `${mode}: camelCase adds the converted key next to the original`,
    styles["my-button"] && styles.myButton === styles["my-button"],
    JSON.stringify(styles),
  );
  // generic-names over `[local]__[hash:base64:5]`, exactly as Vite 8.3.5
  // names these (the hash covers the module's path and the local).
  check(
    `${mode}: generateScopedName pattern gives Vite's names`,
    styles["my-button"] === "my-button__HuMui" && styles.wide === "wide__P5f5e base__bYZhS",
    JSON.stringify(styles),
  );
  check(
    `${mode}: composes from another module file`,
    typeof styles.wide === "string" && styles.wide.split(" ").length === 2 && /^base__/.test(styles.wide.split(" ")[1]),
    JSON.stringify(styles),
  );
  check(
    `${mode}: globalModulePaths file exports no locals`,
    theme && Object.keys(theme).length === 0,
    JSON.stringify(theme),
  );
  check(`${mode}: globalModulePaths file is unscoped in the CSS`, /\.theme-dark\s*{/.test(css), css);
}

// Dev: the JS wrappers of the two modules plus the served CSS.
{
  const port = 6304;
  // From the app, as Vite ran for the recorded names: a pattern's hash
  // covers the file's path from the directory it was started in.
  const srv = spawn(oj, ["dev", app, "--port", String(port)], { cwd: app, stdio: ["ignore", "pipe", "pipe"] });
  let log = "";
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  try {
    let up = false;
    up = await waitUp(`http://localhost:${port}/`).then(
      () => true,
      () => false,
    );
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
    // Wait it out: a server still exiting writes its code cache into the app
    // dir the cleanup below removes.
    if (srv.exitCode === null && srv.signalCode === null) {
      const exited = new Promise((resolve) => srv.once("exit", resolve));
      srv.kill();
      await exited;
    }
  }
}

// Build: the bundled chunk and the emitted stylesheet.
{
  const r = spawnSync(oj, ["build", app], { cwd: app, encoding: "utf8" });
  check("build succeeds", r.status === 0, `${r.stdout}\n${r.stderr}`);
  if (r.status === 0) {
    const assets = path.join(app, "dist", "assets");
    const js = fs
      .readdirSync(assets)
      .filter((f) => f.endsWith(".js"))
      .map((f) => fs.readFileSync(path.join(assets, f), "utf8"))
      .join("\n");
    const css = fs
      .readdirSync(assets)
      .filter((f) => f.endsWith(".css"))
      .map((f) => fs.readFileSync(path.join(assets, f), "utf8"))
      .join("\n");
    check("build: myButton camelCase key is in the chunk", /myButton/.test(js), js.slice(0, 400));
    check("build: scoped names are Vite's", js.includes("my-button__HuMui") && css.includes(".my-button__HuMui{"), css);
    check(
      "build: composes value has two classes",
      /wide"?:["'`]wide__[^"'` ]+ base__[^"'` ]+["'`]/.test(js),
      js.slice(-400),
    );
    check("build: globalModulePaths file is unscoped", /\.theme-dark{/.test(css), css);
  }
}

// Started from the app's parent, a pattern's hash covers `<app>/src/...`
// (postcss-modules' context is `process.cwd()`): oj names the classes as
// Vite does from there.
{
  const viteBin = path.join(repo, "e2e", "fixtures", "start-app", "node_modules", "vite", "bin", "vite.js");
  if (fs.existsSync(viteBin)) {
    fs.writeFileSync(
      path.join(app, "vite.config.js"),
      `export default { css: { modules: { localsConvention: "camelCase", generateScopedName: "[local]__[hash:base64:5]", globalModulePaths: [/\\.global\\.module\\.css$/] } } };\n`,
    );
    const parent = path.dirname(app);
    const named = (dir) => {
      const assets = path.join(app, dir, "assets");
      const css = fs
        .readdirSync(assets)
        .filter((f) => f.endsWith(".css"))
        .map((f) => fs.readFileSync(path.join(assets, f), "utf8"))
        .join("\n");
      return /\.(my-button__[A-Za-z0-9_-]{5})\{/.exec(css)?.[1];
    };
    const o = spawnSync(oj, ["build", app], { cwd: parent, encoding: "utf8" });
    check("build from the parent succeeds", o.status === 0, `${o.stdout}\n${o.stderr}`);
    const v = spawnSync(process.execPath, [viteBin, "build", app, "--outDir", "dist-vite", "--logLevel", "error"], {
      cwd: parent,
      encoding: "utf8",
    });
    check("vite build from the parent succeeds", v.status === 0, `${v.stdout}\n${v.stderr}`);
    if (o.status === 0 && v.status === 0) {
      const ojName = named("dist");
      check(
        "build from the parent: Vite's names",
        ojName && ojName === named("dist-vite"),
        `${ojName} vs ${named("dist-vite")}`,
      );
      check("build from the parent: the path is in the hash", ojName !== "my-button__HuMui", ojName);
    }
  } else {
    console.log("skip parent-directory check: fixture Vite not installed");
  }
}

await rmrf(app);
if (failed) process.exit(1);
console.log("CSS-MODULES-OPTIONS E2E PASSED");
