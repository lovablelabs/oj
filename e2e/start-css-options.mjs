// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The app's css and build options reach TanStack Start's stylesheets as they
// reach Vite's (css.ts compileCSS and minifyCSS):
// - css.preprocessorOptions.scss.additionalData feeds a Sass CSS module, in
//   dev SSR, the dev client and the build alike, under one scoped class;
// - the build minifies emitted sheets (build.cssMinify, on by default) with
//   css.lightningcss spread in (unusedSymbols, pseudoClasses), and dev never
//   minifies, so css.lightningcss stays out of it;
// - build.cssMinify: false leaves the emitted sheets readable.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5399);

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start css options: fixture deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const rm = (p) => {
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(p, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) throw e;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};
const filesUnder = (dir, ext) => {
  if (!fs.existsSync(dir)) return [];
  return fs
    .readdirSync(dir, { withFileTypes: true, recursive: true })
    .filter((e) => e.isFile() && e.name.endsWith(ext))
    .map((e) => fs.readFileSync(path.join(e.parentPath ?? e.path, e.name), "utf8"));
};
const carries = (code, cls) => ['"', "'", "`"].some((q) => code.includes(q + cls + q));
const brandClass = (html) => /<p class="([^"]*)" data-css-module="brand">/.exec(html)?.[1] ?? "";

const routes = path.join(app, "src", "routes");
const aboutFile = path.join(routes, "about.tsx");
const originalAbout = fs.readFileSync(aboutFile, "utf8");
const files = {
  [path.join(routes, "brand.module.scss")]: `.brand { color: $brand; }\n`,
  [path.join(routes, "extra.css")]: `.kept { color: red; }\n.gone { color: blue; }\n.c:hover { color: green; }\n`,
  [aboutFile]: `import { createRoute } from "@tanstack/react-router";

import brand from "./brand.module.scss";
import "./extra.css";
import { rootRoute } from "./__root";

export const aboutRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/about",
  component: About,
});

function About() {
  return (
    <main>
      <h1 className="fixture-heading">about-page-marker</h1>
      <p className={brand.brand} data-css-module="brand">css-options-brand</p>
      <p className="kept c">css-options-extra</p>
    </main>
  );
}
`,
};
// The fixture's config plus the css options under test, and a variant with
// build.cssMinify off.
const CSS = `{
  preprocessorOptions: { scss: { additionalData: "$brand: rgb(21, 22, 23);" } },
  lightningcss: { unusedSymbols: ["gone"], pseudoClasses: { hover: "is-hover" } },
}`;
const configFile = path.join(app, "vite.css-options.config.ts");
const noMinifyFile = path.join(app, "vite.css-options-nominify.config.ts");
files[configFile] = `import base from "./vite.config";\nexport default { ...base, css: ${CSS} };\n`;
files[noMinifyFile] =
  `import base from "./vite.config";\nexport default { ...base, css: ${CSS}, build: { cssMinify: false } };\n`;

const dist = path.join(app, "dist");
const build = (config) => {
  rm(dist);
  rm(path.join(app, ".oj-cache"));
  execSync(`${oj} build ${app} --config ${config}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });
  return { js: filesUnder(path.join(dist, "client"), ".js"), css: filesUnder(path.join(dist, "client"), ".css") };
};

let srv;
let log = "";
try {
  for (const [file, source] of Object.entries(files)) fs.writeFileSync(file, source);
  rm(path.join(app, ".oj-cache"));

  // Dev: additionalData applies on the server and the client; nothing is
  // minified, so css.lightningcss leaves the sheet alone.
  srv = spawn(oj, ["dev", app, "--port", String(PORT), "--config", configFile], { stdio: ["ignore", "pipe", "pipe"] });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);
  let html = "";
  await settles(async () => {
    try {
      const res = await fetch(`http://localhost:${PORT}/about`);
      html = await res.text();
      return res.status === 200 && html.includes("css-options-brand");
    } catch {
      return false;
    }
  });
  const devClass = brandClass(html);
  must(
    devClass.includes("brand") && devClass !== "brand",
    `dev SSR rendered no scoped class: "${devClass}"\n${log.slice(-1500)}`,
  );
  must(
    filesUnder(path.join(app, ".oj-cache"), ".js").some((c) => carries(c, devClass)),
    `dev client bundle does not carry "${devClass}"`,
  );
  const hrefs = [...html.matchAll(/<link[^>]*rel="stylesheet"[^>]*href="([^"]+)"/g)].map((m) => m[1]);
  const sheets = await Promise.all(
    hrefs.map(async (h) => (await fetch(new URL(h, `http://localhost:${PORT}/`))).text()),
  );
  must(
    sheets.some((css) => css.includes(`.${devClass}`) && /rgb\(21, 22, 23\)|#151617/.test(css)),
    `no dev sheet carries .${devClass} with the additionalData color: ${hrefs.join(", ")}`,
  );
  must(
    sheets.some((css) => css.includes(".gone") && css.includes(":hover")),
    "dev applied css.lightningcss (Vite only applies it to the build minify)",
  );
  console.log(`start dev: ${devClass} compiled with additionalData, css.lightningcss left to the build`);
  srv.kill("SIGKILL");

  // Build, default cssMinify: minified with css.lightningcss, same class.
  const minified = build(configFile);
  const built = spawn("node", [path.join(dist, "server.mjs")], {
    cwd: app,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, PORT: String(PORT) },
  });
  try {
    await waitUp(`http://localhost:${PORT}/about`);
    const page = await (await fetch(`http://localhost:${PORT}/about`)).text();
    must(brandClass(page) === devClass, `built SSR rendered "${brandClass(page)}", dev rendered "${devClass}"`);
  } finally {
    built.kill("SIGKILL");
  }
  must(
    minified.js.some((c) => carries(c, devClass)),
    `built client bundle does not carry "${devClass}"`,
  );
  must(
    minified.css.some((css) => css.includes(`.${devClass}{color:#151617}`)),
    `no built sheet carries the minified .${devClass}:\n${minified.css.join("\n---\n")}`,
  );
  const extra = minified.css.find((css) => css.includes(".kept")) ?? "";
  must(
    extra.includes(".kept{color:red}") && !extra.includes(".gone") && extra.includes(".c.is-hover{"),
    `built extra.css missed css.lightningcss: ${JSON.stringify(extra)}`,
  );
  must(
    minified.css.every((css) => css.endsWith("\n")),
    "a built sheet does not end in a newline",
  );
  console.log("start build: minified with css.lightningcss");

  // Build, cssMinify off: readable, css.lightningcss not applied.
  const readable = build(noMinifyFile);
  const extraReadable = readable.css.find((css) => css.includes(".kept")) ?? "";
  must(
    extraReadable.includes(".kept { color: red; }") && extraReadable.includes(".gone"),
    `build.cssMinify false still minified: ${JSON.stringify(extraReadable)}`,
  );
  console.log("start build: cssMinify false keeps sheets readable");
  console.log("\nSTART CSS OPTIONS PASSED");
} catch (e) {
  console.error("\nSTART CSS OPTIONS FAILED:", e.message);
  process.exitCode = 1;
} finally {
  for (const file of Object.keys(files)) if (file !== aboutFile) rm(file);
  fs.writeFileSync(aboutFile, originalAbout);
  rm(dist);
  srv?.kill("SIGKILL");
}
