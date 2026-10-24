// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// An app's own CSS modules under Start, in every language: the class the
// server renders, the class map the client bundle carries and the stylesheet
// the browser loads must name the same scoped class, in dev and in the
// production build (Vite's compileCSS serves every consumer from one compile:
// the preprocessor, then postcss-modules). A client map that differs from the
// server's breaks hydration, and an unscoped sheet styles nothing. A plain
// .scss sheet must reach the browser compiled.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5397);

const installed = fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start css modules: fixture deps not installed");
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
  const out = [];
  if (!fs.existsSync(dir)) return out;
  for (const e of fs.readdirSync(dir, { withFileTypes: true, recursive: true })) {
    if (e.isFile() && e.name.endsWith(ext)) out.push(path.join(e.parentPath ?? e.path, e.name));
  }
  return out;
};
// The class the page renders on a marked element.
const renderedClass = (html, mark) => new RegExp(`<p class="([^"]*)" data-css-module="${mark}">`).exec(html)?.[1] ?? "";
// A string literal in any quote style (a minified bundle uses backticks).
const carries = (code, cls) => ['"', "'", "`"].some((q) => code.includes(q + cls + q));
const scopedSelector = (css, cls) => new RegExp(`\\.${cls.replace(/[$]/g, "\\$&")}\\s*\\{`).test(css);

// Less runs on the app's own `less` package (Vite's preprocessor contract),
// installed beside the fixture so its shared node_modules stays untouched.
const lessLink = path.join(app, "node_modules", "less");
let lessHome = null;
if (!fs.existsSync(lessLink)) {
  lessHome = fs.mkdtempSync(path.join(os.tmpdir(), "oj-less-"));
  try {
    execSync("npm install less --no-audit --no-fund --loglevel=error", { cwd: lessHome, stdio: "ignore" });
  } catch {
    console.log("SKIP start css modules: could not install less (offline?)");
    process.exit(0);
  }
}

