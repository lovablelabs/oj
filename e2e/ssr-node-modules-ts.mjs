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

const dep = writeTsSourceDep(path.join(playground, "node_modules"));
const counterFile = path.join(playground, "src", "Counter.tsx");
const originalCounter = fs.readFileSync(counterFile, "utf8");
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

const srv = spawn(oj, ["dev", playground, "--ssr", "src/entry-server.tsx", "--port", String(PORT)], {
  stdio: ["ignore", "pipe", "pipe"],
});
let log = "";
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));

try {
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
  must(body.includes("badge-outline:bold:"), `SSR html missing "badge-outline:bold:":\n${body.slice(0, 800)}`);
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
  const out = path.join(playground, "dist-ssr-ts-dep");
  fs.rmSync(out, { recursive: true, force: true });
  execSync(`${oj} build playground --ssr src/entry-server.tsx --out dist-ssr-ts-dep`, {
    cwd: repo,
    stdio: ["ignore", "ignore", "inherit"],
  });
  try {
    const bundle = fs.readFileSync(path.join(out, "entry-server.mjs"), "utf8");
    must(
      !/from\s*["']ts-source-dep(?:\/[^"']*)?["']/.test(bundle),
      "built server bundle still imports the raw-TS dependency bare",
    );
    const mod = await import(pathToFileURL(path.join(out, "entry-server.mjs")).href);
    const html = String(await mod.render());
    must(html.includes("badge-outline:bold:"), `built render() missing "badge-outline:bold:":\n${html.slice(0, 800)}`);
  } finally {
    fs.rmSync(out, { recursive: true, force: true });
  }
  console.log("\nSSR NODE_MODULES TS PASSED");
} catch (e) {
  console.error("\nSSR NODE_MODULES TS FAILED:", e.message);
  process.exitCode = 1;
} finally {
  fs.writeFileSync(counterFile, originalCounter);
  fs.rmSync(dep, { recursive: true, force: true });
  srv.kill("SIGKILL");
  fs.rmSync(path.join(playground, ".oj-cache", "v1", "ssr"), { recursive: true, force: true });
}
