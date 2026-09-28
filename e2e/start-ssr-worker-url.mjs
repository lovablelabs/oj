// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start dev SSR, two Vite contracts a real app hit together:
//
// 1. Worker import queries in the SSR loader. `import url from "./w.ts?worker&url"`
//    is the worker's URL string and `?worker` a constructor wrapper, in every
//    environment (Vite's worker plugin load). The SSR host only knew an exact
//    `?raw`/`?url`/`?inline` suffix, so it loaded the worker as a plain module
//    and the render died with "does not provide an export named 'default'".
// 2. Post plugins see JS. Vite strips TS/JSX (vite:oxc) before normal and post
//    plugins, so an enforce:"post" plugin can `this.parse` every module; oj
//    used to hand it raw TypeScript.
//
// The worker is imported through a computed dynamic import that only runs on
// the server, so the SSR loader alone resolves it.

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
write("src/lib/wk/worker.ts", "const n: number = 1;\nself.onmessage = () => self.postMessage(n);\n");
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
  [
    'import { rootRoute } from "./__root";',
    'const wkSpec = ["..", "lib", "wk", "index.ts"].join("/");',
    "const wk: { wurl?: string; wtype?: string } | null =",
    '  typeof window === "undefined" ? await import(/* @vite-ignore */ wkSpec) : null;',
  ].join("\n"),
);
about = about.replace(
  '<h1 className="fixture-heading">about-page-marker</h1>',
  '<h1 className="fixture-heading">about-page-marker</h1>\n      <p id="wk">{`${wk?.wurl ?? "client"}|${wk?.wtype ?? "client"}`}</p>',
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

const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "ignore", "pipe"], detached: true });
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

  const res = await fetch(`http://localhost:${PORT}/about`);
  const html = await res.text();
  must(res.status === 200, `/about returned ${res.status}\n${stderr.slice(-2000)}`);
  const shown = html.match(/<p id="wk">([^<]*)<\/p>/)?.[1];
  must(shown === "/src/lib/wk/worker.ts|function", `SSR worker imports rendered ${JSON.stringify(shown)}`);
  const worker = await fetch(`http://localhost:${PORT}/src/lib/wk/worker.ts`);
  must(worker.status === 200, `the worker URL the SSR render emitted returned ${worker.status}`);
  must(!(await worker.text()).includes(": number"), "the worker URL serves uncompiled TypeScript");
  console.log("start-dev ssr: ?worker&url is the worker's dev URL, ?worker a constructor, the URL serves the worker");

  const failures = fs.existsSync(parseLog) ? fs.readFileSync(parseLog, "utf8").trim() : "";
  must(failures === "", `a post plugin could not this.parse:\n${failures}`);
  console.log("start-dev ssr: an enforce:post plugin parses every TS/TSX module (TS/JSX stripped at vite:oxc's slot)");
  console.log("\nSTART SSR WORKER URL PASSED");
} catch (e) {
  console.error("\nSTART SSR WORKER URL FAILED:", e.message);
  process.exitCode = 1;
} finally {
  try {
    process.kill(-srv.pid, "SIGKILL");
  } catch {}
  await exited;
  fs.rmSync(app, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
