// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The app's chunk policy reaches the Start client bundle: Vite applies no
// chunk grouping of its own and passes `build.rolldownOptions` through to
// rolldown in build and bundled dev alike, so an app's
// `output.codeSplitting` groups (vendor chunks, i18n buckets) must shape
// oj's dev bundle and prod client build the same way. The override config
// groups react into a named vendor chunk; dev must emit and serve it, the
// prod client build must emit it under assets/.
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 6855;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const installed = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start-chunk-policy: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-chunk-policy-"));
const app = path.join(tmp, "app");
fs.mkdirSync(app);
for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json"]) {
  fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
}
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
// The fixture's config plus the app's chunk policy, the lovable/web shape:
// a test-matched vendor group and a function-named group (both forms Vite
// passes through live, since the config loads in-process).
fs.writeFileSync(
  path.join(app, "vite.config.ts"),
  fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8").replace(
    "export default defineConfig({",
    `const groups: any[] = [
  { name: "vendor-react", test: /node_modules[\\\\/](?:react|react-dom|scheduler)[\\\\/]/, priority: 10 },
  { name: (id: string) => (id.includes("/@tanstack/") ? "vendor-tanstack" : undefined), priority: 5 },
  // Sweep off: rolldown requires preserveEntrySignatures false or
  // allow-extension for it and warns INVALID_OPTION when the option is unset;
  // Vite's client build sets false.
  { name: "vendor-scheduler", test: /node_modules[\\\\/]scheduler[\\\\/]/, includeDependenciesRecursively: false, priority: 20 },
  // A function test and a function name that walks the graph, after a
  // function-named group already ran: getModuleInfo answers every module of
  // the pass, importers included, as it does under Vite.
  {
    test: (id: string) => /[\\\\/]src[\\\\/]routes[\\\\/]/.test(id),
    name: (id: string, { getModuleInfo }: any) => {
      const info = getModuleInfo(id);
      if (!info) throw new Error(\`getModuleInfo(\${id}) is null\`);
      for (const importer of info.importers) {
        if (!getModuleInfo(importer)) throw new Error(\`getModuleInfo(\${importer}), an importer of \${id}, is null\`);
      }
      if (info.importers.length && !(globalThis as any).__walked) {
        (globalThis as any).__walked = true;
        console.error("chunk-policy-walk: getModuleInfo answered a route and its importers");
      }
      return "app-routes";
    },
    priority: 1,
  },
];
// The route-shared shape: a group computed by an apply-build plugin, whose
// guard throws when that plugin did not run. Dev must step around it;
// a production build must fail on it (CHUNK_POLICY_THROW gates it in so the
// passing prod leg can drop it).
if (process.env.CHUNK_POLICY_THROW) {
  groups.push({
    name: () => {
      throw new Error("group route-shared: its plugin did not run before chunking");
    },
    priority: 100,
  });
}
export default defineConfig({
  build: { rolldownOptions: { output: { codeSplitting: { groups } } } },`,
  ),
);

const cache = path.join(app, ".oj-cache");
const vendorChunks = () => {
  const idx = JSON.parse(fs.readFileSync(path.join(cache, "v1", "start", "client-chunks.json"), "utf8"));
  return {
    react: idx.files.find((f) => /^vendor-react-.*\.js$/.test(f.name)),
    tanstack: idx.files.find((f) => /^vendor-tanstack-.*\.js$/.test(f.name)),
    scheduler: idx.files.find((f) => /^vendor-scheduler-.*\.js$/.test(f.name)),
  };
};

