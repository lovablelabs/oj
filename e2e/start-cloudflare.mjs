// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A TanStack Start app on Cloudflare: with @cloudflare/vite-plugin in the
// config, `oj build` must produce what `vite build` produces for that plugin,
// a client build plus a Worker environment bundle for workerd (dist/<env>/
// index.js, the plugin's wrangler.json and deploy redirect), not a Node server.
// The Start fixture is copied with the plugin and a wrangler config added
// (the plugin and wrangler are installed at test time, or taken from
// OJ_E2E_CF_DEPS), built, and the output is run under workerd via
// `wrangler dev`; the SSR render must show every fixture feature.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6733;
const INSPECTOR_PORT = 6734;

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start cloudflare build: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-cf-"));
const app = path.join(tmp, "app");
const keep = !!process.env.OJ_E2E_KEEP;
const cleanup = () => {
  if (!keep) fs.rmSync(tmp, { recursive: true, force: true });
};

// The plugin and wrangler (with workerd) next to the app, so the config's
// import resolves up from the app while the app's own node_modules stays the
// fixture's.
function installCloudflareDeps() {
  const prepared = process.env.OJ_E2E_CF_DEPS;
  if (prepared) {
    fs.symlinkSync(path.join(path.resolve(prepared), "node_modules"), path.join(tmp, "node_modules"), "dir");
    return;
  }
  fs.writeFileSync(
    path.join(tmp, "package.json"),
    JSON.stringify({ name: "oj-cf-deps", private: true, type: "module" }),
  );
  execSync("npm install --no-audit --no-fund --no-package-lock @cloudflare/vite-plugin wrangler", {
    cwd: tmp,
    stdio: "inherit",
  });
}

function makeApp() {
  fs.mkdirSync(app);
  for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json"]) {
    fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
  }
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  fs.writeFileSync(
    path.join(app, "wrangler.jsonc"),
    [
      "{",
      '  "name": "oj-start-fixture",',
      '  "main": "@tanstack/react-start/server-entry",',
      '  "compatibility_date": "2025-09-01",',
      '  "compatibility_flags": ["nodejs_compat"],',
      '  "vars": { "EDITION": "fixture-edition" }',
      "}",
      "",
    ].join("\n"),
  );
  const original = fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8");
  const withImport = original.replace(
    'import { defineConfig } from "vite";',
    'import { defineConfig } from "vite";\nimport { cloudflare } from "@cloudflare/vite-plugin";',
  );
  // Insert the Cloudflare plugin ahead of tanstackStart(...) whatever options the
  // fixture passes it.
  const config = withImport.replace(
    /^(\s*)tanstackStart\(/m,
    '$1cloudflare({ viteEnvironment: { name: "ssr" } }),\n$1tanstackStart(',
  );
  if (config === original || config === withImport)
    throw new Error("fixture vite.config.ts changed shape; update this script");
  fs.writeFileSync(path.join(app, "vite.config.ts"), config);
}

function assertLayout() {
  const dist = path.join(app, "dist");
  const must = ["client", "ssr/index.js", "ssr/wrangler.json", "client/.assetsignore"];
  for (const rel of must) {
    if (!fs.existsSync(path.join(dist, rel))) throw new Error(`missing dist/${rel}`);
  }
  for (const rel of ["server.mjs", "worker.mjs", "server-bundle.mjs", "cf-loader.mjs", "cf-server.mjs"]) {
    if (fs.existsSync(path.join(dist, rel)))
      throw new Error(`dist/${rel} written: the Cloudflare build must not carry the Node server`);
  }
  if (!fs.readdirSync(path.join(dist, "client", "assets")).some((f) => f.endsWith(".js")))
    throw new Error("no client bundle");
  if (!fs.readFileSync(path.join(dist, "client", ".assetsignore"), "utf8").includes("wrangler.json")) {
    throw new Error("client/.assetsignore lacks wrangler.json (the plugin's client generateBundle did not run)");
  }

  const wrangler = JSON.parse(fs.readFileSync(path.join(dist, "ssr", "wrangler.json"), "utf8"));
  const expect = (cond, what) => {
    if (!cond) throw new Error(`dist/ssr/wrangler.json: ${what}: ${JSON.stringify(wrangler)}`);
  };
  expect(wrangler.name === "oj-start-fixture", "worker name");
  expect(wrangler.main === "index.js", "main is the entry chunk");
  expect(wrangler.no_bundle === true, "no_bundle");
  expect(wrangler.assets?.directory === "../client", "assets directory points at the client build");
  expect((wrangler.compatibility_flags || []).includes("nodejs_compat"), "compat flags kept");
  expect(wrangler.vars?.EDITION === "fixture-edition", "vars kept");
  expect(Array.isArray(wrangler.rules) && wrangler.rules[0]?.type === "ESModule", "ESModule rules");

  const deploy = JSON.parse(fs.readFileSync(path.join(app, ".wrangler", "deploy", "config.json"), "utf8"));
  if (path.resolve(app, ".wrangler/deploy", deploy.configPath) !== path.join(dist, "ssr", "wrangler.json")) {
    throw new Error(`deploy redirect points elsewhere: ${JSON.stringify(deploy)}`);
  }

  // Bundle shape: bundled for workerd, every bare import an external the
  // runtime provides, no Node server wrapper, no createRequire banner.
  const files = [path.join(dist, "ssr", "index.js")];
  const assets = path.join(dist, "ssr", "assets");
  if (fs.existsSync(assets))
    for (const f of fs.readdirSync(assets)) if (f.endsWith(".js")) files.push(path.join(assets, f));
  const bare = new Set();
  for (const file of files) {
    const code = fs.readFileSync(file, "utf8");
    for (const bad of ["node:http", "createRequire", "node:module", "server-bundle.mjs"]) {
      if (code.includes(bad)) throw new Error(`${path.relative(app, file)} contains ${JSON.stringify(bad)}`);
    }
    for (const m of code.matchAll(/(?:from|import)\s*["']([^"']+)["']/g)) {
      const spec = m[1];
      if (!spec.startsWith(".") && !spec.startsWith("/")) bare.add(spec);
    }
  }
  const foreign = [...bare].filter((s) => !s.startsWith("cloudflare:") && !s.startsWith("node:"));
  if (foreign.length) throw new Error(`worker bundle left non-runtime imports external: ${foreign.join(", ")}`);
  if (!bare.has("cloudflare:workers"))
    throw new Error(`worker bundle does not import cloudflare:workers (externals: ${[...bare].join(", ")})`);
  console.log(`start-cloudflare: layout ok (externals: ${[...bare].sort().join(", ")})`);
}

