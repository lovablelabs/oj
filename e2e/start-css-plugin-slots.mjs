// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Under TanStack Start, a stylesheet's plugin transforms run in Vite's slots
// in each environment: "pre" plugins see the source, "normal" ones the CSS
// vite:css makes of it (a CSS module's classes scoped), "post" ones the
// module vite:css-post makes of that. Plugins record what they see per
// environment in oj's dev server and build; the same app then runs under
// Vite's dev server and build, and the records must match, each sheet once
// per slot and environment (in dev, a sheet's `<link>` serves what its
// client import ran through the plugins, as Vite serves both from one
// transform). An edit still reaches the linked sheet.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath, pathToFileURL } from "node:url";
import { sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(repo, "e2e", "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6378;
const VITE_PORT = 6379;

if (!fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start css plugin slots: fixture deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-slots-"));
const seenFile = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-slots-seen-")), "seen.txt");
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
write(
  "package.json",
  JSON.stringify({
    name: "start-slots",
    private: true,
    type: "module",
    dependencies: { "@tanstack/react-router": "*", "@tanstack/react-start": "*", react: "*", "react-dom": "*" },
    devDependencies: { vite: "*", "@vitejs/plugin-react": "*", less: "*" },
  }),
);
write(
  "tsconfig.json",
  JSON.stringify({
    compilerOptions: { jsx: "react-jsx", module: "ESNext", moduleResolution: "Bundler", strict: true },
  }),
);
write(
  "src/router.tsx",
  `import { createRouter } from "@tanstack/react-router";\nimport { routeTree } from "./routeTree.gen";\n\nexport function getRouter() {\n  return createRouter({ routeTree });\n}\n`,
);
write(
  "src/routes/__root.tsx",
  `import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "slots" }] }),
  component: () => (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  ),
});
`,
);
write(
  "src/routes/index.tsx",
  `import { createFileRoute } from "@tanstack/react-router";
import "../a.less";
import b from "../b.less?inline";
import m from "../m.module.css";
import "virtual:theme.css";
import u from "../u.less?url";

export const Route = createFileRoute("/")({
  component: () => (
    <p className={m.m} data-inline={b.length} data-url={u}>
      slots
    </p>
  ),
});
`,
);
write("src/a.less", "@c: red;\n.a { color: @c; }\n");
write("src/b.less", "@c: blue;\n.b { color: @c; }\n");
write("src/m.module.css", ".m { color: teal; }\n");
write("src/u.less", "@c: green;\n.u { color: @c; }\n");
write(
  "spies.mjs",
  `import { appendFileSync } from "node:fs";
const record = (name) => function (code, id) {
  if (!/\\.(less|css)(\\?|$)/.test(id)) return null;
  const kind = code.includes("@c:")
    ? "source"
    : /export (default|const|\\{)|^$/.test(code) || code.startsWith("import")
      ? "js"
      : /\\.m[\\s{]/.test(code) || !id.includes(".module.")
        ? "css"
        : "css-scoped";
  appendFileSync(${JSON.stringify(seenFile)}, \`\${this.environment?.name} \${name} \${id.slice(id.lastIndexOf("/") + 1)} \${kind}\\n\`);
  return null;
};
const spy = (name, enforce) => ({ name, enforce, transform: record(name) });
const virtualCss = {
  name: "virtual-css",
  resolveId(id) { if (id === "virtual:theme.css") return "\\0virtual:theme.css"; },
  load(id) { if (id === "\\0virtual:theme.css") return ".from-plugin { color: rebeccapurple; }\\n"; },
};
export default [spy("post", "post"), spy("normal"), spy("pre", "pre"), virtualCss];
`,
);
write(
  "vite.config.ts",
  `import { tanstackStart } from "@tanstack/react-start/plugin/vite";\nimport react from "@vitejs/plugin-react";\nimport { defineConfig } from "vite";\nimport spies from "./spies.mjs";\n\nexport default defineConfig({ plugins: [...spies, tanstackStart(), react()] });\n`,
);

const records = () => (fs.existsSync(seenFile) ? fs.readFileSync(seenFile, "utf8").split("\n").filter(Boolean) : []);
const sorted = (lines) => [...lines].sort();
const reset = () => fs.rmSync(seenFile, { force: true });
const CLIENT_MODULES = [
  "/src/a.less",
  "/src/b.less?inline",
  "/src/m.module.css",
  "/@id/__x00__virtual:theme.css",
  "/src/u.less?url",
];

let failed = false;
let srv;
try {
  // oj build, then Vite's.
  reset();
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const ojBuild = sorted(records());
  assert.equal(ojBuild.length, 36, ojBuild.join("\n"));
  const clientCss = (dir) =>
    fs
      .readdirSync(path.join(app, dir, "client", "assets"))
      .filter((f) => f.endsWith(".css"))
      .map((f) => fs.readFileSync(path.join(app, dir, "client", "assets", f), "utf8"))
      .join("\n");
  assert.ok(clientCss("dist").includes(".from-plugin{color:#639}"), clientCss("dist"));
  const urlSheet = (dir) => {
    const assets = path.join(app, dir, "client", "assets");
    const f = fs.readdirSync(assets).find((f) => /^u-.*\.css$/.test(f));
    return f && [f, fs.readFileSync(path.join(assets, f), "utf8")];
  };
  const ojUrlSheet = urlSheet("dist");
  assert.ok(ojUrlSheet && ojUrlSheet[1].includes(".u{color:green}"), `the ?url sheet compiled: ${ojUrlSheet}`);
  const manifest = fs.readFileSync(path.join(app, ".oj-cache", "v1", "start", "manifest.ts"), "utf8");
  assert.ok(!manifest.includes(ojUrlSheet[0]), `the page does not link the ?url sheet:\n${manifest}`);
  console.log(
    "[oj build] 36 slot records, each sheet once per slot and environment; the plugin's and the ?url sheets ship",
  );

  // oj dev: the SSR render and the client bundle (with its linked sheets).
  reset();
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const html = await (await fetch(`http://localhost:${PORT}/`)).text();
  // The page's client bundle, as a browser loading it would wait for.
  for (let i = 0; i < 60 && !records().some((l) => l.startsWith("client post")); i++) await sleep(250);
  const links = [...html.matchAll(/<link rel="stylesheet" href="([^"]+)"/g)].map((m) => m[1].replace(/&amp;/g, "&"));
  const fetchLinks = async () => {
    let css = "";
    for (const href of links) css += await (await fetch(`http://localhost:${PORT}${href}`)).text();
    return css;
  };
  const linked = await fetchLinks();
  const ojDev = sorted(records());
  assert.equal(ojDev.length, 30, ojDev.join("\n"));
  assert.ok(linked.includes(".from-plugin"), `the page links the plugin's sheet:\n${html}`);
  console.log("[oj dev] 30 slot records, each sheet once per slot and environment; the page links the plugin's sheet");

  // An edit reaches the linked sheet: the next bundle compiles it again.
  write("src/a.less", "@c: green;\n.a { color: @c; }\n");
  let edited = "";
  for (let i = 0; i < 80 && !/\.a\s*\{\s*color:\s*(green|#008000)/.test(edited); i++) {
    await sleep(250);
    edited = await fetchLinks();
  }
  assert.match(edited, /\.a\s*\{\s*color:\s*(green|#008000)/, `the edit reaches the linked sheet:\n${edited}`);
  write("src/a.less", "@c: red;\n.a { color: @c; }\n");
  console.log("[oj dev] an edit reaches the linked sheet");
  srv.kill("SIGKILL");
  srv = undefined;
  await sleep(300);

  // A warm start, the client bundle from cache (or rebuilt): the plugin's
  // sheet is linked and served all the same.
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const warmHtml = await (await fetch(`http://localhost:${PORT}/`)).text();
  let warm = "";
  for (const m of warmHtml.matchAll(/<link rel="stylesheet" href="([^"]+)"/g)) {
    const res = await fetch(`http://localhost:${PORT}${m[1].replace(/&amp;/g, "&")}`);
    assert.equal(res.status, 200, `[warm] ${m[1]}`);
    warm += await res.text();
  }
  assert.ok(warm.includes(".from-plugin"), `[warm] the page links the plugin's sheet:\n${warmHtml}`);
  console.log("[oj dev, warm] the plugin's sheet is linked and served");
  srv.kill("SIGKILL");
  srv = undefined;
  await sleep(300);

  const viteDir = path.join(fixture, "node_modules", "vite");
  reset();
  execSync(`node ${path.join(viteDir, "bin", "vite.js")} build --logLevel error`, { cwd: app, stdio: "inherit" });
  assert.deepEqual(ojBuild, sorted(records()), "each environment's slots, as in Vite's build");
  fs.renameSync(path.join(app, "dist"), path.join(app, "dist-vite"));
  assert.ok(clientCss("dist-vite").includes(".from-plugin{color:#639}"), "Vite ships it too");
  assert.deepEqual(ojUrlSheet, urlSheet("dist-vite"), "the ?url sheet Vite writes, name and bytes");
  console.log("[vite build] identical records");

  reset();
  const { createServer } = await import(pathToFileURL(path.join(viteDir, "dist", "node", "index.js")).href);
  const vite = await createServer({ root: app, logLevel: "error", server: { port: VITE_PORT } });
  await vite.listen();
  try {
    await (await fetch(`http://localhost:${VITE_PORT}/`)).text();
    // The modules a browser imports for the page.
    for (const u of CLIENT_MODULES) await (await fetch(`http://localhost:${VITE_PORT}${u}`)).text();
  } finally {
    await vite.close();
  }
  assert.deepEqual(ojDev, sorted(records()), "each environment's slots, as in Vite's dev server");
  console.log("[vite dev] identical records");
  console.log("START-CSS-PLUGIN-SLOTS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-CSS-PLUGIN-SLOTS E2E FAILED:", err.message);
} finally {
  srv?.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(path.dirname(seenFile), { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
