// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start, two Vite contracts a real app hit together:
//
// 1. Worker import queries. `import url from "./w.ts?worker&url"` is the
//    worker's URL string and `?worker` a constructor wrapper, in every
//    environment (Vite's worker plugin load). The SSR host and the client
//    bundle only knew an exact `?raw`/`?url`/`?inline` suffix: SSR loaded the
//    worker as a plain module ("does not provide an export named 'default'")
//    and the client bundle failed (UNLOADABLE_DEPENDENCY). Dev serves the
//    worker from the dev pipeline; a build bundles it on its own and emits it.
// 2. Post plugins see JS. Vite strips TS/JSX (vite:oxc) before normal and post
//    plugins, so an enforce:"post" plugin can `this.parse` every module; oj
//    used to hand it raw TypeScript.

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
const PORT = Number(process.env.OJ_E2E_PORT || 3107);

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start ssr worker url: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-worker-"));
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/[\\/](node_modules|\.oj-cache)$/.test(src),
});
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");

const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write("src/lib/wk/helper.ts", "export const n: number = 41;\n");
write(
  "src/lib/wk/worker.ts",
  'import { n } from "./helper";\nconst m: number = n + 1;\nself.onmessage = () => self.postMessage(m);\n',
);
// App plugins' resolveId in Vite's order around the core resolver: an
// enforce:"pre" hook (filtered) claims a specifier no package provides; an
// unfiltered normal hook answers one the core resolver cannot; a normal hook
// never overrides what the core resolver resolves (`./helper`).
write("src/lib/probe.css", ".probe { color: red; }\n");
write("src/lib/sub/sub.css", ".sub { color: blue; }\n");
write("src/lib/sub/mod.ts", 'export const sub: string = "subpath-ok";\n');
// Subpath imports (`#probe/*`), claimed by a pre plugin shaped like
// @cloudflare/vite-plugin's additional-modules rule: filter `^#`, and a
// resolveId that returns its own this.resolve.
{
  const pkgPath = path.join(app, "package.json");
  const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8"));
  pkg.imports = { ...(pkg.imports ?? {}), "#probe/*": "./src/lib/sub/*" };
  fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2));
}
write("src/lib/redirected.ts", 'export const redirected: string = "redirected-ok";\n');
// A real (tiny) PNG: `?url&no-inline` must resolve as `?url` through the SSR
// loader AND the client bundle (Vite's combinable grammar: noInlineRE only
// suppresses inlining, never the URL).
fs.writeFileSync(
  path.join(app, "src/lib/hero.png"),
  Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==", "base64"),
);
write(
  "src/lib/resolve-probe.ts",
  [
    'import { redirected } from "@probe/redirect";',
    'import fallback from "probe-fallback";',
    // An UNFILTERED enforce:"pre" resolveId: the bare-specifier fallback must
    // reach it in the bundle exactly as the SSR dev server's gate does.
    'import barePre from "bare-pre-probe";',
    'import { n } from "./wk/helper";',
    // With an unfiltered normal resolveId every import is routed through the
    // core resolver first; an asset query must still reach oj's asset plugin.
    'import cssUrl from "./probe.css?url";',
    'import subCssUrl from "#probe/sub.css?url";',
    'import "#probe/sub.css";',
    'import { sub } from "#probe/mod";',
    // `?url&inline`: Vite inlines (inline beats url), on SSR and client alike.
    'import heroInline from "../hero.png?url&inline";',
    // `?url&no-inline`: resolves as ?url (no-inline only suppresses inlining).
    'import heroUrl from "./hero.png?url&no-inline";',
    "export const resolved: string = `${redirected}|${fallback}|${n}|${typeof cssUrl}|${typeof subCssUrl}|${sub}|${heroInline.slice(0, 5)}|${barePre}`;",
    "export const hero: string = heroUrl;",
    "",
  ].join("\n"),
);
write("src/lib/wk/inline-worker.ts", 'import { n } from "./helper";\nself.onmessage = () => self.postMessage(`inline-${n}`);\n');
write(
  "src/lib/wk/index.ts",
  [
    'import workerUrl from "./worker.ts?worker&url";',
    'import WorkerCtor from "./worker.ts?worker";',
    'import InlineCtor from "./inline-worker.ts?worker&inline";',
    "export const wurl: string = workerUrl;",
    "export const wtype: string = typeof WorkerCtor + typeof InlineCtor;",
    "export { WorkerCtor, InlineCtor };",
    "",
  ].join("\n"),
);
const aboutPath = path.join(app, "src/routes/about.tsx");
let about = fs.readFileSync(aboutPath, "utf8");
about = about.replace(
  'import { rootRoute } from "./__root";',
  'import { rootRoute } from "./__root";\nimport { useEffect, useState } from "react";\nimport { wurl, wtype, WorkerCtor, InlineCtor } from "../lib/wk";\nimport { resolved, hero } from "../lib/resolve-probe";\n' +
    // After hydration, start all three workers and render their replies.
    "function WorkerRun() {\n" +
    "  const [out, setOut] = useState<string[]>([]);\n" +
    "  useEffect(() => {\n" +
    '    const ws = [new WorkerCtor(), new InlineCtor(), new Worker(wurl, { type: "module" })];\n' +
    "    for (const w of ws) {\n" +
    "      w.onmessage = (e) => setOut((o) => [...o, String(e.data)].sort());\n" +
    "      w.postMessage(0);\n" +
    "    }\n" +
    "    return () => ws.forEach((w) => w.terminate());\n" +
    "  }, []);\n" +
    '  return <p id="wkrun">{out.join(",")}</p>;\n' +
    "}",
);
about = about.replace(
  '<h1 className="fixture-heading">about-page-marker</h1>',
  '<h1 className="fixture-heading">about-page-marker</h1>\n      <p id="wk">{`${wurl}|${wtype}`}</p>\n      <p id="rid">{resolved}</p>\n      <p id="hero">{hero}</p>\n      <WorkerRun />',
);
fs.writeFileSync(aboutPath, about);

