// SPDX-License-Identifier: MIT

// Two writers own src/routeTree.gen.ts under `oj dev`: oj's own regen
// (generate.mjs, and the codegen cache restoring its output) and the
// framework's tanstack:router-generator plugin, which the plugin host keeps
// active from the app's vite.config. Vite has only the second. When their
// bytes differ, the plugin rewrites oj's tree a moment later, and anything
// that snapshots the repo in between (a commit, `git status`) sees a dirty
// committed file. This polls the tree through boot, a route add, a warm
// restart (cache restore) and a start-entry delete, and requires every version
// ever on disk to carry the footer start-plugin-core itself builds for the
// app. Run with a built target/debug/oj (or OJ_BIN).
import { execFileSync, execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 6875;

const spc = path.join(fixture, "node_modules", "@tanstack", "start-plugin-core");
if (!fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start route tree writers: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oj-tree-writers-")));
const app = path.join(tmp, "app");
fs.mkdirSync(app);
for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json", "vite.config.ts"]) {
  fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
}
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
const startFile = path.join(app, "src", "start.ts");
fs.writeFileSync(
  startFile,
  'import { createStart } from "@tanstack/react-start";\nexport const startInstance = createStart(() => ({}));\n',
);
const tree = path.join(app, "src", "routeTree.gen.ts");
// A fresh checkout has no tree (it is gitignored), but earlier dev runs on the
// fixture leave one behind with whatever footer they wrote; copied along, the
// poller would count that stale file as a write.
fs.rmSync(tree, { force: true });

// The framework's own footer and start-entry lookup, from the installed
// package's internals (a moved file fails here loudly, which is the point).
// A fresh process each time: exsolve caches resolutions, deletes included.
const upstreamFooter = () =>
  execFileSync(
    process.execPath,
    [
      "--input-type=module",
      "-e",
      `const { buildRouteTreeFileFooterFromConfig } = await import(${JSON.stringify(
        path.join(spc, "dist/esm/start-router-plugin/route-tree-footer.js"),
      )});
      const { resolveStartEntryPlan } = await import(${JSON.stringify(path.join(spc, "dist/esm/planning.js"))});
      const plan = resolveStartEntryPlan({
        root: ${JSON.stringify(app)},
        startConfig: { srcDirectory: "src", start: {}, router: {}, client: {}, server: {} },
        defaultEntryPaths: {},
      });
      process.stdout.write(buildRouteTreeFileFooterFromConfig({
        generatedRouteTreePath: ${JSON.stringify(tree)},
        corePluginOpts: { framework: "react" },
        getConfig: () => ({ startConfig: { router: {} }, resolvedStartConfig: plan }),
      }).join("\\n"));`,
    ],
    { encoding: "utf8" },
  );

// Every distinct content the tree takes, as seen by a 10ms poller.
const versions = [];
let last;
const poller = setInterval(() => {
  let s = null;
  try {
    s = fs.readFileSync(tree, "utf8");
  } catch {}
  if (s !== last) {
    last = s;
    if (s !== null) versions.push(s);
  }
}, 10);

let log = "";
let srv;
const start = () => {
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"], detached: true });
  srv.stdout.on("data", (c) => (log += c));
  srv.stderr.on("data", (c) => (log += c));
  return waitUp(`http://localhost:${PORT}/`, { proc: srv });
};
const stop = async () => {
  if (!srv || srv.exitCode !== null) return;
  const exited = new Promise((r) => srv.once("exit", r));
  try {
    process.kill(-srv.pid, "SIGKILL");
  } catch {}
  await exited;
};

// Both writers have had their say once the tree holds `done` and has not
// changed for a while (the plugin's rewrite lands within a second here).
async function quiet(done, what) {
  let seen = versions.length;
  let since = Date.now();
  const ok = await settles(
    () => {
      if (versions.length !== seen) [seen, since] = [versions.length, Date.now()];
      return last != null && done(last) && Date.now() - since >= 3000;
    },
    { timeoutMs: 60000, pollMs: 50 },
  );
  if (!ok) throw new Error(`${what}: the tree never settled; log tail:\n${log.slice(-3000)}`);
}

function expectAll(from, footer, what) {
  const seen = versions.slice(from);
  if (seen.length === 0) throw new Error(`${what}: no route tree write observed`);
  const bad = seen.find((v) => !v.trimEnd().endsWith(footer));
  if (bad !== undefined) {
    throw new Error(
      `${what}: a written tree (of ${seen.length}) differs from the framework's footer\n` +
        `want tail:\n${footer}\ngot tail:\n${bad.trimEnd().split("\n").slice(-9).join("\n")}`,
    );
  }
}

let failed = false;
try {
  const withEntry = upstreamFooter();
  if (!withEntry.includes("startInstance")) throw new Error("the framework did not see src/start.ts");

  await start();
  await quiet(() => true, "cold boot");
  expectAll(0, withEntry, "cold boot");
  console.log(`tree writers: cold boot wrote ${versions.length} version(s), all with the framework's footer`);

  let mark = versions.length;
  const logMark = log.length;
  fs.writeFileSync(
    path.join(app, "src", "routes", "zzz.tsx"),
    'import { createFileRoute } from "@tanstack/react-router";\n' +
      'export const Route = createFileRoute("/zzz")({ component: () => null });\n',
  );
  // oj's own regen runs in the rebundle worker, which can still be busy with
  // the boot's rebundle when the plugin's rewrite lands; wait for it, or the
  // phase checks one writer only and the warm restart finds nothing cached.
  // Counted from the add on: a boot can log more than one generation, so a
  // total says nothing about this one, and stopping before it persists
  // leaves the warm boot a miss.
  const regenSinceAdd = () => /route tree cache miss[\s\S]*route tree generated/.test(log.slice(logMark));
  if (!(await settles(regenSinceAdd, { timeoutMs: 120000 }))) {
    throw new Error(`route add: oj never regenerated the tree; log tail:\n${log.slice(-3000)}`);
  }
  await quiet((s) => s.includes("/zzz"), "route add");
  expectAll(mark, withEntry, "route add");
  console.log("tree writers: a route add never put a different footer on disk");

  // A warm boot restores the tree from the codegen cache: the cached copy
  // has to be the framework's bytes too.
  await stop();
  mark = versions.length;
  fs.rmSync(tree);
  await start();
  await quiet((s) => s.includes("/zzz"), "warm restart");
  if (!/route tree restored from cache/.test(log))
    throw new Error(
      "the warm restart did not restore from cache:\n" +
        log
          .split("\n")
          .filter((l) => /route tree/.test(l))
          .join("\n"),
    );
  expectAll(mark, withEntry, "warm restart");
  console.log("tree writers: the warm restart restored the framework's bytes");

  // Deleting the entry restarts the server; from the first tree with the
  // entry-less footer on, nothing may write the old one back.
  fs.rmSync(startFile);
  const without = upstreamFooter();
  if (without.includes("startInstance")) throw new Error("the framework still sees src/start.ts");
  mark = versions.length;
  await quiet((s) => s.trimEnd().endsWith(without), "entry delete");
  const flipped = versions.findIndex((v, i) => i >= mark && v.trimEnd().endsWith(without));
  expectAll(flipped, without, "entry delete");
  console.log("tree writers: deleting src/start.ts switched to the createStart footer for good");
  console.log("START-ROUTE-TREE-WRITERS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-ROUTE-TREE-WRITERS E2E FAILED:", err.message);
} finally {
  clearInterval(poller);
  await stop();
  fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
