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

import { existsSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";

const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj:";
const warn = (msg) => process.stderr.write(`${OJ} ${msg}\n`);

// Runs on oj's embedded engine (`oj start-script`): the values that used to be
// spawn env arrive as `env`; `node <script>` still works for tests.
export async function run(env = null) {
  if (env) for (const [k, v] of Object.entries(env)) process.env[k] = v;
  if (env && env.OJ_APP_ROOT) {
    try { process.chdir(env.OJ_APP_ROOT); } catch {}
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
  const timeoutMs =
    (Number.parseInt(process.env.OJ_PRESEED_TIMEOUT ?? "", 10) > 0
      ? Number.parseInt(process.env.OJ_PRESEED_TIMEOUT, 10)
      : 300) * 1000;

  let vite;
  try {
    const require = createRequire(join(root, "package.json"));
    vite = await import(pathToFileURL(require.resolve("vite")).href);
  } catch (e) {
    warn(`deps pre-optimization: could not load the app's vite: ${e?.message ?? e}`);
    return report([], false);
  }
  if (typeof vite.resolveConfig !== "function" || typeof vite.DevEnvironment !== "function") {
    warn("deps pre-optimization: the app's vite has no environment API; skipping");
    return report([], false);
  }

  const rc = await vite.resolveConfig(
    { root, configFile: undefined, mode },
    "serve",
    "development",
    "development",
  );

  const seeded = [];
  let failed = false;
  for (const [name, envOpts] of Object.entries(rc.environments ?? {})) {
    // The client is served by oj itself; the in-host client environment never
    // transforms requests, so its optimizer never fires there. Only server
    // consumers can run an in-host pass.
    const consumer = envOpts?.consumer ?? (name === "client" ? "client" : "server");
    if (consumer === "client") continue;
    let de;
    try {
      // The base DevEnvironment, never the env's dev.createEnvironment factory
      // (that boots the plugin's runtime, e.g. workerd). The optimizer and its
      // hash inputs read only `name` and the resolved config, which the
      // subclass passes through to this same constructor unchanged.
      de = new vite.DevEnvironment(name, rc, { hot: false });
    } catch (e) {
      warn(`deps pre-optimization (${name}): ${e?.message ?? e}`);
      failed = true;
      continue;
    }
    // No depsOptimizer means Vite considers optimization disabled for this
    // environment (the default for server consumers without an include list):
    // the host will not run a pass either, nothing to seed.
    if (!de.depsOptimizer) continue;
    const metadataPath = join(
      de.config.cacheDir,
      name === "client" ? "deps" : `deps_${name}`,
      "_metadata.json",
    );
    try {
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
      if (existsSync(metadataPath)) {
        seeded.push({ name, metadataPath });
      } else {
        warn(`deps pre-optimization (${name}): the optimizer produced no metadata`);
        failed = true;
      }
    } catch (e) {
      warn(`deps pre-optimization (${name}): ${e?.message ?? e}`);
      failed = true;
    }
    // Only after the commit: depsOptimizer.close() CANCELS an in-flight
    // optimization. Under node this also releases the handles a natural exit
    // waits on; the engine child exits regardless.
    try { await de.close(); } catch {}
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
      process.env.OJ_PRESEED_REPORT ||
      join(dirname(fileURLToPath(import.meta.url)), "optimize-env-report.json");
    writeFileSync(dest, JSON.stringify({ seeded, failed }));
  } catch {}
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