// An AST-reading post plugin, the shape that failed: it logs every module it
// cannot this.parse.
const parseLog = path.join(app, "parse-failures.log");
const configPath = path.join(app, "vite.config.ts");
let config = fs.readFileSync(configPath, "utf8");
const redirected = JSON.stringify(path.join(app, "src/lib/redirected.ts"));
// Each container's lifecycle, one plugin instance per container (the config is
// evaluated per environment): a worker bundle must not re-run it mid-build.
const lifecycleLog = path.join(app, "lifecycle.log");
config = config.replace(
  "plugins: [",
  `plugins: [(() => {
    let env = "?";
    const log = (ev) => require("node:fs").appendFileSync(${JSON.stringify(lifecycleLog)}, env + " " + ev + "\\n");
    return {
      name: "e2e-lifecycle",
      applyToEnvironment(e) { env = e.name; return true; },
      buildStart() { log("buildStart"); },
      buildEnd() { log("buildEnd"); },
      generateBundle() { log("generateBundle"); },
    };
  })(), {
    name: "e2e-pre-subpath",
    enforce: "pre",
    resolveId: { filter: { id: [/^#/] }, async handler(source, importer, options) { return await this.resolve(source, importer, options); } },
  }, {
    name: "e2e-pre-redirect",
    enforce: "pre",
    resolveId: { filter: { id: /^@probe\\/redirect$/ }, handler() { return ${redirected}; } },
  }, {
    name: "e2e-pre-unfiltered",
    enforce: "pre",
    resolveId(id) { if (id === "bare-pre-probe") return "\\0bare-pre-probe"; },
    load(id) { if (id === "\\0bare-pre-probe") return 'export default "pre-bare-ok";'; },
  }, {
    name: "e2e-post-fallback",
    resolveId(id) { if (id === "probe-fallback") return "\\0probe-fallback"; },
    load(id) { if (id === "\\0probe-fallback") return 'export default "fallback-ok";'; },
  }, {
    name: "e2e-post-hijack",
    resolveId(id) { if (id === "./wk/helper") return "\\0hijacked"; },
    load(id) { if (id === "\\0hijacked") return 'export const n = "hijacked";'; },
  }, {
    name: "e2e-post-parse",
    enforce: "post",
    applyToEnvironment: (environment) => environment.name === "ssr",
    transform(code, id) {
      if (id.startsWith("\\0") || id.includes("node_modules") || !/\\.(tsx?|jsx)$/.test(id.split("?")[0])) return null;
      try { this.parse(code); } catch (e) { require("node:fs").appendFileSync(${JSON.stringify(parseLog)}, id + "\\n"); }
      return null;
    },
  }, `,
);
config = `import { createRequire } from "node:module";\nconst require = createRequire(import.meta.url);\n${config}`;
fs.writeFileSync(configPath, config);

