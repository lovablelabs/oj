// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A TanStack Start build bundles a `?worker` entry the way Vite's
// bundleWorkerEntry does: with `worker.plugins` alone. The option is a
// function called per worker bundle; its plugins get their config and
// configResolved (`isWorker`, the bundle chain), then the whole bundle
// lifecycle: buildStart, resolveId/load, moduleParsed (lowered JS),
// generateBundle over the worker's output (what it emits ships with the
// client, a chunk it rewrites ships rewritten), closeBundle. The app's own
// plugins never see a worker's modules. oj ran the app plugins in the worker
// bundle without their lifecycle and ignored `worker.plugins`.
//
// The bundle also follows `worker.format` and `worker.rolldownOptions`. The
// default format is iife: the worker inlines its dynamic imports, starts as a
// classic script (no `type: "module"` in the `?worker` wrapper), and reads
// `import.meta.url` as `self.location.href` (vite:worker-post). With "es" it
// splits its dynamic imports into chunks of their own and starts as a module.
// oj always built ES and warned that the worker config was not applied.
// An import only an app plugin resolves fails inside a worker, as in Vite.

import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start-worker-plugins: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-worker-plugins-"));
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/[\\/](node_modules|\.oj-cache|dist)$/.test(src),
});
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
const report = path.join(app, "worker-seen.json");
const appSeen = path.join(app, "app-seen.json");

write("src/lib/wk/helper.ts", "export const n: number = 41;\n");
write("src/lib/wk/lazy.ts", 'export const lazy: string = "lazy-ok";\n');
write(
  "src/lib/wk/worker.ts",
  'import { n } from "./helper";\nimport { probe } from "virtual:worker-probe";\nconst m: number = n + 1;\n' +
    'self.onmessage = async () => { const { lazy } = await import("./lazy"); self.postMessage(`${probe}-${m}-${lazy}-${import.meta.url}`); };\n',
);
const aboutPath = path.join(app, "src/routes/about.tsx");
let about = fs.readFileSync(aboutPath, "utf8");
about = about
  .replace(
    'import { rootRoute } from "./__root";',
    'import { rootRoute } from "./__root";\nimport workerUrl from "../lib/wk/worker.ts?worker&url";\nimport WorkerCtor from "../lib/wk/worker.ts?worker";\n(globalThis as any).__ojWorker = WorkerCtor;',
  )
  .replace("about-page-marker</h1>", 'about-page-marker</h1>\n      <p id="wk">{workerUrl}</p>');
fs.writeFileSync(aboutPath, about);

