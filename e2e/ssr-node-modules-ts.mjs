// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Plain-mode SSR (`oj dev --ssr`): a dependency that ships raw TypeScript (an
// exports map pointing straight at .ts/.tsx sources) must render. The runtime
// refuses to type-strip under node_modules, and Vite never externalizes a
// resolved file it cannot hand to Node (canExternalizeFile allows only
// js/mjs/cjs), so such files go through the transform pipeline instead.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { writeTsSourceDep } from "./lib/ts-source-dep.mjs";
import { settles } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const playground = path.join(repo, "playground");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5395);

if (!fs.existsSync(path.join(playground, "node_modules", "react"))) {
  console.log("SKIP ssr node_modules ts: playground deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

// The fixture mutations live inside the try so an early throw still restores
// them: the finally below is safe to run against any partial state.
const dep = path.join(playground, "node_modules", "ts-source-dep");
const out = path.join(playground, "dist-ssr-ts-dep");
const counterFile = path.join(playground, "src", "Counter.tsx");
const originalCounter = fs.readFileSync(counterFile, "utf8");
const configFile = path.join(playground, "oj.config.ts");
const originalConfig = fs.readFileSync(configFile, "utf8");
let srv;
let log = "";
try {
  writeTsSourceDep(path.join(playground, "node_modules"));
  fs.writeFileSync(
    counterFile,
    originalCounter
      .replace(
        'import { useState } from "react";',
        'import { useState } from "react";\nimport { describeBadge } from "ts-source-dep/badge";',
      )
      .replace("{label}: {count}", '{`${describeBadge({ label, variant: "outline" })}:${count}`}'),
  );
  must(
    fs.readFileSync(counterFile, "utf8").includes("describeBadge"),
    "Counter.tsx patch did not apply (fixture drifted)",
  );
  fs.rmSync(path.join(playground, ".oj-cache", "v1", "ssr"), { recursive: true, force: true });

  srv = spawn(oj, ["dev", playground, "--ssr", "src/entry-server.tsx", "--port", String(PORT)], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));

  let body = "";
  const ok = await settles(
    async () => {
      try {
        body = await (await fetch(`http://localhost:${PORT}/`)).text();
        return body.includes("badge-outline");
      } catch {
        return false;
      }
    },
    { timeoutMs: 30000, pollMs: 500 },
  );
  must(ok, `SSR never rendered the dependency marker:\n${body.slice(0, 800)}\n---\n${log.slice(-1500)}`);
  must(
    body.includes("badge-outline:bold:nodenext:"),
    `SSR html missing "badge-outline:bold:nodenext:":\n${body.slice(0, 800)}`,
  );
  must(
    !log.includes("TYPE_STRIPPING") && !log.includes("ERR_MODULE_NOT_FOUND"),
    `server log shows a native dependency load failure:\n${log.slice(-1500)}`,
  );
  console.log("ssr dev: raw-TS dependency rendered through the pipeline");
  srv.kill("SIGKILL");

  // Production build: dependencies stay external by default here, but Vite
  // resolves a specifier before externalizing it (canExternalizeFile), so a
  // raw-TS dependency bundles into the server bundle instead of surviving as
  // a bare import plain Node would have to type-strip.
  fs.rmSync(out, { recursive: true, force: true });
  execSync(`${oj} build playground --ssr src/entry-server.tsx --out dist-ssr-ts-dep`, {
    cwd: repo,
    stdio: ["ignore", "ignore", "inherit"],
  });
  // The SSR build code-splits routes, so the dep can land in a hashed sibling
  // chunk: every emitted js file counts, not just entry-server.mjs.
  const builtJs = (dir) =>
    fs
      .readdirSync(dir, { recursive: true })
      .filter((f) => /\.(mjs|js)$/.test(f))
      .map((f) => fs.readFileSync(path.join(dir, f), "utf8"))
      .join("\n");
  const bareDepImport = /from\s*["']ts-source-dep(?:\/[^"']*)?["']/;
  must(!bareDepImport.test(builtJs(out)), "built server bundle still imports the raw-TS dependency bare");
  const mod = await import(pathToFileURL(path.join(out, "entry-server.mjs")).href);
  const html = String(await mod.render());
  must(
    html.includes("badge-outline:bold:nodenext:"),
    `built render() missing "badge-outline:bold:nodenext:":\n${html.slice(0, 800)}`,
  );
  console.log("ssr build: raw-TS dependency bundled into the server bundle");

  // An id listed VERBATIM in `ssr.external` is forced external without the
  // resolve check, as in Vite (a package-name entry still goes through it).
  // The bundle then cannot execute — Vite's documented sharp edge — so
  // prerender is off for this build and only the decision is asserted.
  fs.writeFileSync(
    configFile,
    originalConfig
      .replace("  build: {", '  ssr: { external: ["ts-source-dep/badge"] },\n  build: {')
      .replace('prerender: ["/", "/about"],', "prerender: [],"),
  );
  must(
    fs.readFileSync(configFile, "utf8").includes("ts-source-dep/badge"),
    "oj.config.ts patch did not apply (fixture drifted)",
  );
  fs.rmSync(out, { recursive: true, force: true });
  execSync(`${oj} build playground --ssr src/entry-server.tsx --out dist-ssr-ts-dep`, {
    cwd: repo,
    stdio: ["ignore", "ignore", "inherit"],
  });
  must(
    /from\s*["']ts-source-dep\/badge["']/.test(builtJs(out)),
    "a verbatim ssr.external id should stay a bare import of the server bundle",
  );
  console.log("\nSSR NODE_MODULES TS PASSED");
} catch (e) {
  console.error("\nSSR NODE_MODULES TS FAILED:", e.message);
  process.exitCode = 1;
} finally {
  fs.writeFileSync(counterFile, originalCounter);
  fs.writeFileSync(configFile, originalConfig);
  fs.rmSync(dep, { recursive: true, force: true });
  fs.rmSync(out, { recursive: true, force: true });
  srv?.kill("SIGKILL");
  fs.rmSync(path.join(playground, ".oj-cache", "v1", "ssr"), { recursive: true, force: true });
}
