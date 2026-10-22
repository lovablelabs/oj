// SPDX-License-Identifier: MIT

// Pre-seeds Vite's per-environment deps caches (node_modules/.vite/deps_<env>)
// in a process of its own. A runner-backed config makes the plugin host build
// the app's real Vite DevEnvironments in-process, and a cold deps cache makes
// Vite run its dep optimizer there — a rolldown build whose binding retains
// native memory per build() at PROCESS scope (only process exit releases it;
// the client rebundle runs in one-shot children for the same reason). This
// script does that optimization here and dies; the in-host optimizer then
// loads a warm cache and never builds in-process.
//
// Byte-level cache compatibility is the contract: the app's OWN Vite resolves
// the config with the SAME arguments the plugin host uses (buildEnvironments
// in plugin-host.mjs), and Vite's own optimizer runs through a real
// DevEnvironment — the metadata's lockfileHash/configHash are computed by the
// same code that later validates them in the host, so no hash is imitated.
//
// Known gap, on purpose: this covers the BOOT-TIME pass. A mid-session
// re-optimization (a lockfile edit while serving, a discovery-mode
// environment finding new deps) still runs inside the host; intercepting
// every such path would mean owning Vite's optimizer scheduling.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  dropSnapshotEnvironments,
  foldIncludeSnapshot,
  priorDepIds,
  writeIncludeSnapshot,
} from "./discovered-deps.mjs";
import { appViteEntry, asNode } from "./vite-config-node.mjs";

const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj:";
const warn = (msg) => process.stderr.write(`${OJ} ${msg}\n`);

// Runs on oj's embedded engine (`oj start-script`): the values that used to be
// spawn env arrive as `env`; `node <script>` still works for tests.
export async function run(env = null) {
  if (env) for (const [k, v] of Object.entries(env)) process.env[k] = v;
  if (env && env.OJ_APP_ROOT) {
    try {
      process.chdir(env.OJ_APP_ROOT);
    } catch {}
  }
  return main();
}

