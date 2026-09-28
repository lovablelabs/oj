// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start, two Vite contracts a real app hit together:
//
// 1. Worker import queries. `import url from "./w.ts?worker&url"` is the
//    worker's URL string and `?worker` a constructor wrapper, in every
//    environment (Vite's worker plugin load). The SSR host and the client
//    bundle only knew an exact `?raw`/`?url`/`?inline` suffix: SSR loaded the
//    worker as a plain module ("does not provide an export named 'default'")
//    and the client bundle failed (UNLOADABLE_DEPENDENCY). Dev serves the
//    worker from the dev pipeline; a build bundles it on its own and emits it.
// 2. Post plugins see JS. Vite strips TS/JSX (vite:oxc) before normal and post
//    plugins, so an enforce:"post" plugin can `this.parse` every module; oj
//    used to hand it raw TypeScript.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 3107);

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start ssr worker url: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-worker-"));
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/[\\/](node_modules|\.oj-cache)$/.test(src),
});
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");

const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write("src/lib/wk/helper.ts", "export const n: number = 41;\n");
write(
  "src/lib/wk/worker.ts",
  'import { n } from "./helper";\nconst m: number = n + 1;\nself.onmessage = () => self.postMessage(m);\n',
);
write(
  "src/lib/wk/index.ts",
  [
    'import workerUrl from "./worker.ts?worker&url";',
    'import WorkerCtor from "./worker.ts?worker";',
    "export const wurl: string = workerUrl;",
    "export const wtype: string = typeof WorkerCtor;",
    "",
  ].join("\n"),
);
const aboutPath = path.join(app, "src/routes/about.tsx");
let about = fs.readFileSync(aboutPath, "utf8");
about = about.replace(
  'import { rootRoute } from "./__root";',
  'import { rootRoute } from "./__root";\nimport { wurl, wtype } from "../lib/wk";',
);
about = about.replace(
  '<h1 className="fixture-heading">about-page-marker</h1>',
  '<h1 className="fixture-heading">about-page-marker</h1>\n      <p id="wk">{`${wurl}|${wtype}`}</p>',
);
fs.writeFileSync(aboutPath, about);

// An AST-reading post plugin, the shape that failed: it logs every module it
// cannot this.parse.
const parseLog = path.join(app, "parse-failures.log");
const configPath = path.join(app, "vite.config.ts");
let config = fs.readFileSync(configPath, "utf8");
config = config.replace(
  "plugins: [",
  `plugins: [{
    name: "e2e-post-parse",
    enforce: "post",
    applyToEnvironment: (environment) => environment.name === "ssr",
    transform(code, id) {
      if (id.startsWith("\\0") || id.includes("node_modules") || !/\\.(tsx?|jsx)$/.test(id.split("?")[0])) return null;
      try { this.parse(code); } catch (e) { require("node:fs").appendFileSync(${JSON.stringify(parseLog)}, id + "\\n"); }
      return null;
    },
  }, `,
);
config = `import { createRequire } from "node:module";\nconst require = createRequire(import.meta.url);\n${config}`;
fs.writeFileSync(configPath, config);

// Run `cmd` detached, wait for the port, run `check`, then group-kill and
// await the exit so no server outlives the step.
async function served(cmd, args, opts, check) {
  const srv = spawn(cmd, args, { stdio: ["ignore", "ignore", "pipe"], detached: true, ...opts });
  let stderr = "";
  srv.stderr.on("data", (d) => (stderr += d));
  const exited = new Promise((r) => srv.once("exit", r));
  try {
    let up = false;
    for (let i = 0; i < 240 && !up; i++) {
      try {
        up = (await fetch(`http://localhost:${PORT}/`)).status > 0;
      } catch {
        await sleep(500);
      }
    }
    must(up, `server on :${PORT} did not start\n${stderr.slice(-2000)}`);
    await check(() => stderr);
  } finally {
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
    await exited;
  }
}

const rendered = async () => {
  const res = await fetch(`http://localhost:${PORT}/about`);
  const html = await res.text();
  return { status: res.status, shown: html.match(/<p id="wk">([^<]*)<\/p>/)?.[1] };
};

