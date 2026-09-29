// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies new URL("./asset", import.meta.url) is rewritten to a ?url asset
// import (Vite's asset-import-meta-url) so the asset resolves instead of 404ing
// — checked in the dev server (the primary preview path).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5336;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-newurl-"));

let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"newurl","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "logo.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>\n");
  fs.writeFileSync(path.join(app, "src", "main.js"), 'export const u = new URL("./logo.svg", import.meta.url).href;\n');

  child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app, stdio: "ignore" });
  const up = async () => {
    return waitUp(`http://localhost:${PORT}/`).then(() => true, () => false);
  };
  if (!(await up())) throw new Error("dev server did not start");

  const mod = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  if (/new URL\(\s*["']\.\/logo\.svg["']/.test(mod)) {
    throw new Error(`new URL literal left unrewritten (would 404):\n${mod}`);
  }
  if (!/logo\.svg\?url/.test(mod)) {
    throw new Error(`expected a hoisted ?url asset import:\n${mod}`);
  }
  console.log("dev: new URL -> ?url asset import (no raw literal)");

  // The hoisted ?url import must resolve to the served asset path.
  const m = mod.match(/from\s*["']([^"']*logo\.svg\?url[^"']*)["']/);
  if (m) {
    const urlMod = await (await fetch(`http://localhost:${PORT}${m[1].startsWith("/") ? m[1] : "/src/" + m[1]}`)).text();
    if (!urlMod.includes("logo")) throw new Error(`?url module did not resolve the asset:\n${urlMod}`);
    console.log("dev: ?url module resolves the asset path");
  }
  child.kill("SIGKILL");
  child = null;

  // build: the asset must resolve too (inlined as data: for a tiny file, or an
  // /assets path for a big one) — never a raw relative literal that 404s.
  execSync(`${OJ} build ${app} --out ${path.join(app, "dist")}`, { stdio: ["ignore", "ignore", "inherit"] });
  const built = fs
    .readdirSync(path.join(app, "dist", "assets"))
    .filter((f) => f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(app, "dist", "assets", f), "utf8"))
    .join("");
  if (/new URL\(\s*[`"']\.\/logo\.svg/.test(built)) {
    throw new Error(`build left a raw new URL literal (would 404):\n${built}`);
  }
  if (!/data:image\/svg/.test(built) && !/\/assets\/logo/.test(built)) {
    throw new Error(`build did not resolve the new URL asset:\n${built}`);
  }
  console.log("build: new URL asset resolved (inlined or emitted)");
  console.log("\nnew URL(import.meta.url) VERIFIED (dev + build)");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
