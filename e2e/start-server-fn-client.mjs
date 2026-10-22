// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start, a server function called from the browser. The client
// bundle replaces the handler with `createClientRpc(id)`; that rewrite found
// the handler's end by counting parens, so a `)` in a comment inside the
// handler (`// 1) ...`) ended it early and left the rest of the body in the
// bundle: a rolldown PARSE_ERROR and a blank page. The handler here carries
// that comment, a string and a regex with unbalanced brackets; the page calls
// it after hydration, in dev and in a build, so the client stub parses and
// its id reaches the server's handler.

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
const PORT = Number(process.env.OJ_E2E_PORT || 3108);

const installed = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start server fn client: fixture deps not installed");
  process.exit(0);
}
let pw = null;
try {
  pw = await import("playwright");
} catch {}
if (!pw) {
  console.log("SKIP start server fn client: playwright not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-server-fn-"));
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
  "src/server/keys.functions.ts",
  [
    'import { createServerFn } from "@tanstack/react-start";',
    "",
    'export const saveKey = createServerFn({ method: "POST" })',
    "  .validator((d: { key: string }) => d)",
    "  .handler(async ({ data }) => {",
    "    // 1) chave já gravada localmente para este ambiente",
    "    /* 2) senão, grava :) */",
    '    const close = ")((";',
    "    if (/^[(]+/.test(close)) return { saved: `never ${close}` };",
    "    return { saved: `ok-${data.key}` };",
    "  });",
    "",
  ].join("\n"),
);
const aboutPath = path.join(app, "src/routes/about.tsx");
let about = fs.readFileSync(aboutPath, "utf8");
about = about.replace(
  'import { rootRoute } from "./__root";',
  'import { rootRoute } from "./__root";\nimport { useEffect, useState } from "react";\nimport { saveKey } from "../server/keys.functions";\n' +
    "function SaveKey() {\n" +
    '  const [out, setOut] = useState("pending");\n' +
    "  useEffect(() => {\n" +
    '    saveKey({ data: { key: "k1" } }).then((r) => setOut(r.saved), (e) => setOut("error " + String(e)));\n' +
    "  }, []);\n" +
    '  return <p id="saved">{out}</p>;\n' +
    "}",
);
about = about.replace(
  '<h1 className="fixture-heading">about-page-marker</h1>',
  '<h1 className="fixture-heading">about-page-marker</h1>\n      <SaveKey />',
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

async function callsFromBrowser(label, stderr) {
  const browser = await pw.chromium.launch();
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    await page.goto(`http://localhost:${PORT}/about`);
    const want = "ok-k1";
    const got = await page
      .waitForFunction((w) => document.querySelector("#saved")?.textContent === w, want, { timeout: 20000 })
      .then(() => want)
      .catch(async () =>
        page
          .locator("#saved")
          .textContent()
          .catch(() => null),
      );
    must(
      got === want,
      `${label}: the server fn answered ${JSON.stringify(got)}, want ${want}; page errors: ${errors.join(" | ")}\n${stderr().slice(-2000)}`,
    );
    console.log(`${label}: a server fn with a ) in a handler comment runs from the browser`);
  } finally {
    await browser.close();
  }
}

try {
  await served(oj, ["dev", app, "--port", String(PORT)], {}, async (stderr) => {
    await callsFromBrowser("start-dev", stderr);
    must(!/PARSE_ERROR|client bundle failed/.test(stderr()), `start-dev: the client bundle failed:\n${stderr()}`);
  });

  const out = path.join(app, "dist-e2e");
  execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)} --out ${JSON.stringify(out)}`, {
    stdio: ["ignore", "ignore", "inherit"],
  });
  await served(
    "node",
    [path.join(out, "server.mjs")],
    { cwd: out, env: { ...process.env, PORT: String(PORT) } },
    async (stderr) => callsFromBrowser("start build", stderr),
  );
  console.log("\nSTART SERVER FN CLIENT PASSED");
} catch (e) {
  console.error(`\nSTART SERVER FN CLIENT FAILED: ${e.message}`);
  process.exitCode = 1;
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
