// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start, a server function behind a middleware that imports
// `@tanstack/react-start/server`. That import reaches Start's handler, which
// imports `#tanstack-start-server-fn-resolver`; a resolver that imported every
// `*.functions.ts` statically ran the function module inside that import,
// while its middleware module was still mid-evaluation: `Cannot access
// 'authed' before initialization` and every SSR request 500. Start's own
// resolver loads each module on the first call (`importer: () => import()`).
// The SSR entry imports the middleware first so the order is fixed (a real
// app hit it at random, by which module the server graph reached first). The
// page must render and the browser's call must reach the handler, in dev and
// in a build.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 3110);

const installed = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start server fn cycle: fixture deps not installed");
  process.exit(0);
}
let pw = null;
try {
  pw = await import("playwright");
} catch {}
if (!pw) {
  console.log("SKIP start server fn cycle: playwright not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-server-fn-cycle-"));
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/[\\/](node_modules|\.oj-cache)$/.test(src),
});
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");

const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write(
  "src/server/auth.ts",
  [
    'import { createMiddleware } from "@tanstack/react-start";',
    'import { getRequest } from "@tanstack/react-start/server";',
    "",
    'export const authed = createMiddleware({ type: "function" }).server(({ next }) =>',
    '  next({ context: { method: getRequest()?.method ?? "none" } }),',
    ");",
    "",
  ].join("\n"),
);
write(
  "src/server/guarded.functions.ts",
  [
    'import { createServerFn } from "@tanstack/react-start";',
    'import { authed } from "./auth";',
    "",
    'export const guarded = createServerFn({ method: "POST" })',
    "  .middleware([authed])",
    "  .handler(async ({ context }) => `guarded-${context.method}`);",
    "",
  ].join("\n"),
);
const entryPath = path.join(app, "src/ssr-entry.ts");
fs.writeFileSync(entryPath, 'import "./server/auth";\n' + fs.readFileSync(entryPath, "utf8"));
const aboutPath = path.join(app, "src/routes/about.tsx");
let about = fs.readFileSync(aboutPath, "utf8");
about = about.replace(
  'import { rootRoute } from "./__root";',
  'import { rootRoute } from "./__root";\nimport { useEffect, useState } from "react";\nimport { guarded } from "../server/guarded.functions";\n' +
    "function Guarded() {\n" +
    '  const [out, setOut] = useState("pending");\n' +
    "  useEffect(() => {\n" +
    '    guarded().then((r) => setOut(r), (e) => setOut("error " + String(e)));\n' +
    "  }, []);\n" +
    '  return <p id="guarded">{out}</p>;\n' +
    "}",
);
about = about.replace(
  '<h1 className="fixture-heading">about-page-marker</h1>',
  '<h1 className="fixture-heading">about-page-marker</h1>\n      <Guarded />',
);
fs.writeFileSync(aboutPath, about);

// Run `cmd` detached, wait for the port, run `check`, then group-kill and
// await the exit so no server outlives the step. `check` reads stdout and
// stderr together: either may carry an SSR error.
async function served(cmd, args, opts, check) {
  const srv = spawn(cmd, args, { stdio: ["ignore", "pipe", "pipe"], detached: true, ...opts });
  let stderr = "";
  srv.stdout.on("data", (d) => (stderr += d));
  srv.stderr.on("data", (d) => (stderr += d));
  const exited = new Promise((r) => srv.once("exit", r));
  try {
    const up = await waitUp(`http://localhost:${PORT}/`, { until: () => true, proc: srv }).then(
      () => true,
      () => false,
    );
    must(up, `server on :${PORT} did not start\n${stderr.slice(-2000)}`);
    await check(() => stderr);
  } finally {
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
    await exited;
  }
}

async function rendersAndCalls(label, stderr) {
  const res = await fetch(`http://localhost:${PORT}/about`);
  const html = await res.text();
  must(
    res.status === 200 && html.includes("about-page-marker"),
    `${label}: /about answered ${res.status}, want the rendered page\n${html.slice(0, 500)}\n${stderr().slice(-2000)}`,
  );
  const browser = await pw.chromium.launch();
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    await page.goto(`http://localhost:${PORT}/about`);
    const want = "guarded-POST";
    const got = await page
      .waitForFunction((w) => document.querySelector("#guarded")?.textContent === w, want, { timeout: 20000 })
      .then(() => want)
      .catch(async () =>
        page
          .locator("#guarded")
          .textContent()
          .catch(() => null),
      );
    must(
      got === want,
      `${label}: the server fn answered ${JSON.stringify(got)}, want ${want}; page errors: ${errors.join(" | ")}\n${stderr().slice(-2000)}`,
    );
    must(!/before initialization/.test(stderr()), `${label}: a module ran before its import:\n${stderr()}`);
    // An id naming an Object.prototype member is unknown, not a TypeError.
    await fetch(`http://localhost:${PORT}/_serverFn/constructor`, {
      method: "POST",
      headers: {
        origin: `http://localhost:${PORT}`,
        "x-tsr-serverFn": "true",
        "content-type": "application/json",
      },
      body: "{}",
    });
    const deadline = Date.now() + 5000;
    while (!/Server function info not found for constructor/.test(stderr()) && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
    }
    must(
      /Server function info not found for constructor/.test(stderr()),
      `${label}: /_serverFn/constructor did not fail as an unknown id:\n${stderr().slice(-2000)}`,
    );
    console.log(`${label}: a server fn whose middleware imports Start's server renders and runs`);
  } finally {
    await browser.close();
  }
}

try {
  await served(oj, ["dev", app, "--port", String(PORT)], {}, async (stderr) => rendersAndCalls("start-dev", stderr));

  const out = path.join(app, "dist-e2e");
  execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)} --out ${JSON.stringify(out)}`, {
    stdio: ["ignore", "ignore", "inherit"],
  });
  await served(
    "node",
    [path.join(out, "server.mjs")],
    { cwd: out, env: { ...process.env, PORT: String(PORT) } },
    async (stderr) => rendersAndCalls("start build", stderr),
  );
  console.log("\nSTART SERVER FN CYCLE PASSED");
} catch (e) {
  console.error(`\nSTART SERVER FN CYCLE FAILED: ${e.message}`);
  process.exitCode = 1;
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