const configPath = path.join(app, "vite.config.ts");
let config = fs.readFileSync(configPath, "utf8");
config = config.replace(
  "export default defineConfig({",
  `import { writeFileSync } from "node:fs";

let workerPluginCalls = 0;
function workerPlugins() {
  workerPluginCalls++;
  const seen: Record<string, string[]> = {};
  const add = (hook: string, value: string) => (seen[hook] ??= []).push(value);
  return [
    {
      name: "e2e-worker-collector",
      config(_config: any, env: any) {
        add("config", env.command);
      },
      configResolved(this: any, resolved: any) {
        add("configResolved", [resolved.isWorker, resolved.bundleChain?.some((f: string) => f.endsWith("/wk/worker.ts")), !!this.meta?.rolldownVersion].join(","));
      },
      buildStart() {
        add("buildStart", String(workerPluginCalls));
      },
      resolveId(id: string) {
        if (id === "virtual:worker-probe") return "\\0worker-probe";
      },
      load(id: string) {
        if (id === "\\0worker-probe") return 'export const probe = "worker-plugin-ok";';
      },
      moduleParsed(info: any) {
        const name = info.id.split("/").pop();
        if (/^(worker|helper)\\.ts$/.test(name)) add("moduleParsed", name + ":" + !info.code.includes(": number"));
      },
      generateBundle(_o: any, bundle: any) {
        for (const o of Object.values(bundle) as any[]) {
          if (o.type !== "chunk" || !o.isEntry) continue;
          add("generateBundle", Object.keys(o.modules).some((id) => id.endsWith("/wk/worker.ts")) ? "entry" : "other");
          o.code = "/* worker-plugin-banner */\\n" + o.code;
        }
        this.emitFile({ type: "asset", fileName: "worker-plugin.txt", source: "emitted" });
      },
      closeBundle() {
        add("closeBundle", "ran");
        writeFileSync(${JSON.stringify(report)}, JSON.stringify({ calls: workerPluginCalls, ...seen }));
      },
    },
  ];
}

// The app's plugins never see a worker's modules.
function appWatcher() {
  const seen: string[] = [];
  return {
    name: "e2e-app-watcher",
    apply: "build" as const,
    resolveId(id: string) {
      if (id === "virtual:worker-probe") seen.push("resolveId " + id);
    },
    moduleParsed(info: any) {
      // The \`?worker&url\` wrapper is the importer's own module, as in Vite.
      if (info.id.includes("/wk/") && !info.id.includes("?worker")) seen.push("moduleParsed " + info.id);
    },
    closeBundle() {
      writeFileSync(${JSON.stringify(appSeen)}, JSON.stringify(seen));
    },
  };
}

export default defineConfig({
  worker: {
    plugins: workerPlugins,
    format: (process.env.OJ_E2E_WORKER_FORMAT || undefined) as any,
    rolldownOptions: { output: { entryFileNames: "assets/wk-[name]-[hash].js" } },
  },`,
);
config = config.replace("plugins: [\n    buildInfoPlugin(),", "plugins: [\n    appWatcher(),\n    buildInfoPlugin(),");
fs.writeFileSync(configPath, config);

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const out = path.join(app, "dist");
const client = path.join(out, "client");
const buildWith = (format) => {
  try {
    return execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)} --out ${JSON.stringify(out)} 2>&1`, {
      cwd: repo,
      encoding: "utf8",
      env: { ...process.env, OJ_E2E_WORKER_FORMAT: format ?? "" },
    });
  } catch (e) {
    throw new Error(`oj build (worker format ${format ?? "default"}) failed:\n${e.stdout ?? ""}${e.stderr ?? ""}`);
  }
};
const clientCode = () =>
  fs
    .readdirSync(client, { recursive: true })
    .filter((f) => /\.js$/.test(f))
    .map((f) => fs.readFileSync(path.join(client, f), "utf8"))
    .join("\n");
const WRAPPER = /new Worker\([^,()]+wk-worker-[^,()]+,/;
const MODULE_WORKER = /new Worker\([^,()]+,\s*\{\s*type:\s*["'`]module["'`]/;

try {
  const log = buildWith(undefined);
  must(
    !log.includes("worker config is not applied"),
    `the Start build warned about the worker config it applies:\n${log}`,
  );
  must(fs.existsSync(report), "the worker plugins' closeBundle never ran");
  const seen = JSON.parse(fs.readFileSync(report, "utf8"));
  const want = {
    calls: 1,
    config: ["build"],
    configResolved: ["true,true,true"],
    buildStart: ["1"],
    moduleParsed: ["helper.ts:true", "worker.ts:true"],
    generateBundle: ["entry"],
    closeBundle: ["ran"],
  };
  const got = { ...seen, moduleParsed: [...(seen.moduleParsed ?? [])].sort() };
  must(
    JSON.stringify(got) === JSON.stringify(want),
    `worker.plugins hooks saw ${JSON.stringify(got)}, Vite's worker bundle gives ${JSON.stringify(want)}`,
  );

  const appHooks = JSON.parse(fs.readFileSync(appSeen, "utf8"));
  must(appHooks.length === 0, `the app's plugins saw the worker's modules: ${JSON.stringify(appHooks)}`);

  must(
    fs.existsSync(path.join(client, "worker-plugin.txt")),
    "a worker plugin's emitted file is not in the client output",
  );
  let assets = fs.readdirSync(path.join(client, "assets"));
  // worker.rolldownOptions.output names the worker entry.
  let worker = assets.find((f) => /^wk-worker-.*\.js$/.test(f));
  must(worker, `no wk-worker-*.js entry (worker.rolldownOptions.output) in ${JSON.stringify(assets)}`);
  let code = fs.readFileSync(path.join(client, "assets", worker), "utf8");
  must(code.includes("worker-plugin-ok"), "the worker plugin's virtual module is not in the worker chunk");
  must(code.startsWith("/* worker-plugin-banner */"), "the worker plugin's generateBundle rewrite did not ship");
  // iife, Vite's default: one classic script.
  must(code.includes("lazy-ok"), "the iife worker did not inline its dynamic import");
  must(!assets.some((f) => /^lazy-.*\.js$/.test(f)), `the iife worker split a chunk: ${JSON.stringify(assets)}`);
  must(!/\bimport\.meta\b|\bimport\(/.test(code), "the iife worker kept ES-only syntax");
  must(code.includes("self.location.href"), "import.meta.url is not self.location.href in the iife worker");
  must(WRAPPER.test(clientCode()), "no ?worker wrapper in the client code");
  must(!MODULE_WORKER.test(clientCode()), "the ?worker wrapper starts an iife worker as a module");

  buildWith("es");
  assets = fs.readdirSync(path.join(client, "assets"));
  worker = assets.find((f) => /^wk-worker-.*\.js$/.test(f));
  must(worker, `no es worker entry in ${JSON.stringify(assets)}`);
  code = fs.readFileSync(path.join(client, "assets", worker), "utf8");
  const lazy = assets.find((f) => /^lazy-.*\.js$/.test(f));
  must(lazy, `the es worker did not split its dynamic import: ${JSON.stringify(assets)}`);
  must(code.includes(lazy), "the es worker entry does not load its chunk");
  must(code.includes("import.meta.url"), "the es worker lost import.meta.url");
  must(MODULE_WORKER.test(clientCode()), "the ?worker wrapper starts an es worker as a classic script");

  // As in Vite, an import only an app plugin resolves (the fixture's
  // `virtual:build-info`) does not resolve inside a worker: the build fails
  // naming it. The plugin belongs in `worker.plugins` too.
  const workerFile = path.join(app, "src/lib/wk/worker.ts");
  fs.writeFileSync(
    workerFile,
    'import { buildTag } from "virtual:build-info";\n' +
      fs.readFileSync(workerFile, "utf8") +
      "console.log(buildTag);\n",
  );
  let failure = null;
  try {
    buildWith(undefined);
  } catch (e) {
    failure = e.message;
  }
  must(failure, "a worker importing an app-plugin-only module built; Vite fails to resolve it");
  must(
    /failed to resolve import "virtual:build-info" from "[^"]*worker\.ts"/.test(failure),
    `the failure does not name the unresolved worker import:\n${failure}`,
  );
  console.log("start-worker-plugins: worker.plugins, worker.format and worker.rolldownOptions shape the worker bundle");
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
