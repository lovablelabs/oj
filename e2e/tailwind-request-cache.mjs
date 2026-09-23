// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the memoized Tailwind entry stylesheet: repeated requests serve
// byte-identical CSS (?direct and the ?import wrapper), and an app-source edit
// that uses a new utility class invalidates the memo so the next request
// carries the new class. The fixture lives under playground/ so tailwindcss
// resolves from its node_modules.

import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const OJ = path.join(repo, "target", "debug", "oj");
const PORT = 5341;
const app = path.join(repo, "playground", ".e2e-tailwind-request-cache");
const APP = path.join(app, "src", "App.tsx");
const appSrc = (cls) =>
  `export function App() {\n  return <h1 className="${cls}">hi</h1>;\n}\n`;

let failed = false;
let child;
try {
  fs.rmSync(app, { recursive: true, force: true });
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"tw-cache","private":true}');
  fs.writeFileSync(path.join(app, "src", "index.css"), '@import "tailwindcss";\n');
  fs.writeFileSync(
    path.join(app, "src", "main.tsx"),
    'import "./index.css";\nimport { App } from "./App";\nconsole.log(App);\n',
  );
  fs.writeFileSync(APP, appSrc("underline"));
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>',
  );

  child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app, stdio: "ignore" });
  const get = async (q) => {
    const res = await fetch(`http://localhost:${PORT}/src/index.css${q}`);
    if (!res.ok) throw new Error(`GET index.css${q} -> ${res.status}`);
    return res.text();
  };
  let up = false;
  for (let i = 0; i < 300; i++) {
    try { if ((await fetch(`http://localhost:${PORT}/`)).ok) { up = true; break; } } catch {}
    await new Promise((r) => setTimeout(r, 100));
  }
  if (!up) throw new Error("dev server did not start");

  const direct1 = await get("?direct");
  const direct2 = await get("?direct");
  if (!direct1.includes("underline")) throw new Error("tailwind did not emit .underline");
  if (direct1 !== direct2) throw new Error("repeated ?direct requests differ");
  const wrap1 = await get("?import");
  const wrap2 = await get("?import");
  if (!wrap1.includes("underline")) throw new Error("?import wrapper missing the compiled css");
  if (wrap1 !== wrap2) throw new Error("repeated ?import requests differ");
  console.log("warm re-requests:   identical (?direct and ?import)");

  fs.writeFileSync(APP, appSrc("underline italic"));
  let updated = "";
  for (let i = 0; i < 100; i++) {
    updated = await get("?direct");
    if (updated.includes("italic")) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  if (!updated.includes("italic")) throw new Error("new class missing after app-source edit (stale memo)");
  if (!updated.includes("underline")) throw new Error("old class lost after edit");
  const again = await get("?direct");
  if (again !== updated) throw new Error("post-edit re-request differs");
  console.log("app-source edit:    .italic present, cache re-warmed");
  console.log("\nTAILWIND REQUEST CACHE VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