try {
  await served(oj, ["dev", app, "--port", String(PORT)], {}, async (stderr) => {

    const { status, shown } = await rendered();
    must(status === 200, `dev /about returned ${status}\n${stderr().slice(-2000)}`);
    must(shown === "/src/lib/wk/worker.ts|function", `dev SSR worker imports rendered ${JSON.stringify(shown)}`);
    const worker = await fetch(`http://localhost:${PORT}/src/lib/wk/worker.ts`);
    must(worker.status === 200, `the worker URL the SSR render emitted returned ${worker.status}`);
    must(!(await worker.text()).includes(": number"), "the worker URL serves uncompiled TypeScript");
    // The client bundle (which failed to load the query) carries the same URL,
    // so the rendered value hydrates unchanged.
    const cache = path.join(app, ".oj-cache");
    const versioned = fs.readdirSync(cache).find((d) => /^v\d+$/.test(d));
    const index = JSON.parse(fs.readFileSync(path.join(cache, versioned, "start", "client-chunks.json"), "utf8"));
    let js = "";
    for (const f of index.files) if (f.name.endsWith(".js")) js += await (await fetch(`http://localhost:${PORT}/@oj-start/${f.name}`)).text();
    must(js.includes('"/src/lib/wk/worker.ts"'), "the dev client bundle does not carry the worker URL");
    must(/new Worker\(/.test(js), "the dev client bundle has no ?worker constructor");
    console.log("start-dev: ?worker&url is the worker's dev URL, ?worker a constructor, on SSR and in the client bundle");

    const failures = fs.existsSync(parseLog) ? fs.readFileSync(parseLog, "utf8").trim() : "";
    must(failures === "", `a post plugin could not this.parse:\n${failures}`);
    console.log("start-dev ssr: an enforce:post plugin parses every TS/TSX module (TS/JSX stripped at vite:oxc's slot)");
  });

  // A build bundles the worker entry on its own and emits it once; the client
  // and the server render share the emitted URL.
  const out = path.join(app, "dist-e2e");
  execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)} --out ${JSON.stringify(out)}`, { stdio: ["ignore", "ignore", "inherit"] });
  const assets = path.join(out, "client", "assets");
  const workerFile = fs.readdirSync(assets).find((f) => /^worker-[\w-]+\.js$/.test(f));
  must(workerFile, `the build emitted no worker asset: ${fs.readdirSync(assets).join(", ")}`);
  const workerCode = fs.readFileSync(path.join(assets, workerFile), "utf8");
  // The helper's 41 is inlined (a minifier folds `41 + 1` to 42).
  must(!/\bimport\b/.test(workerCode) && /\b4[12]\b/.test(workerCode), `the emitted worker is not one bundled file with its helper inlined:\n${workerCode}`);
  must(!workerCode.includes(": number"), "the emitted worker is uncompiled TypeScript");
  await served("node", [path.join(out, "server.mjs")], { cwd: out, env: { ...process.env, PORT: String(PORT) } }, async (stderr) => {
    const { status, shown } = await rendered();
    must(status === 200, `built /about returned ${status}\n${stderr().slice(-2000)}`);
    must(shown === `/assets/${workerFile}|function`, `built SSR worker imports rendered ${JSON.stringify(shown)}`);
    const served = await fetch(`http://localhost:${PORT}/assets/${workerFile}`);
    must(served.status === 200, `the emitted worker URL returned ${served.status}`);
  });
  const clientJs = fs.readdirSync(assets).filter((f) => f.endsWith(".js") && f !== workerFile).map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("");
  must(clientJs.includes(`/assets/${workerFile}`), "the client build does not reference the emitted worker");
  console.log("start build: the worker is bundled once and emitted; client and server render the same URL");
  console.log("\nSTART SSR WORKER URL PASSED");
} catch (e) {
  console.error("\nSTART SSR WORKER URL FAILED:", e.message);
  process.exitCode = 1;
} finally {
  if (!process.env.OJ_E2E_KEEP) fs.rmSync(app, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  else console.log("kept", app);
}
