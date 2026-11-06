// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// UnoCSS under TanStack Start: its `virtual:uno.css` is a stylesheet a plugin
// serves with no file behind it. In the build, its placeholder module stays
// in the client chunk and UnoCSS hands the generated CSS to vite:css-post,
// which takes the placeholder sheet's place; the client stylesheet must be
// byte-identical to what Vite builds from the same app. In dev, the page links
// the sheet and its plugin load serves the page's utilities.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(repo, "e2e", "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6380;
const UNOCSS = "unocss@66.6.0";

if (!fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start unocss: fixture deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// UnoCSS installs on its own; the app links its packages beside the fixture's.
const uno = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-unocss-pkg-"));
fs.writeFileSync(path.join(uno, "package.json"), JSON.stringify({ name: "uno-pkg", private: true }));
try {
  execSync(`npm install ${UNOCSS} --no-audit --no-fund --loglevel=error`, { cwd: uno, stdio: "ignore" });
} catch {
  console.log("SKIP start unocss: could not install unocss (offline?)");
  fs.rmSync(uno, { recursive: true, force: true });
  process.exit(0);
}

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-unocss-"));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
fs.mkdirSync(path.join(app, "node_modules"));
for (const dir of [path.join(fixture, "node_modules"), path.join(uno, "node_modules")]) {
  for (const name of fs.readdirSync(dir)) {
    const link = path.join(app, "node_modules", name);
    if (!fs.existsSync(link)) fs.symlinkSync(path.join(dir, name), link);
  }
}
write(
  "package.json",
  JSON.stringify({
    name: "start-unocss",
    private: true,
    type: "module",
    dependencies: { "@tanstack/react-router": "*", "@tanstack/react-start": "*", react: "*", "react-dom": "*" },
    devDependencies: { vite: "*", "@vitejs/plugin-react": "*", unocss: "*" },
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
  head: () => ({ meta: [{ title: "uno" }] }),
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
import "virtual:uno.css";
import { Badge } from "../Badge";

export const Route = createFileRoute("/")({
  component: () => <p className="p-4 text-red-500 m-2">uno <Badge /></p>,
});
`,
);
// A module the bundle transforms after UnoCSS's sheet: its classes still
// reach the linked sheet.
write("src/Badge.tsx", `export function Badge() {\n  return <span className="m-7">badge</span>;\n}\n`);
write(
  "vite.config.ts",
  `import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import UnoCSS from "unocss/vite";
import { presetWind3 } from "unocss";
import { defineConfig } from "vite";

export default defineConfig({ plugins: [UnoCSS({ presets: [presetWind3()] }), tanstackStart(), react()] });
`,
);

const clientCss = (dir) =>
  fs
    .readdirSync(path.join(app, dir, "client", "assets"))
    .filter((f) => f.endsWith(".css"))
    .map((f) => fs.readFileSync(path.join(app, dir, "client", "assets", f), "utf8"))
    .sort();

let failed = false;
let srv;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const built = clientCss("dist");
  const css = built.join("\n");
  for (const rule of [".m-2{margin:.5rem}", ".p-4{padding:1rem}", ".text-red-500{"]) {
    assert.ok(css.includes(rule), `${rule} in the client stylesheet:\n${css}`);
  }
  assert.ok(!css.includes("--unocss--"), `UnoCSS's placeholder was replaced:\n${css}`);
  const manifest = fs.readFileSync(path.join(app, ".oj-cache", "v1", "start", "manifest.ts"), "utf8");
  for (const f of fs.readdirSync(path.join(app, "dist", "client", "assets")).filter((f) => f.endsWith(".css"))) {
    assert.ok(manifest.includes(`/assets/${f}`), `the page links ${f}:\n${manifest}`);
  }
  console.log("[oj build] UnoCSS's generated sheet ships and is linked");

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  // A cold start, then a warm one (the client bundle's cache): UnoCSS
  // collects the classes from the modules' transforms both times.
  for (const run of ["cold", "warm"]) {
    srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
    await waitUp(`http://localhost:${PORT}/`);
    const html = await (await fetch(`http://localhost:${PORT}/`)).text();
    let linked = "";
    for (const m of html.matchAll(/<link rel="stylesheet" href="([^"]+)"/g)) {
      linked += await (await fetch(`http://localhost:${PORT}${m[1].replace(/&amp;/g, "&")}`)).text();
    }
    for (const rule of [/\.p-4\s*\{\s*padding:\s*1rem/, /\.m-7\s*\{\s*margin:\s*1\.75rem/]) {
      assert.match(linked, rule, `[${run}] the page links UnoCSS's sheet:\n${html}\n${linked.slice(0, 600)}`);
    }
    console.log(`[oj dev, ${run}] the page links UnoCSS's sheet with its utilities, a later module's too`);
    srv.kill("SIGKILL");
    srv = undefined;
    await sleep(300);
  }

  execSync(`node ${path.join(fixture, "node_modules", "vite", "bin", "vite.js")} build --logLevel error`, {
    cwd: app,
    stdio: "inherit",
  });
  assert.deepEqual(built, clientCss("dist"), "the client stylesheet Vite writes");
  console.log("[vite] byte-identical client stylesheet");
  console.log("START-CSS-PLUGIN-UNOCSS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-CSS-PLUGIN-UNOCSS E2E FAILED:", err.message);
} finally {
  srv?.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(uno, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
