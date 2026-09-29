// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

// A plain `.svg` import (no `?react`) must go through a configured plugin
// `transform` that componentizes svgs (vite-plugin-svgr with an `include` list,
// `exportType: "default"`), exactly as Vite does. oj must not stamp `?url` onto
// such an import (which would force URL-asset treatment); an svg the plugin does
// not transform still falls back to a URL asset. This mirrors the real app where
// `import Svg from "./x.svg"` yields a React component.
const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-svgr-plugin-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "svgr-plugin-app", version: "1.0.0" }));
try {
  execSync("npm install react react-dom --no-audit --no-fund --loglevel=error", { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP svgr-plugin: could not install react (offline?)");
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}

fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(
  path.join(app, "src", "icon.svg"),
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4 4h16"/></svg>`,
);
fs.writeFileSync(
  path.join(app, "src", "photo.svg"),
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 8 8"><rect width="8" height="8"/></svg>`,
);
// A minimal svgr-like plugin: componentize svgs under src/, leave others alone.
// `icon.svg` becomes a component; `photo.svg` is excluded, so it must stay a URL.
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
     name: "mini-svgr",
     enforce: "pre",
     transform(code, id) {
       const clean = id.split("?")[0];
       if (clean.endsWith("icon.svg")) {
         return { code: "export default function Icon(props){ return <svg data-svgr=\\"1\\" {...props}><path d=\\"M4 4h16\\"/></svg>; }", map: null };
       }
       return null;
     },
   }];\n`,
);
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";\n` +
    `import Icon from "./icon.svg";\n` +
    `import photo from "./photo.svg";\n` +
    `const root = document.createElement("div"); root.id = "root"; document.body.appendChild(root);\n` +
    `const probe = document.createElement("div"); probe.id = "photo"; probe.textContent = String(photo); document.body.appendChild(probe);\n` +
    `createRoot(root).render(<Icon className="added" data-testid="icon" />);\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.tsx"></script></body></html>`,
);

async function checkSource(port) {
  // Dev only: the importer must NOT carry a `?url` marker on the componentized
  // svg, and the svg module itself must serve component JS, not raw markup.
  const mainSrc = await (await fetch(`http://localhost:${port}/src/main.tsx`)).text();
  const iconImport = mainSrc.match(/import Icon from "([^"]*)"/);
  if (!iconImport) throw new Error("icon import not found in served main.tsx");
  if (/\?url/.test(iconImport[1])) throw new Error(`componentized svg was marked ?url: ${iconImport[1]}`);
  const iconMod = await (await fetch(`http://localhost:${port}${iconImport[1]}`)).text();
  if (/^\s*<svg|^\s*<\?xml/.test(iconMod)) throw new Error("svg served as raw markup, not a component");
  if (!/data-svgr/.test(iconMod)) throw new Error("plugin transform did not run on the svg");
}

async function checkRender(port) {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    const el = await page.waitForSelector("#root svg[data-svgr]", { timeout: 10000 });
    const outer = await page.evaluate((e) => e.outerHTML, el);
    if (!/class="added"/.test(outer)) throw new Error(`props not spread onto component svg: ${outer.slice(0, 80)}`);
    // The excluded svg is a URL asset: its default export is a served/emitted path
    // (dev) or an inlined data: URL (prod), never a rendered component.
    const photo = await page.evaluate(() => document.getElementById("photo").textContent);
    if (!/(\.svg|data:image\/svg)/.test(photo) || /\bfunction\b|\[object/.test(photo)) {
      throw new Error(`excluded svg did not fall back to a URL asset: ${photo}`);
    }
    if (errors.length) throw new Error(`page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
}

async function mode(label, args, port, build) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  if (build) {
    fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
    execSync(`${oj} build ${app}`, { stdio: "ignore" });
  }
  const srv = spawn(oj, args, { stdio: "ignore" });
  try {
    await waitUp(`http://localhost:${port}/`);
    if (!build) await checkSource(port);
    await checkRender(port);
    console.log(`[${label}] plain-svg svgr OK`);
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  await mode("dev", ["dev", app, "--port", "5396"], 5396, false);
  await mode("prod", ["preview", app, "--port", "5398"], 5398, true);
  console.log("SVGR-PLUGIN E2E PASSED");
} catch (err) {
  failed = true;
  console.error("SVGR-PLUGIN E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