async function main() {
  const root = process.env.OJ_APP_ROOT ?? process.cwd();
  // The exact mode string the plugin host passes to vite.resolveConfig in
  // buildEnvironments (the spawn payload's environment.mode): the resolved
  // config feeds Vite's configHash, so any divergence seeds a cache the
  // in-host optimizer would reject as stale.
  const mode = process.env.OJ_ENV_MODE || "dev";
  // Matches the spawner's kill bound (preseed.rs PRESEED_KILL): give up on a
  // metadata file that never lands before the parent reclaims this child.
  const timeoutMs = 300 * 1000;

  let vite;
  try {
    vite = await import(pathToFileURL(appViteEntry(root)).href);
  } catch (e) {
    warn(`deps pre-optimization: could not load the app's vite: ${e?.message ?? e}`);
    return report([], false);
  }
  if (typeof vite.resolveConfig !== "function" || typeof vite.DevEnvironment !== "function") {
    warn("deps pre-optimization: the app's vite has no environment API; skipping");
    return report([], false);
  }

  const rc = await asNode(() =>
    vite.resolveConfig({ root, configFile: undefined, mode }, "serve", "development", "development"),
  );
  // Snapshot BEFORE folding: the host folds the same file, and this child's
  // own optimize rewrites the metadata the snapshot was derived from.
  const snapshotPath = process.env.OJ_PRESEED_INCLUDE;
  if (snapshotPath) writeIncludeSnapshot(snapshotPath, priorDepIds(rc), rc.root);
  const folded = foldIncludeSnapshot(rc, snapshotPath);

  // One environment's pass: construct, init, wait for the committed metadata.
  // Returns the seeded entry, null for an environment with no optimizer to
  // satisfy, and throws what Vite threw.
  async function seedEnvironment(name) {
    let de;
    try {
      // The base DevEnvironment, never the env's dev.createEnvironment factory
      // (that boots the plugin's runtime, e.g. workerd). The optimizer and its
      // hash inputs read only `name` and the resolved config, which the
      // subclass passes through to this same constructor unchanged.
      de = new vite.DevEnvironment(name, rc, { hot: false });
      // No depsOptimizer means Vite considers optimization disabled for this
      // environment (the default for server consumers without an include list):
      // the host will not run a pass either, nothing to seed.
      if (!de.depsOptimizer) return null;
      const metadataPath = join(de.config.cacheDir, name === "client" ? "deps" : `deps_${name}`, "_metadata.json");
      await de.init();
      // Explicit optimizers (noDiscovery, the server default) run and COMMIT
      // inside init(); a discovery optimizer commits at crawl end, which with
      // no requests fires ~50ms after idle — wait for the metadata to land.
      await de.depsOptimizer.init();
      if (!de.depsOptimizer.options?.noDiscovery) {
        await de.waitForRequestsIdle();
        const deadline = Date.now() + timeoutMs;
        while (!existsSync(metadataPath) && Date.now() < deadline) {
          await new Promise((r) => setTimeout(r, 100));
        }
      }
      if (!existsSync(metadataPath)) throw new Error("the optimizer produced no metadata");
      return { name, metadataPath };
    } finally {
      // Only after the commit: depsOptimizer.close() CANCELS an in-flight
      // optimization. Under node this also releases the handles a natural exit
      // waits on; the engine child exits regardless.
      try {
        await de?.close();
      } catch {}
    }
  }

  const seeded = [];
  let failed = false;
  for (const [name, envOpts] of Object.entries(rc.environments ?? {})) {
    // The client is served by oj itself; the in-host client environment never
    // transforms requests, so its optimizer never fires there. Only server
    // consumers can run an in-host pass.
    const consumer = envOpts?.consumer ?? (name === "client" ? "client" : "server");
    if (consumer === "client") continue;
    let entry;
    try {
      entry = await seedEnvironment(name);
    } catch (e) {
      warn(`deps pre-optimization (${name}): ${e?.message ?? e}`);
      // Self-heal: Vite only ever optimizes ids its session discovered, so a
      // failing optimize with REPLAYED ids folded in blames them first — a dep
      // bump can leave a snapshot id the package's exports map now refuses,
      // which throws (an unresolvable id only warns) and would otherwise fail
      // every boot: the id ages out only through the very commit it prevents.
      // Drop the environment's replayed set from the include and the snapshot
      // (the host folds that same file strictly after this child exits) and
      // retry once as a discovery session, Vite's own cost for a dep change.
      const replayed = snapshotPath ? folded[name] : undefined;
      if (!replayed?.length) {
        failed = true;
        continue;
      }
      warn(
        `deps pre-optimization (${name}): dropping ${replayed.length} dep id(s) replayed from the last session and retrying without them`,
      );
      // A drop that cannot be WRITTEN fails the seed instead of retrying: the
      // host folds this same file, and a successful retry would stamp the
      // boot warm over a snapshot that still replays the toxic set — the
      // child (and this heal) would then be skipped on every later boot.
      if (!dropSnapshotEnvironments(snapshotPath, [name])) {
        warn(`deps pre-optimization (${name}): could not clear the replayed dep id(s) from the include snapshot`);
        failed = true;
        continue;
      }
      const optimizeDeps = rc.environments[name]?.optimizeDeps;
      if (optimizeDeps?.include) {
        optimizeDeps.include = optimizeDeps.include.filter((id) => !replayed.includes(id));
      }
      try {
        entry = await seedEnvironment(name);
      } catch (e2) {
        warn(`deps pre-optimization (${name}): ${e2?.message ?? e2}`);
        failed = true;
        continue;
      }
    }
    if (entry) seeded.push(entry);
  }
  if (seeded.length) {
    warn(`deps pre-optimized for environment(s): ${seeded.map((s) => s.name).join(", ")}`);
  }
  report(seeded, failed);
  if (failed) throw new Error("deps pre-optimization incomplete");
}

// What was seeded, for the parent's warm/cold stamp — at the path the parent
// asked for (OJ_PRESEED_REPORT), else next to this script (oj's cache root).
function report(seeded, failed) {
  try {
    const dest =
      process.env.OJ_PRESEED_REPORT || join(dirname(fileURLToPath(import.meta.url)), "optimize-env-report.json");
    writeFileSync(dest, JSON.stringify({ seeded, failed }));
  } catch {}
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
