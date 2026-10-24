// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A stylesheet's plugin transforms in dev run in Vite's slots: "pre" plugins
// see the source, "normal" ones the CSS vite:css makes of it, "post" ones what
// vite:css-post serves (the CSS for `?direct`, the JS module for an import or
// `?inline`), each request a module of its own. Plugins record what they see
// under oj dev; when the start-app fixture's Vite is installed, the same app
// runs under Vite's dev server and the records must be identical.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath, pathToFileURL } from "node:url";
import { sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6376;
const VITE_PORT = 6377;
const SASS = "sass@1.93.2";

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-css-slots-"));
const seenFile = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "oj-css-slots-seen-")), "seen.txt");
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write("package.json", JSON.stringify({ name: "css-slots", type: "module" }));
write("src/a.scss", "$c: red;\n.a { color: $c; }\n");
write("src/b.scss", "$c: blue;\n.b { color: $c; }\n");
write("src/m.module.css", ".m { color: teal; }\n");
write(
  "src/main.js",
  `import "./a.scss";\nimport b from "./b.scss?inline";\nimport m from "./m.module.css";\nconsole.log(b, m);\n`,
);
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
write(
  "plugins.mjs",
  `import { appendFileSync } from "node:fs";
const record = (name) => function (code, id) {
  if (!/\\.(scss|css)(\\?|$)/.test(id)) return null;
  const kind = code.includes("$c")
    ? "source"
    : /export (default|const)|^$/.test(code) || code.startsWith("import")
      ? "js"
      : /\\.m[\\s{]/.test(code) || !id.includes(".module.")
        ? "css"
        : "css-scoped";
  appendFileSync(${JSON.stringify(seenFile)}, \`\${name} \${id.slice(id.lastIndexOf("/") + 1)} \${kind}\\n\`);
  return null;
};
const spy = (name, enforce) => ({ name, enforce, transform: record(name) });
const ordered = (name, enforce, order) => ({ name, enforce, transform: { order, handler: record(name) } });
export default [
  spy("post", "post"),
  spy("normal"),
  spy("pre", "pre"),
  ordered("normal-order-pre", undefined, "pre"),
  ordered("pre-order-post", "pre", "post"),
  ordered("post-order-pre", "post", "pre"),
];
`,
);
write("vite.config.js", `import plugins from "./plugins.mjs";\nexport default { plugins };\n`);

const URLS = ["/src/a.scss", "/src/a.scss?direct", "/src/b.scss?inline", "/src/m.module.css"];
// The records start empty for each server; oj's eager crawl may compile
// the imported sheets before the requests, as Vite's warmup would.
const record = async (port) => {
  for (const u of URLS) {
    const res = await fetch(`http://localhost:${port}${u}`, { headers: { accept: "*/*" } });
    const body = await res.text();
    assert.equal(res.status, 200, `${u}: ${body}`);
  }
  return fs.readFileSync(seenFile, "utf8").split("\n").filter(Boolean).sort();
};

let failed = false;
let srv;
try {
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const ojSeen = await record(PORT);
  for (const line of ["pre a.scss source", "normal a.scss css", "post a.scss js", "post a.scss?direct css"]) {
    assert.ok(ojSeen.includes(line), `${line} in:\n${ojSeen.join("\n")}`);
  }
  assert.ok(ojSeen.includes("normal m.module.css css-scoped"), ojSeen.join("\n"));
  console.log(`[dev] ${ojSeen.length} slot records`);
  srv.kill("SIGKILL");
  await sleep(300);

  const viteDir = path.join(repo, "e2e", "fixtures", "start-app", "node_modules", "vite");
  let sassReady = false;
  try {
    execSync(`npm install ${SASS} --no-audit --no-fund --loglevel=error`, { cwd: app, stdio: "ignore" });
    sassReady = true;
  } catch {}
  if (fs.existsSync(viteDir) && sassReady) {
    const { createServer } = await import(pathToFileURL(path.join(viteDir, "dist", "node", "index.js")).href);
    fs.rmSync(seenFile, { force: true });
    const vite = await createServer({ root: app, logLevel: "error", server: { port: VITE_PORT } });
    await vite.listen();
    try {
      assert.deepEqual(ojSeen, await record(VITE_PORT), "what each slot sees, as under Vite's dev server");
    } finally {
      await vite.close();
    }
    console.log("[vite] identical to a live Vite dev server");
  } else {
    console.log("[vite] fixture Vite or sass not installed; skipped the live comparison");
  }
  console.log("CSS-PLUGIN-SLOTS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-PLUGIN-SLOTS E2E FAILED:", err.message);
} finally {
  srv?.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(path.dirname(seenFile), { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
