// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start dev: the config's `define` reaches the CLIENT bundle (not only
// the SSR loader), and `oj dev --mode <mode>` selects `.env.<mode>` and sets
// `import.meta.env.MODE` on both sides, like `vite dev --mode`. A define that
// only the server applied SSRs fine and throws a ReferenceError on hydration,
// which the SSR-HTML-only checks in start.mjs never see.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 3099);

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start define/mode: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
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
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const get = async (route) => {
  const res = await fetch(`http://localhost:${PORT}${route}`);
  return { status: res.status, body: await res.text() };
};

// Every JS chunk of the dev client bundle, concatenated (routes may be split
// out of the entry chunk, so the entry alone is not enough).
async function clientBundle() {
  const cache = path.join(app, ".oj-cache");
  const versioned = fs.readdirSync(cache).find((d) => /^v\d+$/.test(d));
  const index = JSON.parse(fs.readFileSync(path.join(cache, versioned, "start", "client-chunks.json"), "utf8"));
  let code = "";
  for (const f of index.files) {
    if (!f.name.endsWith(".js")) continue;
    const res = await get(`/@oj-start/${f.name}`);
    if (res.status !== 200) throw new Error(`client chunk ${f.name} returned ${res.status}`);
    code += res.body;
  }
  return code;
}

async function served(args, check) {
  rm(path.join(app, ".oj-cache"));
  const srv = spawn(oj, ["dev", app, "--port", String(PORT), ...args], { stdio: "ignore" });
  try {
    await waitUp(`http://localhost:${PORT}/`);
    await check();
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

const must = (cond, msg) => { if (!cond) throw new Error(msg); };

try {
  await served([], async () => {
    const html = (await get("/")).body;
    must(html.includes("fixture-define-marker"), "default: SSR did not apply the config define");
    must(html.includes("jsenv:development:true:default-flavor"), `default: SSR MODE/.env.development wrong:\n${html.match(/jsenv:[^<]*/)?.[0]}`);
    const js = await clientBundle();
    must(js.includes("fixture-define-marker"), "default: client bundle did not apply the config define (hydration would throw)");
    must(!/\b__FIXTURE_DEFINE__\b/.test(js), "default: client bundle still references the bare define identifier");
    must(/"MODE":\s*"development"/.test(js), "default: client import.meta.env.MODE is not development");
    must(js.includes("default-flavor"), "default: client did not get .env.development VITE_ var");
    must(!js.includes("staging-flavor"), "default: .env.staging leaked into the default mode");
    // envPrefix: a FIXTURE_ var reaches both sides, an unprefixed one neither.
    must(html.includes(":true:custom-prefix-edition"), `default: SSR import.meta.env missed the custom envPrefix var (DEV should be true):\n${html.match(/jsenv:[^<]*/)?.[0]}`);
    must(js.includes("custom-prefix-edition"), "default: client import.meta.env missed the custom envPrefix var");
    must(!html.includes("must-not-leak") && !js.includes("must-not-leak"), "default: an unprefixed .env var leaked into import.meta.env");
    // environments.{ssr,client}.define: each bundle gets its own value.
    must(html.includes(">server-side<"), "default: SSR did not apply environments.ssr.define");
    must(js.includes('"client-side"') && !js.includes('"server-side"'), "default: client bundle did not apply environments.client.define");
    must(!/\b__FIXTURE_SIDE__\b/.test(js), "default: client bundle still references the bare environment define");
    console.log("start-dev: config define reaches the client bundle; envPrefix + environment defines; default mode ok");
  });

  await served(["--mode", "staging"], async () => {
    const html = (await get("/")).body;
    must(html.includes("jsenv:staging:true:staging-flavor"), `--mode staging: SSR MODE/.env.staging wrong:\n${html.match(/jsenv:[^<]*/)?.[0]}`);
    must(html.includes("fixture-define-marker"), "--mode staging: SSR lost the config define");
    const js = await clientBundle();
    must(/"MODE":\s*"staging"/.test(js), "--mode staging: client import.meta.env.MODE is not staging");
    must(/"DEV":\s*true/.test(js), "--mode staging: a non-production dev mode is still DEV");
    must(js.includes("staging-flavor"), "--mode staging: client did not get .env.staging VITE_ var");
    must(!js.includes("default-flavor"), "--mode staging: .env.development leaked into staging");
    must(js.includes("fixture-define-marker"), "--mode staging: client lost the config define");
    console.log("start-dev: --mode staging selects .env.staging + MODE on server and client");
  });
  console.log("\nSTART DEFINE/MODE PASSED");
} catch (e) {
  console.error("\nSTART DEFINE/MODE FAILED:", e.message);
  process.exit(1);
}
