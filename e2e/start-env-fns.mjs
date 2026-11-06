// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start's environment functions compiled per environment, as its
// start-compiler does: `createIsomorphicFn().server(..).client(..)` keeps only
// this side's function and `createServerOnlyFn(f)` throws TanStack's error on
// the client. oj used a runtime `typeof document` stub, so both sides' bodies
// (and whatever they import) shipped to the browser. In dev and in a build:
// the page gets the client function and the server-only error, the SSR HTML
// the server function, and no JS the browser loads carries the server marker.

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
const PORT = Number(process.env.OJ_E2E_PORT || 3109);

const installed = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start env fns: fixture deps not installed");
  process.exit(0);
}
let pw = null;
try {
  pw = await import("playwright");
} catch {}
if (!pw) {
  console.log("SKIP start env fns: playwright not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-env-fns-"));
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/[\\/](node_modules|\.oj-cache)$/.test(src),
});
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");

const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
const MARKER = "SERVER_ONLY_MARKER";
write(
  "src/lib/env.ts",
  [
    'import { createIsomorphicFn, createServerOnlyFn } from "@tanstack/react-start";',
    "",
    `export const who = createIsomorphicFn().server(() => "${MARKER}_iso").client(() => "client-impl");`,
    `export const secret = createServerOnlyFn(() => "${MARKER}_only");`,
    "",
  ].join("\n"),
);
const aboutPath = path.join(app, "src/routes/about.tsx");
let about = fs.readFileSync(aboutPath, "utf8");
about = about.replace(
  'import { rootRoute } from "./__root";',
  'import { rootRoute } from "./__root";\nimport { useEffect, useState } from "react";\nimport { secret, who } from "../lib/env";\n' +
    "function Env() {\n" +
    '  const [out, setOut] = useState("pending");\n' +
    "  useEffect(() => {\n" +
    "    let thrown;\n" +
    "    try { secret(); } catch (e) { thrown = e.message; }\n" +
    '    setOut(who() + "|" + thrown);\n' +
    "  }, []);\n" +
    '  return (<><p id="ssr" suppressHydrationWarning>{who()}</p><p id="env">{out}</p></>);\n' +
    "}",
);
about = about.replace(
  '<h1 className="fixture-heading">about-page-marker</h1>',
  '<h1 className="fixture-heading">about-page-marker</h1>\n      <Env />',
);
fs.writeFileSync(aboutPath, about);

// Run `cmd` detached, wait for the port, run `check`, then group-kill and
// await the exit so no server outlives the step.
async function served(cmd, args, opts, check) {
  const srv = spawn(cmd, args, { stdio: ["ignore", "ignore", "pipe"], detached: true, ...opts });
  let stderr = "";
  srv.stderr.on("data", (d) => (stderr += d));
  const exited = new Promise((r) => srv.once("exit", r));
  try {
    const up = await waitUp(`http://localhost:${PORT}/`, { until: () => true }).then(
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

async function checkPage(label, stderr) {
  const html = await fetch(`http://localhost:${PORT}/about`).then((r) => r.text());
  must(html.includes(`${MARKER}_iso`), `${label}: the SSR HTML renders the server function:\n${html.slice(0, 2000)}`);
  const browser = await pw.chromium.launch();
  try {
    const page = await browser.newPage();
    const errors = [];
    const scripts = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    page.on("response", (r) => {
      if (/javascript/.test(r.headers()["content-type"] || "")) {
        scripts.push(
          r.text().then(
            (t) => [r.url(), t],
            () => [r.url(), ""],
          ),
        );
      }
    });
    await page.goto(`http://localhost:${PORT}/about`);
    const want = "client-impl|createServerOnlyFn() functions can only be called on the server!";
    const got = await page
      .waitForFunction((w) => document.querySelector("#env")?.textContent === w, want, { timeout: 20000 })
      .then(() => want)
      .catch(async () =>
        page
          .locator("#env")
          .textContent()
          .catch(() => null),
      );
    must(
      got === want,
      `${label}: the page got ${JSON.stringify(got)}, want ${JSON.stringify(want)}; page errors: ${errors.join(" | ")}\n${stderr().slice(-2000)}`,
    );
    const leaked = (await Promise.all(scripts)).filter(([, t]) => t.includes(MARKER)).map(([u]) => u);
    must(scripts.length > 0, `${label}: no scripts seen`);
    must(leaked.length === 0, `${label}: server-only code reached the browser in ${leaked.join(", ")}`);
    console.log(`${label}: the client gets its own function and the server-only error, no server code`);
  } finally {
    await browser.close();
  }
}

try {
  await served(oj, ["dev", app, "--port", String(PORT)], {}, async (stderr) => {
    await checkPage("start-dev", stderr);
  });

  const out = path.join(app, "dist-e2e");
  execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)} --out ${JSON.stringify(out)}`, {
    stdio: ["ignore", "ignore", "inherit"],
  });
  await served(
    "node",
    [path.join(out, "server.mjs")],
    { cwd: out, env: { ...process.env, PORT: String(PORT) } },
    async (stderr) => checkPage("start build", stderr),
  );
  console.log("\nSTART ENV FNS PASSED");
} catch (e) {
  console.error(`\nSTART ENV FNS FAILED: ${e.message}`);
  process.exitCode = 1;
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