// mark -> [file, source, the class the module exports, a selector its source
// spells unscoped]. The scss and less modules make their class with the
// preprocessor (`&-title`, a variable), so a sheet that skipped it is caught.
const MODULES = {
  card: ["card.module.css", `.card { color: rgb(1, 2, 3); }\n`, "card", /\.card\s*\{/],
  panel: [
    "panel.module.scss",
    `$c: rgb(4, 5, 6);\n.panel { &-title { color: $c; } }\n`,
    "panel-title",
    /\.panel-title\s*\{|&-title/,
  ],
  badge: ["badge.module.less", `@c: rgb(7, 8, 9);\n.badge { color: @c; }\n`, "badge", /\.badge\s*\{/],
};
// The names Vite gives these classes (postcss-modules' default over the
// preprocessed sheet, recorded from Vite 8.3.5 with sass and less).
const VITE_NAMES = { card: "_card_k2p0w_1", panel: "_panel-title_1h7bj_1", badge: "_badge_lh6u3_1" };
const PLAIN = ["theme.scss", `$t: rgb(10, 11, 12);\n.scss-plain { color: $t; }\n`];
// Imported `?inline`: the compiled sheet as a string (Vite's css.ts), its
// relative url() pointed at the served image.
const INLINE = ["inline.scss", `$i: rgb(13, 14, 15);\n.inline-box { color: $i; background: url(../hero.png); }\n`];

const routes = path.join(app, "src", "routes");
const aboutFile = path.join(routes, "about.tsx");
const originalAbout = fs.readFileSync(aboutFile, "utf8");
const patchedAbout = `import { createRoute } from "@tanstack/react-router";

import card from "./card.module.css";
import panel from "./panel.module.scss";
import badge from "./badge.module.less";
import "./theme.scss";
import inlineCss from "./inline.scss?inline";
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
      <p className={card.card} data-css-module="card">css-module-card</p>
      <p className={panel["panel-title"]} data-css-module="panel">css-module-panel</p>
      <p className={badge.badge} data-css-module="badge">css-module-badge</p>
      <style data-inline-style="x" dangerouslySetInnerHTML={{ __html: inlineCss }} />
    </main>
  );
}
`;

// Every module's class scoped and agreeing across the rendered page, the
// client code and the sheets; the plain scss sheet compiled.
function checkStyles(where, html, js, sheets) {
  const classes = {};
  for (const [mark, [file, , , unscoped]] of Object.entries(MODULES)) {
    const cls = renderedClass(html, mark);
    must(cls && cls.includes(mark), `${where}: ${file} rendered no scoped class: "${cls}"`);
    must(cls === VITE_NAMES[mark], `${where}: ${file} is "${cls}", Vite names it "${VITE_NAMES[mark]}"`);
    must(
      js.some((c) => carries(c, cls)),
      `${where}: client bundle does not carry "${cls}" (${file})`,
    );
    must(
      sheets.some((css) => scopedSelector(css, cls)),
      `${where}: no stylesheet scopes .${cls} (${file})`,
    );
    must(!sheets.some((css) => unscoped.test(css)), `${where}: a stylesheet still carries ${file} unscoped`);
    classes[mark] = cls;
  }
  must(
    sheets.some((css) => /\.scss-plain\s*\{\s*color:\s*(rgb\(10, 11, 12\)|#0a0b0c)/.test(css)),
    `${where}: no stylesheet carries the compiled ${PLAIN[0]}`,
  );
  must(!sheets.some((css) => css.includes("$t")), `${where}: a stylesheet carries uncompiled scss`);
  return classes;
}

// The `?inline` sheet the page renders: compiled, its image url the one the
// environment serves, and the same text in the client code that hydrates it.
function checkInline(where, html, js, imageUrl) {
  const css = /<style data-inline-style="x">([\s\S]*?)<\/style>/.exec(html)?.[1] ?? "";
  must(/\.inline-box\s*\{/.test(css), `${where}: no inline sheet rendered:\n${html.slice(0, 1200)}`);
  must(/rgb\(13, 14, 15\)|#0d0e0f/.test(css) && !css.includes("$i"), `${where}: inline sheet not compiled: ${css}`);
  const url = /url\("?([^")]+)"?\)/.exec(css)?.[1] ?? "";
  must(imageUrl(url), `${where}: inline sheet's image url is "${url}"`);
  // The exact text, as a double-quoted or (minified) template literal.
  const literals = [JSON.stringify(css), "`" + css.replace(/[`\\$]/g, "\\$&") + "`"];
  must(
    js.some((c) => literals.some((l) => c.includes(l))),
    `${where}: client bundle does not carry the rendered inline sheet:\n${css}`,
  );
  return css;
}

let srv;
let log = "";
try {
  if (lessHome) fs.symlinkSync(path.join(lessHome, "node_modules", "less"), lessLink);
  for (const [file, source] of Object.values(MODULES)) fs.writeFileSync(path.join(routes, file), source);
  fs.writeFileSync(path.join(routes, PLAIN[0]), PLAIN[1]);
  fs.writeFileSync(path.join(routes, INLINE[0]), INLINE[1]);
  fs.writeFileSync(aboutFile, patchedAbout);
  rm(path.join(app, ".oj-cache"));

  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);
  let html = "";
  await settles(async () => {
    try {
      const res = await fetch(`http://localhost:${PORT}/about`);
      html = await res.text();
      return res.status === 200 && html.includes("css-module-badge");
    } catch {
      return false;
    }
  });
  must(
    html.includes("css-module-badge"),
    `dev /about did not render:\n${html.slice(0, 800)}\n---\n${log.slice(-1500)}`,
  );
  const chunks = filesUnder(path.join(app, ".oj-cache"), ".js").map((f) => fs.readFileSync(f, "utf8"));
  const hrefs = [...html.matchAll(/<link[^>]*rel="stylesheet"[^>]*href="([^"]+)"/g)].map((m) => m[1]);
  const sheets = await Promise.all(
    hrefs.map(async (h) => (await fetch(new URL(h, `http://localhost:${PORT}/`))).text()),
  );
  const devClasses = checkStyles("dev", html, chunks, sheets);
  const hero = fs.realpathSync(path.join(app, "src", "hero.png"));
  checkInline("dev", html, chunks, (u) => u === `/@oj-start/fs${hero}`);
  console.log(`start dev: ${Object.values(devClasses).join(", ")} agree on server, client and sheet`);
  srv.kill("SIGKILL");

  const dist = path.join(app, "dist");
  rm(dist);
  rm(path.join(app, ".oj-cache"));
  execSync(`${oj} build ${app}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });
  const builtJs = filesUnder(path.join(dist, "client"), ".js").map((f) => fs.readFileSync(f, "utf8"));
  const builtCss = filesUnder(path.join(dist, "client"), ".css").map((f) => fs.readFileSync(f, "utf8"));
  must(
    !filesUnder(path.join(dist, "client"), ".scss").length && !filesUnder(path.join(dist, "client"), ".less").length,
    "the build emitted a preprocessor file instead of css",
  );
  const built = spawn("node", [path.join(dist, "server.mjs")], {
    cwd: app,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, PORT: String(PORT) },
  });
  let builtLog = "";
  built.stdout.on("data", (d) => (builtLog += d));
  built.stderr.on("data", (d) => (builtLog += d));
  try {
    await waitUp(`http://localhost:${PORT}/about`);
    const res = await fetch(`http://localhost:${PORT}/about`);
    const page = await res.text();
    must(
      res.status === 200,
      `built /about returned ${res.status}:\n${page.slice(0, 800)}\n---\n${builtLog.slice(-1200)}`,
    );
    const buildClasses = checkStyles("build", page, builtJs, builtCss);
    const inlined = checkInline("build", page, builtJs, (u) => /^\/assets\/hero-[A-Za-z0-9_-]{8}\.png$/.test(u));
    // Vite's build.cssMinify (on by default for both environments): emitted
    // sheets and inlined text are minified.
    must(!/\s{2}|\n/.test(inlined), `build: inline sheet not minified: ${JSON.stringify(inlined)}`);
    must(
      builtCss.length && builtCss.every((css) => !/\{\n\s/.test(css) && css.endsWith("}\n")),
      "build: an emitted stylesheet is not minified (one line, ending in a newline)",
    );
    for (const mark of Object.keys(MODULES)) {
      must(
        buildClasses[mark] === devClasses[mark],
        `built SSR rendered "${buildClasses[mark]}", dev rendered "${devClasses[mark]}"`,
      );
    }
  } finally {
    built.kill("SIGKILL");
    rm(dist);
  }
  console.log("\nSTART CSS MODULES PASSED");
} catch (e) {
  console.error("\nSTART CSS MODULES FAILED:", e.message);
  process.exitCode = 1;
} finally {
  fs.writeFileSync(aboutFile, originalAbout);
  for (const [file] of Object.values(MODULES)) rm(path.join(routes, file));
  rm(path.join(routes, PLAIN[0]));
  rm(path.join(routes, INLINE[0]));
  if (lessHome) {
    rm(lessLink);
    rm(lessHome);
  }
  srv?.kill("SIGKILL");
}
