// SPDX-License-Identifier: MIT

// Inline `<style>` and `style="url(...)"` in a dev page, held to the fixture's
// Vite: the same page is served by Vite's dev server and by `oj dev`, and
//   - every html-proxy id a plugin transform sees under Vite, oj's plugins see
//     too (Vite's devHtmlHook runs both shapes through the plugin container),
//   - each compiled `<style>` and attribute means the same CSS (both sides
//     normalized through lightningcss, since oj prints minified CSS in dev),
// with plugins, with a plugin-less config and with no config file at all.
// Run with a built target/debug/oj (or OJ_BIN).
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const nm = path.join(here, "fixtures", "start-app", "node_modules");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 6896;

if (!fs.existsSync(path.join(nm, "vite")) || !fs.existsSync(path.join(nm, "lightningcss"))) {
  console.log("SKIP html inline style vite: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const { createServer } = await import(path.join(nm, "vite/dist/node/index.js"));
const lightningcss = await import(path.join(nm, "lightningcss/node/index.mjs"));

// Without an alias the `@/` urls are left out: Vite keeps an unresolvable
// url as written while oj rebases it like a relative one, which the browser
// resolves to the same place.
const page = (alias) => `<!doctype html><html><head>
<style>@import "./src/base.css";
${alias ? ".hero { background: url(@/bg.png); }\n" : ""}.rel { background-image: url('./src/bg.png') }
.abs { background: url(/src/bg.png) }
.mark { color: __MARK__ }
</style>
</head><body>
<div style="background: url(./src/bg.png)"></div>
<div style="background: url('./src/bg.png'); margin-top: 0; color: __MARK__"></div>
${alias ? `<div style='background-image: url("@/bg.png")'></div>\n` : ""}<div style="background: url(https://example.com/x.png)"></div>
<p style="color: red"></p>
<style>.second { color: blue }</style>
</body></html>
`;

// Records every html-proxy id a transform sees, and rewrites a marker the
// way a CSS plugin would, so a skipped transform shows in the output too.
const PLUGIN = `{
  name: "html-style-probe",
  transform(code, id) {
    if (!id.includes("html-proxy")) return;
    fs.appendFileSync(process.env.OJ_STYLE_IDS, id + "\\n");
    return code.replaceAll("__MARK__", "rebeccapurple");
  },
}`;

function makeApp(config, html) {
  const app = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oj-html-style-")));
  fs.mkdirSync(path.join(app, "src"));
  fs.symlinkSync(nm, path.join(app, "node_modules"), "dir");
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"html-style","private":true,"type":"module"}');
  fs.writeFileSync(path.join(app, "src/base.css"), ".base { margin: 0 }\n");
  fs.writeFileSync(path.join(app, "src/bg.png"), Buffer.alloc(4, 7));
  fs.writeFileSync(path.join(app, "index.html"), html);
  if (config) fs.writeFileSync(path.join(app, "vite.config.js"), config(app));
  return app;
}

// The compiled `<style>` bodies and `style` attributes, in page order.
function styles(html) {
  const sheets = [...html.matchAll(/<style>([\s\S]*?)<\/style>/g)].map((m) => m[1]);
  const attrs = [...html.matchAll(/style=(?:"([^"]*)"|'([^']*)')/g)].map((m) =>
    (m[1] ?? m[2]).replaceAll("&quot;", '"'),
  );
  return { sheets, attrs };
}
const norm = {
  sheet: (css) => lightningcss.transform({ filename: "x.css", code: Buffer.from(css), minify: true }).code.toString(),
  attr: (css) =>
    lightningcss.transformStyleAttribute({ filename: "x.css", code: Buffer.from(css), minify: true }).code.toString(),
};

async function viaVite(app, html, idLog) {
  process.env.OJ_STYLE_IDS = idLog;
  const vite = await createServer({
    root: app,
    logLevel: "silent",
    configFile: fs.existsSync(path.join(app, "vite.config.js")) ? undefined : false,
    server: { middlewareMode: true, ws: false },
  });
  try {
    return await vite.transformIndexHtml("/index.html", html, "/");
  } finally {
    await vite.close();
  }
}

async function viaOj(app, idLog) {
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    stdio: ["ignore", "pipe", "pipe"],
    detached: true,
    env: { ...process.env, OJ_STYLE_IDS: idLog },
  });
  srv.stdout.on("data", (c) => (log += c));
  srv.stderr.on("data", (c) => (log += c));
  try {
    const res = await waitUp(`http://localhost:${PORT}/`, { proc: srv });
    return await res.text();
  } catch (e) {
    throw new Error(`${e.message}; log tail:\n${log.slice(-2000)}`);
  } finally {
    const exited = new Promise((r) => srv.once("exit", r));
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
    await exited;
  }
}

const readIds = (file, app) =>
  fs.existsSync(file)
    ? [...new Set(fs.readFileSync(file, "utf8").trim().split("\n").filter(Boolean))]
        .map((id) => id.replaceAll(app, "<root>"))
        .sort()
    : [];

async function compare(name, config, { probe }) {
  const html = page(config !== null);
  const app = makeApp(config, html);
  try {
    const viteIds = path.join(app, "vite-ids.txt");
    const ojIds = path.join(app, "oj-ids.txt");
    const expected = styles(await viaVite(app, html, viteIds));
    const actual = styles(await viaOj(app, ojIds));
    for (const kind of ["sheets", "attrs"]) {
      const want = expected[kind].map(norm[kind === "sheets" ? "sheet" : "attr"]);
      const got = actual[kind].map(norm[kind === "sheets" ? "sheet" : "attr"]);
      if (JSON.stringify(want) !== JSON.stringify(got)) {
        throw new Error(
          `${name}: ${kind} differ from Vite's\nvite:\n  ${want.join("\n  ")}\noj:\n  ${got.join("\n  ")}`,
        );
      }
    }
    if (probe) {
      const want = readIds(viteIds, app);
      const got = readIds(ojIds, app);
      if (want.length === 0) throw new Error(`${name}: Vite ran no html-proxy transform`);
      if (JSON.stringify(want) !== JSON.stringify(got)) {
        throw new Error(
          `${name}: plugin transform ids differ\nvite:\n  ${want.join("\n  ")}\noj:\n  ${got.join("\n  ")}`,
        );
      }
      if (!actual.sheets.join("").includes("rebeccapurple") && !actual.sheets.join("").includes("#639")) {
        throw new Error(`${name}: the plugin's transform is missing from the <style>`);
      }
    }
    console.log(`html inline style: ${name} matches Vite`);
  } finally {
    fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}

const alias = (app) => `resolve: { alias: { "@": ${JSON.stringify(path.join(app, "src"))} } }`;
let failed = false;
try {
  await compare(
    "with a plugin",
    (app) => `import fs from "node:fs";\nexport default { plugins: [${PLUGIN}], ${alias(app)} };\n`,
    {
      probe: true,
    },
  );
  await compare("a config with no plugins", (app) => `export default { ${alias(app)} };\n`, { probe: false });
  await compare("no config file", null, { probe: false });
  console.log("HTML-INLINE-STYLE-VITE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HTML-INLINE-STYLE-VITE E2E FAILED:", err.message);
}
await sleep(100);
process.exit(failed ? 1 : 0);
