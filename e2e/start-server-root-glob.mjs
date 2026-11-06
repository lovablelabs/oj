// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start server code with a root-absolute `import.meta.glob`, the
// shape a generated auth middleware uses: inside the middleware's server
// handler, `import.meta.glob('/src/integrations/auth/factory.server.ts')`,
// looked up by that same key, with a fallback when the lookup is empty.
//
// The rule (Vite's importMetaGlob): `toAbsoluteGlob` joins a `/` pattern to
// the app root, never the filesystem root, and `resolvePaths` keys every file
// of a glob that is not all-relative root-absolute (`/src/...`). oj's dev SSR
// pipeline globbed `/src/...` on the filesystem root and keyed matches `./x`,
// so the lookup came back empty and the middleware silently took its
// fallback. The page calls the server fn after hydration, in dev (the Rust
// SSR pipeline) and in a build (the Start server bundle), and must see the
// factory's answer and the root-absolute keys.

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
const PORT = Number(process.env.OJ_E2E_PORT || 3112);

const installed = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start server root glob: fixture deps not installed");
  process.exit(0);
}
let pw = null;
try {
  pw = await import("playwright");
} catch {}
if (!pw) {
  console.log("SKIP start server root glob: playwright not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-root-glob-"));
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/[\\/](node_modules|\.oj-cache)$/.test(src),
});
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");

const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write("src/integrations/auth/factory.server.ts", 'export async function authenticate() {\n  return "factory";\n}\n');
write("src/integrations/auth/session.server.ts", "export const session = 1;\n");
write(
  "src/integrations/auth/middleware.ts",
  [
    'import { createMiddleware } from "@tanstack/react-start";',
    "",
    'export const auth = createMiddleware({ type: "function" }).server(async ({ next }) => {',
    '  const modules = import.meta.glob("/src/integrations/auth/factory.server.ts");',
    '  const load = modules["/src/integrations/auth/factory.server.ts"];',
    "  const via = load",
    "    ? await ((await load()) as { authenticate: () => Promise<string> }).authenticate()",
    '    : "fallback";',
    '  const keys = Object.keys(import.meta.glob("/src/integrations/auth/*.server.ts")).sort();',
    "  return next({ context: { via, keys } });",
    "});",
    "",
  ].join("\n"),
);
write(
  "src/server/whoami.functions.ts",
  [
    'import { createServerFn } from "@tanstack/react-start";',
    'import { auth } from "../integrations/auth/middleware";',
    "",
    'export const whoami = createServerFn({ method: "GET" })',
    "  .middleware([auth])",
    '  .handler(async ({ context }) => `${context.via}|${context.keys.join(",")}`);',
    "",
  ].join("\n"),
);
const aboutPath = path.join(app, "src/routes/about.tsx");
let about = fs.readFileSync(aboutPath, "utf8");
about = about.replace(
  'import { rootRoute } from "./__root";',
  'import { rootRoute } from "./__root";\nimport { useEffect, useState } from "react";\nimport { whoami } from "../server/whoami.functions";\n' +
    "function WhoAmI() {\n" +
    '  const [out, setOut] = useState("pending");\n' +
    "  useEffect(() => {\n" +
    '    whoami().then((r) => setOut(r), (e) => setOut("error " + String(e)));\n' +
    "  }, []);\n" +
    '  return <p id="who">{out}</p>;\n' +
    "}",
);
about = about.replace(
  '<h1 className="fixture-heading">about-page-marker</h1>',
  '<h1 className="fixture-heading">about-page-marker</h1>\n      <WhoAmI />',
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

async function middlewareFindsTheFactory(label, stderr) {
  const browser = await pw.chromium.launch();
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    await page.goto(`http://localhost:${PORT}/about`);
    // The single-file lookup found its loader (not the fallback), and the
    // wildcard glob's keys are root-absolute.
    const want = "factory|/src/integrations/auth/factory.server.ts,/src/integrations/auth/session.server.ts";
    const got = await page
      .waitForFunction((w) => document.querySelector("#who")?.textContent === w, want, { timeout: 20000 })
      .then(() => want)
      .catch(async () =>
        page
          .locator("#who")
          .textContent()
          .catch(() => null),
      );
    must(
      got === want,
      `${label}: the server fn answered ${JSON.stringify(got)}, want ${want}; page errors: ${errors.join(" | ")}\n${stderr().slice(-2000)}`,
    );
    console.log(`${label}: a root-absolute glob in server code resolves from the app root, keyed /src/...`);
  } finally {
    await browser.close();
  }
}

try {
  await served(oj, ["dev", app, "--port", String(PORT)], {}, (stderr) =>
    middlewareFindsTheFactory("start-dev", stderr),
  );

  const out = path.join(app, "dist-e2e");
  execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)} --out ${JSON.stringify(out)}`, {
    stdio: ["ignore", "ignore", "inherit"],
  });
  await served(
    "node",
    [path.join(out, "server.mjs")],
    { cwd: out, env: { ...process.env, PORT: String(PORT) } },
    (stderr) => middlewareFindsTheFactory("start build", stderr),
  );
  console.log("\nSTART SERVER ROOT GLOB PASSED");
} catch (e) {
  console.error(`\nSTART SERVER ROOT GLOB FAILED: ${e.message}`);
  process.exitCode = 1;
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
