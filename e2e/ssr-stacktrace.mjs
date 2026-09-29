// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `oj dev --ssr`: an error thrown while rendering reports stack frames at the
// ORIGINAL file:line of the .ts source, like Vite's ssrFixStacktrace. The
// /@ssr-module response carries an inline source map, the SSR rewrite keeps
// every line in place (hoisted imports share line 1, removed multi-line imports
// keep their line breaks), and the runner maps frames through the map.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5236);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-ssrstack-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "ssrstack", version: "1.0.0", type: "module" }),
);
fs.writeFileSync(
  path.join(app, "index.html"),
  '<!doctype html><html><head><title>t</title></head><body><div id="root"></div></body></html>',
);
fs.writeFileSync(path.join(app, "src", "util.ts"), "export function helper(n: number): number {\n  return n;\n}\n");
// A multi-line import (3 lines the SSR rewrite removes) above the throw, so a
// line-collapsing transform would report the wrong line.
fs.writeFileSync(
  path.join(app, "src", "lib.ts"),
  [
    "import {",
    "  helper,",
    '} from "./util";',
    "export function boom(): never {",
    "  helper(1);",
    '  throw new Error("ssr-boom-marker");',
    "}",
    "",
  ].join("\n"),
);
fs.writeFileSync(
  path.join(app, "src", "entry-server.ts"),
  ['import { boom } from "./lib";', "export function render(_url: string): string {", "  return boom();", "}", ""].join(
    "\n",
  ),
);

const srv = spawn(oj, ["dev", app, "--ssr", "src/entry-server.ts", "--port", String(PORT)], {
  stdio: ["ignore", "ignore", "inherit"],
});
let failed = false;
try {
  let body = "";
  await settles(
    async () => {
      try {
        body = await (await fetch(`http://localhost:${PORT}/`)).text();
        return body.includes("ssr-boom-marker");
      } catch {
        return false;
      }
    },
    { timeoutMs: 60000, pollMs: 500 },
  );
  if (!body.includes("ssr-boom-marker"))
    throw new Error(`the render error never reached the response:\n${body.slice(0, 400)}`);
  const stack = body.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&amp;/g, "&");
  const libFrame = stack.match(/at boom \(([^)]*)\)/);
  if (!libFrame) throw new Error(`no frame for boom():\n${stack.slice(0, 600)}`);
  if (!/\/src\/lib\.ts:6:\d+$/.test(libFrame[1])) {
    throw new Error(`boom() frame is not at the original src/lib.ts:6 (got ${libFrame[1]}):\n${stack.slice(0, 600)}`);
  }
  const entryFrame = stack.match(/at (?:Module\.)?render \(([^)]*)\)/);
  if (!entryFrame || !/\/src\/entry-server\.ts:3:\d+$/.test(entryFrame[1])) {
    throw new Error(
      `render() frame is not at the original src/entry-server.ts:3 (got ${entryFrame?.[1]}):\n${stack.slice(0, 600)}`,
    );
  }
  console.log(
    `ssr-stacktrace: frames mapped to source (${libFrame[1].split("/").pop()}, ${entryFrame[1].split("/").pop()})`,
  );
  console.log("SSR STACKTRACE E2E PASSED");
} catch (e) {
  failed = true;
  console.error("SSR STACKTRACE E2E FAILED:", e.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