let failed = false;
let log = "";
// Dev runs WITH the throwing group: a group whose guard assumes an
// apply-build plugin must step aside under dev (one warning), never crash
// the server, and the other groups still apply. Vite's own bundled dev
// passes user output straight through and would crash here; oj's dev
// bundle is the production path, so it degrades instead.
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
  stdio: ["ignore", "pipe", "pipe"],
  env: { ...process.env, CHUNK_POLICY_THROW: "1" },
});
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  const home = await fetch(`http://localhost:${PORT}/`);
  if (home.status !== 200) throw new Error(`/ returned ${home.status}`);
  const chunks = vendorChunks();
  if (!chunks.react) throw new Error("dev bundle has no vendor-react chunk (codeSplitting not honored)");
  if (!chunks.tanstack) throw new Error("dev bundle has no vendor-tanstack chunk (function-named group lost)");
  if (!chunks.scheduler) throw new Error("dev bundle has no vendor-scheduler chunk (non-recursive group lost)");
  // The walk's group stepping aside under dev would hide a null answer.
  if (log.includes("getModuleInfo(")) throw new Error(`a group's getModuleInfo walk came back null in dev:\n${log}`);
  if (!log.includes("chunk-policy-walk:")) throw new Error("the getModuleInfo walk group never ran in dev");
  if (log.includes("INVALID_OPTION")) {
    throw new Error("dev bundle warned INVALID_OPTION (preserveEntrySignatures left unset, Vite sets false)");
  }
  const served = await fetch(`http://localhost:${PORT}/@oj-start/${chunks.react.name}`);
  if (served.status !== 200) throw new Error(`vendor-react chunk not served: ${served.status}`);
  if (!(await served.text()).includes("react")) throw new Error("vendor-react chunk carries no react");
  if (!/codeSplitting group .* threw under dev and steps aside/.test(log)) {
    throw new Error("the throwing group was not named in the dev log");
  }
  console.log("chunk-policy: dev bundle grouped and served the app's vendor chunks");
  console.log("chunk-policy: a group whose build-only plugin never ran stepped aside in dev");

  srv.kill("SIGKILL");
  await sleep(300);

  // A production build with the throwing group fails loudly, as vite build
  // does: dev tolerance must never leak into the build.
  const outThrow = path.join(tmp, "out-throw");
  let prodFailed = false;
  try {
    execSync(`${oj} build ${app} --out ${outThrow}`, {
      cwd: repo,
      stdio: ["ignore", "ignore", "pipe"],
      env: { ...process.env, CHUNK_POLICY_THROW: "1" },
    });
  } catch (e) {
    prodFailed = true;
    const err = String(e?.stderr ?? "");
    if (!err.includes("its plugin did not run before chunking")) {
      throw new Error(`prod build failed without naming the group's error:\n${err.slice(-800)}`);
    }
  }
  if (!prodFailed) throw new Error("prod build swallowed the throwing group (dev tolerance leaked into build)");
  console.log("chunk-policy: prod build still fails loudly on a throwing group");

  const out = path.join(tmp, "out");
  const buildLog = execSync(`${oj} build ${app} --out ${out} 2>&1`, { cwd: repo, encoding: "utf8" });
  if (buildLog.includes("INVALID_OPTION")) {
    throw new Error(`prod client build warned INVALID_OPTION:\n${buildLog.slice(-800)}`);
  }
  const assets = fs.readdirSync(path.join(out, "client", "assets"));
  if (!assets.some((f) => /^vendor-scheduler-.*\.js$/.test(f))) {
    throw new Error("prod client build has no vendor-scheduler chunk (non-recursive group lost)");
  }
  if (!assets.some((f) => /^vendor-react-.*\.js$/.test(f))) {
    throw new Error("prod client build has no vendor-react chunk (codeSplitting not honored)");
  }
  if (!assets.some((f) => /^vendor-tanstack-.*\.js$/.test(f))) {
    throw new Error("prod client build has no vendor-tanstack chunk (function-named group lost)");
  }
  if (!buildLog.includes("chunk-policy-walk:")) {
    throw new Error(`the getModuleInfo walk group never ran in the prod build:\n${buildLog.slice(-800)}`);
  }
  console.log("chunk-policy: prod client build grouped the app's vendor chunks");
  console.log("START-CHUNK-POLICY E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-CHUNK-POLICY E2E FAILED:", err.message);
  console.error(log.slice(-1500));
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  for (let i = 0; ; i++) {
    try {
      fs.rmSync(tmp, { recursive: true, force: true });
      break;
    } catch {
      if (i >= 20) break;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
}
process.exit(failed ? 1 : 0);