// Run `cmd` detached, wait for the port, run `check`, then group-kill and
// await the exit so no server outlives the step.
async function served(cmd, args, opts, check) {
  const srv = spawn(cmd, args, { stdio: ["ignore", "ignore", "pipe"], detached: true, ...opts });
  let stderr = "";
  srv.stderr.on("data", (d) => (stderr += d));
  const exited = new Promise((r) => srv.once("exit", r));
  try {
    const up = await waitUp(`http://localhost:${PORT}/`, { until: () => true }).then(() => true, () => false);
    must(up, `server on :${PORT} did not start\n${stderr.slice(-2000)}`);
    await check(() => stderr);
  } finally {
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
    await exited;
  }
}

// Load /about in Chromium and wait for all three workers to answer: the
// ?worker constructor, the ?worker&inline one (a Blob URL in a build) and
// `new Worker(?worker&url)`.
async function workersRunInBrowser(label) {
  let pw = null;
  try {
    pw = await import("playwright");
  } catch {}
  if (!pw) {
    console.log(`${label}: SKIP browser worker run (playwright not installed)`);
    return;
  }
  const browser = await pw.chromium.launch();
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    await page.goto(`http://localhost:${PORT}/about`);
    const hydrated = await page.locator("#rid").textContent();
    must(hydrated === RESOLVED, `${label}: the hydrated page resolved differently: ${JSON.stringify(hydrated)} (errors: ${errors.join(" | ")})`);
    const want = "42,42,inline-41";
    const got = await page
      .waitForFunction((w) => document.querySelector("#wkrun")?.textContent === w, want, { timeout: 20000 })
      .then(() => want)
      .catch(async () => page.locator("#wkrun").textContent().catch(() => null));
    must(got === want, `${label}: workers answered ${JSON.stringify(got)}, want ${want}; page errors: ${errors.join(" | ")}`);
    console.log(`${label}: ?worker, ?worker&inline and ?worker&url all run in the browser`);
  } finally {
    await browser.close();
  }
}

const rendered = async () => {
  const res = await fetch(`http://localhost:${PORT}/about`);
  const html = await res.text();
  return {
    status: res.status,
    shown: html.match(/<p id="wk">([^<]*)<\/p>/)?.[1],
    rid: html.match(/<p id="rid">([^<]*)<\/p>/)?.[1],
    hero: html.match(/<p id="hero">([^<]*)<\/p>/)?.[1],
  };
};
const RESOLVED = "redirected-ok|fallback-ok|41|string|string|subpath-ok|data:|pre-bare-ok";
// The client bundle resolved like the server: both plugin answers are in it,
// the hijack of a core-resolvable import is not.
const clientResolvedLikeVite = (js, label) => {
  must(js.includes("redirected-ok"), `${label}: the client bundle missed the enforce:pre resolveId redirect`);
  must(js.includes("fallback-ok"), `${label}: the client bundle missed the post-core resolveId fallback`);
  must(js.includes("pre-bare-ok"), `${label}: an UNFILTERED enforce:pre resolveId never saw a bare specifier`);
  must(!js.includes("hijacked"), `${label}: a normal resolveId overrode an import the core resolver resolves`);
};

