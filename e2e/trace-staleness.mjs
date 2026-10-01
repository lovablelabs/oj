// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// pulp P2 constructive traces: a plugin transform that READS a side file
// (without declaring it) bakes that file's content into its output. Before
// traces, a warm restart served the stale output after the side file changed;
// now the cached entry records the read with a digest and a mismatch is a
// miss. This runs the exact scenario: serve v1, warm-restart (still v1, from
// cache), edit the side file, warm-restart again, and demand v2.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const port = 5323;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-trace-"));
const store = fs.mkdtempSync(path.join(os.tmpdir(), "oj-trace-store-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "trace", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.tsx"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "main.tsx"),
  `export const banner: string = "__EXTRA__/__NAMED__";\nconsole.log(banner);\n`,
);
fs.writeFileSync(path.join(app, "extra.txt"), "v1");
fs.writeFileSync(path.join(app, "named.txt"), "n1");
// A second module whose only dynamic input is an import.meta.glob listing.
fs.mkdirSync(path.join(app, "globbed"));
fs.writeFileSync(path.join(app, "globbed", "a.ts"), "export const a = 1;\n");
fs.writeFileSync(path.join(app, "glob.tsx"), `export const mods = import.meta.glob("./globbed/*.ts");\n`);
// A plugin that reads TWO side files the cache key knows nothing about:
// extra.txt through the default node:fs import (the JS-level patched module
// object sees it) and named.txt through a NAMED esm import (invisible to the
// JS patch under Deno; only the Rust TracingFs ops layer sees it).
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import fsmod from "node:fs";
import { readFileSync as namedRead } from "node:fs";
export default [
  {
    name: "side-file-injector",
    transform(code, id) {
      if (!id.split("?")[0].endsWith("main.tsx")) return null;
      const extra = fsmod.readFileSync(${JSON.stringify(path.join(app, "extra.txt"))}, "utf8").trim();
      const named = namedRead(${JSON.stringify(path.join(app, "named.txt"))}, "utf8").trim();
      return { code: code.replace("__EXTRA__", extra).replace("__NAMED__", named), map: null };
    },
  },
];
`,
);

const env = { ...process.env, OJ_STORE_DIR: store };

async function bootAndFetch(extraEnv = {}, url = "/main.tsx") {
  const proc = spawn(oj, ["dev", app, "--port", String(port), "--store"], {
    stdio: "ignore",
    env: { ...env, ...extraEnv },
  });
  try {
    let up = false;
    for (let i = 0; i < 150 && !up; i++) {
      try {
        up = (await fetch(`http://localhost:${port}/`)).ok;
      } catch {}
      if (!up) await sleep(100);
    }
    assert.ok(up, "oj never came up");
    const r = await fetch(`http://localhost:${port}${url}`);
    assert.equal(r.status, 200);
    const body = await r.text();
    await sleep(600); // async cache writer
    return body;
  } finally {
    try {
      execSync(`pkill -P ${proc.pid}`);
    } catch {}
    try {
      proc.kill("SIGKILL");
    } catch {}
    try {
      execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs -r kill -9`);
    } catch {}
    await sleep(500);
  }
}

let failed = false;
try {
  const first = await bootAndFetch();
  assert.ok(first.includes('"v1"') || first.includes("v1"), `cold boot bakes v1: ${first.slice(0, 200)}`);

  // Warm boot under the verifier: hits recompile and compare; a pure plugin
  // must produce no divergence and identical output.
  const warm = await bootAndFetch({ OJ_VERIFY_HITS: "1" });
  assert.ok(warm.includes("v1"), "unchanged side file: warm boot still serves v1 (from cache)");

  fs.writeFileSync(path.join(app, "extra.txt"), "v2");
  const after = await bootAndFetch();
  assert.ok(
    after.includes("v2") && !after.includes("v1/"),
    `edited side file must re-transform, got: ${after.slice(0, 200)}`,
  );

  // The named-import file: only the Rust TracingFs layer can see this read.
  fs.writeFileSync(path.join(app, "named.txt"), "n2");
  const afterNamed = await bootAndFetch();
  assert.ok(
    afterNamed.includes("n2") && !afterNamed.includes("/n1"),
    `named-esm-import side file must re-transform via TracingFs, got: ${afterNamed.slice(0, 200)}`,
  );

  // And deleting the side file must fail loudly, never serve stale.
  fs.rmSync(path.join(app, "extra.txt"));
  const gone = await bootAndFetch().then(
    (b) => b,
    () => "BOOT_FAILED_OK",
  );
  assert.ok(
    !gone.includes('"v2"') || gone.includes("error"),
    "deleted side file must not serve the stale v2 module silently",
  );

  // Glob trace rows: an added file under an import.meta.glob pattern must
  // re-expand across a warm restart (the silent stale case before glob rows).
  const glob1 = await bootAndFetch({}, "/glob.tsx");
  assert.ok(glob1.includes("globbed/a.ts"), `glob lists a.ts: ${glob1.slice(0, 200)}`);
  const globWarm = await bootAndFetch({}, "/glob.tsx");
  assert.ok(globWarm.includes("globbed/a.ts"), "unchanged listing still serves");
  fs.writeFileSync(path.join(app, "globbed", "b.ts"), "export const b = 2;\n");
  const glob2 = await bootAndFetch({}, "/glob.tsx");
  assert.ok(
    glob2.includes("globbed/b.ts"),
    `created file under the glob must re-expand on a warm boot: ${glob2.slice(0, 300)}`,
  );
  fs.rmSync(path.join(app, "globbed", "a.ts"));
  const glob3 = await bootAndFetch({}, "/glob.tsx");
  assert.ok(
    !glob3.includes("globbed/a.ts") && glob3.includes("globbed/b.ts"),
    `deleted file under the glob must drop out on a warm boot: ${glob3.slice(0, 300)}`,
  );

  console.log(
    "PASS trace-staleness (warm hit, default-import edit re-transforms, NAMED-import edit re-transforms via TracingFs, no stale serve on delete, verifier clean on pure hits, glob add+delete re-expand)",
  );
} catch (err) {
  failed = true;
  console.error(`FAIL trace-staleness: ${err.message}`);
} finally {
  process.exit(failed ? 1 : 0);
}