const get = async (route) => {
  const res = await fetch(`http://127.0.0.1:${PORT}${route}`);
  return { status: res.status, type: res.headers.get("content-type") || "", body: await res.text() };
};

async function runInWorkerd() {
  const bin = path.join(tmp, "node_modules", ".bin", "wrangler");
  let log = "";
  const srv = spawn(
    bin,
    [
      "dev",
      "--port",
      String(PORT),
      "--inspector-port",
      String(INSPECTOR_PORT),
      "--ip",
      "127.0.0.1",
      "--show-interactive-dev-session=false",
    ],
    {
      cwd: app,
      detached: true,
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1", NO_COLOR: "1", FORCE_COLOR: "0" },
    },
  );
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  const stop = () => {
    try {
      process.kill(-srv.pid, "SIGTERM");
    } catch {}
    setTimeout(() => {
      try {
        process.kill(-srv.pid, "SIGKILL");
      } catch {}
    }, 2000).unref();
  };
  try {
    let up = false;
    await settles(
      async () => {
        if (srv.exitCode != null) return true;
        try {
          up = (await fetch(`http://127.0.0.1:${PORT}/`)).status === 200;
        } catch {}
        return up;
      },
      { timeoutMs: 120000, pollMs: 500 },
    );
    if (!up) throw new Error(`wrangler dev did not serve on :${PORT}; log:\n${log.slice(-3000)}`);
    if (!/redirected Wrangler configuration/i.test(log) || !log.includes("dist/ssr/wrangler.json")) {
      throw new Error(`wrangler did not pick up the plugin's deploy redirect; log:\n${log.slice(0, 2000)}`);
    }

    const home = await get("/");
    if (home.status !== 200) throw new Error(`/ returned ${home.status}`);
    const h = home.body;
    const want = [
      ["#lib alias (shout)", "HOME!"],
      ["server function ran in the worker", "server-fn-marker"],
      ["wrangler var via getCloudflareContext().env", "fixture-edition"],
      ["tsconfig paths alias", "ALIAS!"],
      ["commonjs dep facade", "[INTEROP]"],
      ["commonjs subpath (extensionless)", "[deep:ok]"],
      ["plugin virtual module", "fixture-virtual-ok"],
      ["plugin load override + buildStart + this.environment.config.consumer", "FRESH_via_buildStart_ssr-server"],
      ["import.meta.glob", "Alpha Widget, Beta Widget"],
      ["?raw import", "raw-notes-marker"],
      ["svgr bare .svg component", "<rect"],
      ["svgr ?react component", "<polygon"],
      ["mdx module", "mdx-content-marker"],
      ["config define applied", "fixture-define-marker"],
      ["import.meta.env in a plain .js module", "jsenv:production:true"],
    ];
    for (const [what, marker] of want) {
      if (!h.includes(marker)) throw new Error(`workerd render: missing ${what} ("${marker}")\n${h.slice(0, 1500)}`);
    }
    if (!/<link[^>]*rel="stylesheet"[^>]*\.css/.test(h.slice(0, h.indexOf("</head>"))))
      throw new Error("stylesheet not linked in the SSR head");

    const about = await get("/about");
    if (about.status !== 200 || !about.body.includes("about-page-marker"))
      throw new Error(`/about did not render (${about.status})`);

    // Static assets come from the Worker's `assets` directory (the client build).
    const script = h.match(/<script[^>]*src="([^"]+\.js)"/)?.[1] ?? h.match(/href="(\/assets\/[^"]+\.js)"/)?.[1];
    if (!script) throw new Error("no client script in the SSR html");
    const js = await get(script);
    if (js.status !== 200 || !/javascript/.test(js.type))
      throw new Error(`client bundle ${script}: ${js.status} ${js.type}`);
    const pub = await get("/favicon.txt");
    if (pub.status !== 200 || !pub.body.includes("public-dir-marker"))
      throw new Error("publicDir file not served as a static asset");

    console.log(`start-cloudflare: workerd render ok (${want.length} features + /about + assets + publicDir)`);
  } finally {
    stop();
  }
}

try {
  installCloudflareDeps();
  makeApp();
  execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)}`, { cwd: app, stdio: "inherit" });
  assertLayout();
  await runInWorkerd();
  cleanup();
} catch (e) {
  console.error(e?.stack || e);
  if (keep) console.error(`kept ${tmp}`);
  else cleanup();
  process.exit(1);
}