try {
  await served(oj, ["dev", app, "--port", String(PORT)], {}, async (stderr) => {

    const { status, shown } = await rendered();
    must(status === 200, `dev /about returned ${status}\n${stderr().slice(-2000)}`);
    must(shown === "/src/lib/wk/worker.ts|functionfunction", `dev SSR worker imports rendered ${JSON.stringify(shown)}`);
    const worker = await fetch(`http://localhost:${PORT}/src/lib/wk/worker.ts`);
    must(worker.status === 200, `the worker URL the SSR render emitted returned ${worker.status}`);
    must(!(await worker.text()).includes(": number"), "the worker URL serves uncompiled TypeScript");
    // The client bundle (which failed to load the query) carries the same URL,
    // so the rendered value hydrates unchanged.
    const cache = path.join(app, ".oj-cache");
    const versioned = fs.readdirSync(cache).find((d) => /^v\d+$/.test(d));
    const index = JSON.parse(fs.readFileSync(path.join(cache, versioned, "start", "client-chunks.json"), "utf8"));
    let js = "";
    for (const f of index.files) if (f.name.endsWith(".js")) js += await (await fetch(`http://localhost:${PORT}/@oj-start/${f.name}`)).text();
    must(js.includes('"/src/lib/wk/worker.ts"'), "the dev client bundle does not carry the worker URL");
    must(/new Worker\(/.test(js), "the dev client bundle has no ?worker constructor");
    clientResolvedLikeVite(js, "start-dev");
    const devPage = await rendered();
    must(devPage.rid === RESOLVED, "start-dev: SSR resolved the plugin specifiers differently");
    // oj's dev asset URLs are fsBase-shaped (/@oj-start/fs<abs>), the same
    // value the client bundle renders; the fix's contract is that the import
    // resolves as ?url (a served URL) instead of loading the PNG as a module.
    must((devPage.hero ?? "").endsWith("/src/lib/hero.png") && devPage.hero.startsWith("/"), `start-dev: ?url&no-inline rendered ${JSON.stringify(devPage.hero)}, want a served asset URL`);
    const heroRes = await fetch(`http://localhost:${PORT}${devPage.hero}`);
    must(heroRes.status === 200, `the ?url&no-inline URL returned ${heroRes.status}`);
    console.log("start-dev: app resolveId runs in Vite's order in the client bundle (pre before core, normal only after it)");
    console.log("start-dev: ?worker&url is the worker's dev URL, ?worker a constructor, on SSR and in the client bundle");

    await workersRunInBrowser("start-dev");

    const failures = fs.existsSync(parseLog) ? fs.readFileSync(parseLog, "utf8").trim() : "";
    must(failures === "", `a post plugin could not this.parse:\n${failures}`);
    console.log("start-dev ssr: an enforce:post plugin parses every TS/TSX module (TS/JSX stripped at vite:oxc's slot)");
  });

  // A build bundles the worker entry on its own and emits it once; the client
  // and the server render share the emitted URL.
  const out = path.join(app, "dist-e2e");
  fs.rmSync(lifecycleLog, { force: true });
  execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)} --out ${JSON.stringify(out)}`, { stdio: ["ignore", "ignore", "inherit"] });
  const assets = path.join(out, "client", "assets");
  const workerFile = fs.readdirSync(assets).find((f) => /^worker-[\w-]+\.js$/.test(f));
  must(workerFile, `the build emitted no worker asset: ${fs.readdirSync(assets).join(", ")}`);
  const workerCode = fs.readFileSync(path.join(assets, workerFile), "utf8");
  // The helper's 41 is inlined (a minifier folds `41 + 1` to 42).
  must(!/\bimport\b/.test(workerCode) && /\b4[12]\b/.test(workerCode), `the emitted worker is not one bundled file with its helper inlined:\n${workerCode}`);
  must(!workerCode.includes(": number"), "the emitted worker is uncompiled TypeScript");
  await served("node", [path.join(out, "server.mjs")], { cwd: out, env: { ...process.env, PORT: String(PORT) } }, async (stderr) => {
    const { status, shown } = await rendered();
    must(status === 200, `built /about returned ${status}\n${stderr().slice(-2000)}`);
    must(shown === `/assets/${workerFile}|functionfunction`, `built SSR worker imports rendered ${JSON.stringify(shown)}`);
    const { hero } = await rendered();
    must(/^\/assets\/hero-[\w-]+\.png$/.test(hero ?? ""), `built ?url&no-inline rendered ${JSON.stringify(hero)}, want an emitted asset URL`);
    must(fs.existsSync(path.join(out, "client", hero)), "the no-inline asset the render references was not emitted");
    const served = await fetch(`http://localhost:${PORT}/assets/${workerFile}`);
    must(served.status === 200, `the emitted worker URL returned ${served.status}`);
    await workersRunInBrowser("start build");
  });
  const clientJs = fs.readdirSync(assets).filter((f) => f.endsWith(".js") && f !== workerFile).map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("");
  must(clientJs.includes(`/assets/${workerFile}`), "the client build does not reference the emitted worker");
  clientResolvedLikeVite(clientJs, "start build");
  // The worker bundles run while the client build loads modules; the client
  // container's own lifecycle must stay one buildStart and one buildEnd
  // before its generateBundle (Vite's worker build never runs the importer's).
  const events = fs.readFileSync(lifecycleLog, "utf8").trim().split("\n");
  const clientBefore = events.slice(0, events.indexOf("client generateBundle"));
  must(events.includes("client generateBundle"), `no client generateBundle in the lifecycle log:\n${events.join("\n")}`);
  const count = (ev) => clientBefore.filter((l) => l === `client ${ev}`).length;
  must(count("buildStart") === 1 && count("buildEnd") === 1, `a worker bundle re-ran the client container's lifecycle:\n${events.join("\n")}`);
  console.log("start build: worker bundles leave the client container's lifecycle alone");
  console.log("start build: the worker is bundled once and emitted; client and server render the same URL");
  // ?worker&inline: the bundled worker ships inside the client as a string
  // (its helper inlined) and starts from a Blob URL, as Vite's build does;
  // no separate file is emitted for it.
  must(fs.readdirSync(assets).every((f) => !f.startsWith("inline-worker")), "?worker&inline emitted a separate worker file");
  // The bundled worker source is a string literal in the client (the
  // minifier renames the `jsContent` binding, so match the content).
  const inlineSource = clientJs.match(/"[^"]*onmessage[^"]*"/g)?.find((t) => t.includes("inline-"));
  must(inlineSource, "the client build has no inlined worker source");
  must(inlineSource.includes("inline-41") && !/\bimport\b/.test(inlineSource), `the inlined worker is not the bundled entry:\n${inlineSource}`);
  must(clientJs.includes("URL.revokeObjectURL(import.meta.url)") && clientJs.includes("createObjectURL"), "the inline worker does not start from a Blob URL");
  console.log("start build: ?worker&inline ships the bundled worker inline and starts it from a Blob URL");
  console.log("\nSTART SSR WORKER URL PASSED");
} catch (e) {
  console.error("\nSTART SSR WORKER URL FAILED:", e.message);
  process.exitCode = 1;
} finally {
  if (!process.env.OJ_E2E_KEEP) fs.rmSync(app, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  else console.log("kept", app);
}
