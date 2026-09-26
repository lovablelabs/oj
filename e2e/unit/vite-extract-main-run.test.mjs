// SPDX-License-Identifier: MIT

// End to end through the extractor's exported `extract()` — the function oj
// calls on its in-process JS engine. Each case runs in a fresh node child
// (the engine boots a fresh isolate per extraction; a child is the node-test
// equivalent, and it isolates the config's module cache, env and any timers
// a hook leaves behind). The wrapper prints the returned result on stdout and
// the `__stderr` transcript on stderr, so assertions read like the old
// subprocess protocol.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { asset, repo, tmpProject } from "./harness.mjs";

// extract() wraps process.stdout/stderr.write and console (its output
// capture), so the wrapper saves raw write fds first. writeSync keeps the
// result whole across the explicit exit — which itself asserts that a config
// hook's leftover interval cannot hold the extraction open.
const EXTRACT_WRAPPER = `
import { writeSync } from "node:fs";
import { pathToFileURL } from "node:url";
const [script, vite, root, command = "serve", mode = "development", modeKind = "explicit"] = process.argv.slice(1);
const { extract } = await import(pathToFileURL(script).href);
const out = await extract({ vite, root, command, mode, modeKind });
const { __stderr = "", ...rest } = out;
if (__stderr) writeSync(2, __stderr);
writeSync(1, JSON.stringify(rest));
process.exit(0);
`;

// spawnSync shape ({ stdout, stderr }) for tests that assert on the transcript.
function spawnExtractor(base, config, args = [], opts = {}) {
  return spawnSync(
    process.execPath,
    ["--input-type=module", "-e", EXTRACT_WRAPPER, join(base, "vite-extract.mjs"), join(base, config), base, ...args],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout: 30_000, ...opts },
  );
}

// End to end through the extractor entry: a config whose plugin declares a
// dev-runtime environment from its `config` hook (Vite's declaration
// mechanism, as @cloudflare/vite-plugin does) marks the ssr environment
// runner-backed (`ssr.runnerBacked`), and the RAW top-level `resolve` block
// travels as `rawResolve` — the two inputs the Node SSR consumers select
// conditions from.
test("the extractor emits ssr.runnerBacked and rawResolve", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-rb-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "rb-app", type: "module" }));
    // A raw plugin list nests, as a plugin factory's return does.
    writeFileSync(
      join(base, "vite.config.mjs"),
      `export default {
        plugins: [[
          { name: "vite-plugin-cloudflare" },
          { name: "vite-plugin-cloudflare:config",
            config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }) },
        ]],
        resolve: { conditions: ["custom"], externalConditions: ["custom-ext"] },
        ssr: { target: "node" },
      };\n`,
    );
    const run = (config) => JSON.parse(spawnExtractor(base, config).stdout);
    const out = run("vite.config.mjs");
    assert.equal(out.__ok, true);
    assert.equal(out.ssr.runnerBacked, true);
    assert.equal(out.ssr.target, "node");
    assert.deepEqual(out.rawResolve, { conditions: ["custom"], externalConditions: ["custom-ext"] });

    // Without the plugin (and no custom createEnvironment) nothing is marked.
    writeFileSync(
      join(base, "plain.config.mjs"),
      'export default { plugins: [{ name: "react" }], ssr: { target: "node" } };\n',
    );
    const plain = run("plain.config.mjs");
    assert.equal(plain.__ok, true);
    assert.equal(plain.ssr.runnerBacked, undefined);
    assert.equal(plain.rawResolve, null);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

function extractorDir(prefix) {
  const base = mkdtempSync(join(tmpdir(), prefix));
  copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
  writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module", dependencies: { vite: "*" } }));
  return base;
}

function runExtractor(base, config, args = [], opts = {}) {
  return execFileSync(
    process.execPath,
    ["--input-type=module", "-e", EXTRACT_WRAPPER, join(base, "vite-extract.mjs"), join(base, config), base, ...args],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout: 30_000, ...opts },
  );
}

// A config/configEnvironment hook is real plugin code: one that starts an
// interval (a watcher, a server) must not keep the extraction from resolving
// — extract() returns the result while the timer is still live (oj then drops
// the engine, killing it, as the old one-shot subprocess's exit did).
test("extract() resolves even when a hook keeps the event loop alive", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-exit-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module" }));
    writeFileSync(
      join(base, "vite.config.mjs"),
      `export default {
        plugins: [{
          name: "keeps-loop-alive",
          config: () => { setInterval(() => {}, 1000); return null; },
        }],
        base: "/app/",
      };\n`,
    );
    const out = JSON.parse(runExtractor(base, "vite.config.mjs", [], { timeout: 15_000 }));
    assert.equal(out.__ok, true);
    assert.equal(out.base, "/app/");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// Vite computes configEnv.isSsrBuild from the INLINE config before the file
// loads and never recomputes it, so a build.ssr set only by the config FILE is
// invisible to config hooks even under `vite build`. oj has no inline
// build.ssr at extraction time: hooks must see false for serve AND build.
test("config hooks see isSsrBuild false even when the file sets build.ssr under build", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-ssrbuild-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module" }));
    writeFileSync(
      join(base, "vite.config.mjs"),
      `import { writeFileSync } from "node:fs";
      export default {
        build: { ssr: "src/server.ts" },
        plugins: [{
          name: "env-probe",
          config: (conf, env) => { writeFileSync(new URL("./env.json", import.meta.url), JSON.stringify(env)); return null; },
        }],
      };\n`,
    );
    const out = JSON.parse(runExtractor(base, "vite.config.mjs", ["build", "production", "explicit"]));
    assert.equal(out.__ok, true);
    const env = JSON.parse(fs.readFileSync(join(base, "env.json"), "utf8"));
    assert.equal(env.command, "build");
    assert.equal(env.isSsrBuild, false, "Vite never recomputes isSsrBuild from the file's build.ssr");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// The single-eval flow through an installed vite: loadConfigFromFile runs
// first, its config goes back into resolveConfig via configFile:false, and the
// detection sentinel decides runner-backed inside resolveConfig's OWN hook run
// — the config file is evaluated once and no plugin's config hook runs twice.
test("with vite installed the config file is evaluated once and the sentinel decides runnerBacked", () => {
  const base = extractorDir("oj-vite-extract-single-");
  try {
    fs.mkdirSync(join(base, "node_modules", "vite"), { recursive: true });
    fs.writeFileSync(
      join(base, "node_modules", "vite", "package.json"),
      JSON.stringify({ name: "vite", version: "0.0.0-stub", type: "module", main: "index.mjs" }),
    );
    // A minimal vite honoring the contract the extractor relies on: config
    // hooks run hook-order-first inside resolveConfig, configEnvironment hooks
    // after, and the RESOLVED config default-fills every environment's
    // dev.createEnvironment (so reading the resolved config would always
    // false-positive — only the sentinel's pre-fill view is trustworthy).
    fs.writeFileSync(
      join(base, "node_modules", "vite", "index.mjs"),
      `import { pathToFileURL } from "node:url";
      export async function loadConfigFromFile(configEnv, configFile) {
        const m = await import(pathToFileURL(configFile).href);
        const config = typeof m.default === "function" ? await m.default(configEnv) : m.default;
        return { config, path: configFile, dependencies: [configFile] };
      }
      export function mergeConfig(a, b) { return { ...a, ...b }; }
      const merge = (a, b) => {
        const out = { ...a };
        for (const k of Object.keys(b ?? {})) {
          const v = b[k];
          if (v == null) continue;
          out[k] = out[k] && typeof out[k] === "object" && !Array.isArray(out[k]) && typeof v === "object" && !Array.isArray(v) ? merge(out[k], v) : v;
        }
        return out;
      };
      export async function resolveConfig(inline, command, mode) {
        if (inline.configFile !== false) throw new Error("stub expects the single-eval configFile:false flow");
        let conf = { ...inline };
        const plugins = (conf.plugins ?? []).flat(Infinity).filter(Boolean);
        const rank = (h) => (h && typeof h === "object" ? (h.order === "pre" ? -1 : h.order === "post" ? 1 : 0) : 0);
        const hooks = plugins.map((p) => ({ p, h: p.config })).filter((e) => e.h);
        hooks.sort((a, b) => rank(a.h) - rank(b.h));
        for (const { h } of hooks) {
          const res = await (typeof h === "object" ? h.handler : h).call({ error(m) { throw new Error(m); }, warn() {} }, conf, { command, mode, isSsrBuild: false, isPreview: false });
          if (res) conf = merge(conf, res);
        }
        conf.environments = { client: {}, ssr: {}, ...(conf.environments ?? {}) };
        for (const p of plugins) {
          if (!p.configEnvironment) continue;
          const h = typeof p.configEnvironment === "object" ? p.configEnvironment.handler : p.configEnvironment;
          for (const name of Object.keys(conf.environments)) {
            const r = await h.call({}, name, conf.environments[name], { command, mode });
            if (r) conf.environments[name] = merge(conf.environments[name], r);
          }
        }
        // The default factory fill: presence in the RESOLVED config says nothing.
        for (const name of Object.keys(conf.environments)) {
          conf.environments[name] = merge({ dev: { createEnvironment: () => ({}) } }, conf.environments[name]);
        }
        conf.configFileDependencies = [];
        return conf;
      }\n`,
    );
    // The config counts its evaluations and its hook runs on disk.
    fs.writeFileSync(
      join(base, "vite.config.mjs"),
      `import { appendFileSync } from "node:fs";
      appendFileSync(new URL("./evals.log", import.meta.url), "eval\\n");
      export default {
        base: "/one-eval/",
        plugins: [{
          name: "declares",
          config: () => {
            appendFileSync(new URL("./hooks.log", import.meta.url), "config\\n");
            return { environments: { worker: { dev: { createEnvironment: () => ({}) } } } };
          },
        }],
      };\n`,
    );
    const out = JSON.parse(runExtractor(base, "vite.config.mjs"));
    assert.equal(out.__ok, true);
    assert.equal(out.base, "/one-eval/");
    assert.equal(out.ssr?.runnerBacked, true, "the sentinel saw the declaration inside resolveConfig");
    assert.equal(fs.readFileSync(join(base, "evals.log"), "utf8"), "eval\n", "the config file is evaluated exactly once");
    assert.equal(fs.readFileSync(join(base, "hooks.log"), "utf8"), "config\n", "no plugin's config hook runs twice");

    // A non-declaring config through the same flow: the sentinel's false is
    // authoritative (no fallback re-run of the hooks on the same instances).
    fs.writeFileSync(
      join(base, "plain.config.mjs"),
      `import { appendFileSync } from "node:fs";
      export default {
        plugins: [{
          name: "plain",
          config: () => { appendFileSync(new URL("./plain-hooks.log", import.meta.url), "config\\n"); return null; },
        }],
      };\n`,
    );
    const plain = JSON.parse(runExtractor(base, "plain.config.mjs"));
    assert.equal(plain.__ok, true);
    assert.equal(plain.ssr?.runnerBacked, undefined);
    assert.equal(fs.readFileSync(join(base, "plain-hooks.log"), "utf8"), "config\n");

    // configEnvironment-declared factories are seen by the sentinel too.
    fs.writeFileSync(
      join(base, "envhook.config.mjs"),
      `export default {
        plugins: [{
          name: "declares-via-configEnvironment",
          configEnvironment: (name) => (name === "ssr" ? { dev: { createEnvironment: () => ({}) } } : null),
        }],
      };\n`,
    );
    const envhook = JSON.parse(runExtractor(base, "envhook.config.mjs"));
    assert.equal(envhook.__ok, true);
    assert.equal(envhook.ssr?.runnerBacked, true, "a configEnvironment declaration is a runner declaration");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// When the raw config cannot be loaded but resolveConfig still succeeds, the
// degenerate path is LOUD and safe: a warning names the failure, and the
// resolved ssr.resolve sugar (possibly a foreign runtime's conditions) is
// withheld so Node consumers can never adopt it with detection unavailable.
test("a failed raw load warns and withholds the resolved ssr.resolve sugar", () => {
  const base = extractorDir("oj-vite-extract-rawnull-");
  try {
    fs.mkdirSync(join(base, "node_modules", "vite"), { recursive: true });
    fs.writeFileSync(
      join(base, "node_modules", "vite", "package.json"),
      JSON.stringify({ name: "vite", version: "0.0.0-stub", type: "module", main: "index.mjs" }),
    );
    fs.writeFileSync(
      join(base, "node_modules", "vite", "index.mjs"),
      `export async function loadConfigFromFile() { throw new Error("raw load exploded"); }
      export function mergeConfig(a, b) { return { ...a, ...b }; }
      export async function resolveConfig() {
        return {
          base: "/resolved/",
          ssr: { target: "webworker", resolve: { conditions: ["workerd"], externalConditions: ["workerd"] } },
          resolve: { conditions: ["workerd", "browser"], externalConditions: ["workerd"], extensions: [".mjs", ".js"] },
          configFileDependencies: [],
        };
      }\n`,
    );
    fs.writeFileSync(join(base, "vite.config.mjs"), "export default {};\n");
    const child = spawnExtractor(base, "vite.config.mjs");
    assert.match(
      child.stderr,
      /could not load the raw config .*raw load exploded.*ssr\.resolve conditions are unavailable/s,
      "the degenerate path is loud, never silent",
    );
    const out = JSON.parse(child.stdout);
    assert.equal(out.__ok, true);
    assert.equal(out.base, "/resolved/");
    assert.equal(out.ssr?.resolve, undefined, "foreign resolved conditions are withheld without detection");
    assert.equal(out.ssr?.target, "webworker", "the rest of the ssr block still travels");
    assert.equal(out.ssr?.runnerBacked, undefined);
    assert.equal(out.rawResolve, null);
    // The resolved TOP-LEVEL conditions are withheld on this path too — the
    // whole point is that no foreign conditions reach Node consumers
    // undetected; the condition-free resolve keys still travel.
    assert.equal(out.resolve?.conditions, undefined, "resolved top-level conditions are withheld without detection");
    assert.equal(out.resolve?.externalConditions, undefined);
    assert.deepEqual(out.resolve?.extensions, [".mjs", ".js"], "condition-free resolve keys still travel");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// Tests against the REAL vite installed in the start-app fixture: the
// single-eval flow through vite's actual loadConfigFromFile/resolveConfig.
const realViteSrc = join(repo, "e2e/fixtures/start-app/node_modules/vite");
const hasRealVite = fs.existsSync(realViteSrc);
const skipNoVite = hasRealVite ? false : "fixture vite not installed";

function realViteDir(prefix) {
  const base = mkdtempSync(join(tmpdir(), prefix));
  copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
  writeFileSync(
    join(base, "package.json"),
    JSON.stringify({ name: "fx", type: "module", dependencies: { vite: "*" } }),
  );
  mkdirSync(join(base, "node_modules"));
  // vite's own dependencies resolve from its realpath (the fixture's
  // node_modules), so one symlink is a full install.
  symlinkSync(realViteSrc, join(base, "node_modules", "vite"), "dir");
  return base;
}

// The single-eval flow hands the FILE's config to resolveConfig as the inline
// config, and Vite computes isSsrBuild from the inline config alone — so the
// file's build.ssr must not ride it (hooks would see true where real Vite
// gives false). The value re-enters the config at Vite's own post-load merge
// seam (before user config hooks), and the emitted build block keeps it.
test("real vite: hooks see isSsrBuild false while the file's build.ssr is preserved", { skip: skipNoVite }, () => {
  const base = realViteDir("oj-vite-extract-realssr-");
  try {
    writeFileSync(
      join(base, "vite.config.mjs"),
      `import { writeFileSync } from "node:fs";
      export default {
        build: { ssr: "src/server.ts" },
        plugins: [{
          name: "env-probe",
          config(conf, env) {
            writeFileSync(
              new URL("./probe.json", import.meta.url),
              JSON.stringify({ isSsrBuild: env.isSsrBuild, buildSsr: conf.build?.ssr ?? null }),
            );
            return null;
          },
        }],
      };\n`,
    );
    const out = JSON.parse(runExtractor(base, "vite.config.mjs", ["build", "production", "explicit"]));
    assert.equal(out.__ok, true);
    const probe = JSON.parse(fs.readFileSync(join(base, "probe.json"), "utf8"));
    assert.equal(probe.isSsrBuild, false, "real Vite computes isSsrBuild from the inline config only");
    assert.equal(
      probe.buildSsr,
      "src/server.ts",
      "the file's build.ssr is back in the config before user hooks run, as under Vite's post-load merge",
    );
    assert.equal(out.build?.ssr, "src/server.ts", "the extracted build block keeps the file's build.ssr");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// resolveConfig fills defaults the user never wrote (optimizeDeps.noDiscovery:
// false, empty server.warmup arrays): adopting them as explicit settings would
// force-enable dep auto-discovery for every app and ship empty warmup blobs.
// Only values visible in the RAW user config may cross.
test("real vite: resolved-config defaults are not adopted as user settings", { skip: skipNoVite }, () => {
  const base = realViteDir("oj-vite-extract-defaults-");
  try {
    writeFileSync(join(base, "vite.config.mjs"), "export default { base: '/app/' };\n");
    const out = JSON.parse(runExtractor(base, "vite.config.mjs"));
    assert.equal(out.__ok, true);
    assert.equal(out.optimizeDeps?.noDiscovery, undefined, "Vite's default noDiscovery:false must not cross");
    assert.equal(out.serverFlags?.warmup, undefined, "Vite's default empty warmup must not cross");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// resolveConfig can throw AFTER the plugin config hooks ran (a throwing
// configResolved): the sentinel's partial verdict must survive the throw, and
// the hooks must NOT re-run out of band on the same instances (double side
// effects; run-once guards break). The failure is loud, naming the error.
test("real vite: a throwing configResolved keeps the partial verdict and never re-runs config hooks", { skip: skipNoVite }, () => {
  const base = realViteDir("oj-vite-extract-throwres-");
  try {
    writeFileSync(
      join(base, "vite.config.mjs"),
      `import { appendFileSync } from "node:fs";
      export default {
        base: "/partial/",
        plugins: [
          {
            name: "declares",
            config: () => {
              appendFileSync(new URL("./hooks.log", import.meta.url), "config\\n");
              return { environments: { worker: { dev: { createEnvironment: () => ({}) } } } };
            },
          },
          { name: "boom", configResolved: () => { throw new Error("configResolved exploded"); } },
        ],
      };\n`,
    );
    const child = spawnExtractor(base, "vite.config.mjs");
    assert.match(
      child.stderr,
      /resolveConfig failed after plugin config hooks ran.*configResolved exploded/s,
      "the mid-run failure is loud and names the error",
    );
    const out = JSON.parse(child.stdout);
    assert.equal(out.__ok, true);
    assert.equal(out.base, "/partial/", "the raw config's values still travel");
    assert.equal(out.ssr?.runnerBacked, true, "the sentinel's partial verdict survives the throw");
    assert.equal(
      fs.readFileSync(join(base, "hooks.log"), "utf8"),
      "config\n",
      "the declaring plugin's config hook ran exactly once — never re-run after the throw",
    );
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// mergeConfig shares untouched subtrees with the loaded file config, so a
// plugin config hook that MUTATES the config in place used to write into the
// object later emitted as the RAW config: rawResolve must reflect the file,
// not the hook's mutation.
test("real vite: an in-place config mutation does not leak into rawResolve", { skip: skipNoVite }, () => {
  const base = realViteDir("oj-vite-extract-mutate-");
  try {
    writeFileSync(
      join(base, "vite.config.mjs"),
      `export default {
        resolve: { conditions: ["custom"] },
        plugins: [{
          name: "mutator",
          config(conf) {
            (conf.resolve.conditions ??= []).push("workerd-injected");
            return null;
          },
        }],
      };\n`,
    );
    const out = JSON.parse(runExtractor(base, "vite.config.mjs"));
    assert.equal(out.__ok, true);
    assert.deepEqual(
      out.rawResolve,
      { conditions: ["custom"] },
      "rawResolve is the FILE's resolve block, snapshotted before any hook ran",
    );
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// The partial verdict must not false-negative a RAW declaration: when a config
// hook throws BEFORE the sentinel's post-ordered sniff ran, declared() is
// false even though the config FILE itself declares a runner environment (the
// CF shape). The emit ORs in the shape-only raw check — no hook re-runs.
test("real vite: a mid-run hook throw keeps a raw runner declaration visible in the partial verdict", { skip: skipNoVite }, () => {
  const base = realViteDir("oj-vite-extract-partialraw-");
  try {
    const rawDeclaring = (boomHook) =>
      `export default {
        base: "/partial-raw/",
        environments: { worker: { dev: { createEnvironment: () => ({}) } } },
        plugins: [{ name: "boom", ${boomHook} }],
      };\n`;
    // A throwing config hook: it aborts resolveConfig before the sentinel's
    // post sniff ever ran (started=true via the pre plugin, declared=false).
    writeFileSync(join(base, "vite.config.mjs"), rawDeclaring('config: () => { throw new Error("config exploded"); }'));
    const run = () => spawnExtractor(base, "vite.config.mjs");
    let child = run();
    assert.match(child.stderr, /resolveConfig failed after plugin config hooks ran.*config exploded/s);
    let out = JSON.parse(child.stdout);
    assert.equal(out.__ok, true);
    assert.equal(out.base, "/partial-raw/");
    assert.equal(out.ssr?.runnerBacked, true, "the raw declaration survives a pre-sniff hook throw");
    // The configResolved variant (hooks all ran, resolveConfig still threw):
    // the sentinel decided true and the OR keeps it.
    writeFileSync(join(base, "vite.config.mjs"), rawDeclaring('configResolved: () => { throw new Error("configResolved exploded"); }'));
    child = run();
    out = JSON.parse(child.stdout);
    assert.equal(out.__ok, true);
    assert.equal(out.ssr?.runnerBacked, true, "a configResolved throw keeps the verdict too");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// A `cloudflare({ configPath })`-relocated Worker config can carry ANY name
// and live anywhere (a custom worker.jsonc, a
// CLOUDFLARE_VITE_WRANGLER_CONFIG_PATH override, a .wrangler/deploy/config.json
// redirect) — places the cache's root-level epoch (wrangler.* in the root)
// cannot see. The extractor records every .json/.jsonc/.toml file the config
// evaluation READS — the read is the signal, not a filename list — existence
// probes of missing ones included, node_modules excluded, and folds them into
// __deps, so the Rust extraction cache stamps them like config imports (an
// edit, create or delete is then a cache miss).
test("config-shaped files the evaluation reads travel in __deps, custom names and missing probes included", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-wrangler-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module" }));
    mkdirSync(join(base, "config"));
    mkdirSync(join(base, "node_modules"));
    writeFileSync(join(base, "config", "wrangler.jsonc"), JSON.stringify({ base: "/from-wrangler/" }));
    // A custom-named Worker config (cloudflare({ configPath: "./config/worker.jsonc" })).
    writeFileSync(join(base, "config", "worker.jsonc"), JSON.stringify({ name: "custom" }));
    writeFileSync(join(base, "node_modules", "dep-meta.json"), "{}");
    writeFileSync(
      join(base, "vite.config.mjs"),
      `import fs from "node:fs";
      const w = JSON.parse(fs.readFileSync(new URL("./config/wrangler.jsonc", import.meta.url), "utf8"));
      fs.readFileSync(new URL("./config/worker.jsonc", import.meta.url), "utf8");
      fs.readFileSync(new URL("./node_modules/dep-meta.json", import.meta.url), "utf8");
      fs.existsSync(new URL("./aux/wrangler.toml", import.meta.url));
      export default { base: w.base };\n`,
    );
    const out = JSON.parse(runExtractor(base, "vite.config.mjs"));
    assert.equal(out.__ok, true);
    assert.equal(out.base, "/from-wrangler/", "the relocated wrangler config was read");
    // The config module's import.meta.url is symlink-free (Node's ESM loader
    // realpaths module locations), so the recorded paths are too.
    const real = fs.realpathSync(base);
    assert.ok(
      out.__deps.includes(join(real, "config", "wrangler.jsonc")),
      `the read wrangler config is a stamped dep: ${JSON.stringify(out.__deps)}`,
    );
    assert.ok(
      out.__deps.includes(join(real, "config", "worker.jsonc")),
      "a custom-named .jsonc read via configPath is stamped — the read is the signal",
    );
    assert.ok(
      out.__deps.includes(join(real, "aux", "wrangler.toml")),
      "a probed-but-missing config file is stamped too (creating it must invalidate)",
    );
    assert.ok(
      !out.__deps.some((d) => d.split("/").includes("node_modules")),
      `node_modules reads never join __deps: ${JSON.stringify(out.__deps)}`,
    );
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// Vite's resolveConfig loads `.env`, `.env.local`, `.env.${mode}` and
// `.env.${mode}.local` under the RESOLVED envDir (getEnvFilesForMode →
// loadEnv), and their values change the verdict. The read recorder stamps the
// env files the evaluation actually consulted — the probed-but-missing ones
// included — so editing (or creating) `.env.production` invalidates a
// build-mode extraction even under a custom envDir the root-level epoch
// cannot name.
test("real vite: the mode's env files under the resolved envDir travel in __deps", { skip: skipNoVite }, () => {
  const base = realViteDir("oj-vite-extract-envdeps-");
  try {
    fs.mkdirSync(join(base, "cfg"));
    writeFileSync(join(base, "vite.config.mjs"), 'export default { envDir: "./cfg" };\n');
    writeFileSync(join(base, "cfg", ".env"), "VITE_A=1\n");
    writeFileSync(join(base, "cfg", ".env.production"), "VITE_B=2\n");
    const out = JSON.parse(runExtractor(base, "vite.config.mjs", ["build", "production", "explicit"]));
    assert.equal(out.__ok, true);
    // Vite resolves the envDir from the root as handed in (no realpath), but
    // stay robust to either spelling.
    const real = fs.realpathSync(base);
    const dep = (rel) => out.__deps.includes(join(base, rel)) || out.__deps.includes(join(real, rel));
    assert.ok(dep("cfg/.env"), `.env read by loadEnv is a stamped dep: ${JSON.stringify(out.__deps)}`);
    assert.ok(
      dep("cfg/.env.production"),
      "the build mode's .env.production is a stamped dep — editing it must invalidate",
    );
    assert.ok(
      dep("cfg/.env.production.local"),
      "a probed-but-missing mode env file is stamped too (creating it must invalidate)",
    );
    assert.equal(out.__depsTruncated, undefined, "a whole stamp carries no truncation flag");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// The read recorder's cap is honest: overflowing it flags the emit
// (__depsTruncated) so the Rust side serves the result WITHOUT caching it —
// an incomplete stamp could survive an edit to an unrecorded file. Re-reading
// an already recorded path at the cap is NOT truncation.
test("recorder overflow sets __depsTruncated; re-reads at the cap do not", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-trunc-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module" }));
    writeFileSync(join(base, "a.json"), "{}");
    writeFileSync(join(base, "b.json"), "{}");
    writeFileSync(
      join(base, "vite.config.mjs"),
      `import fs from "node:fs";
      fs.readFileSync(new URL("./a.json", import.meta.url), "utf8");
      fs.readFileSync(new URL("./b.json", import.meta.url), "utf8");
      export default { base: "/t/" };\n`,
    );
    writeFileSync(
      join(base, "dupe.config.mjs"),
      `import fs from "node:fs";
      fs.readFileSync(new URL("./a.json", import.meta.url), "utf8");
      fs.readFileSync(new URL("./a.json", import.meta.url), "utf8");
      export default { base: "/d/" };\n`,
    );
    const run = (config, env) => JSON.parse(runExtractor(base, config, [], { env: { ...process.env, ...env } }));
    const capped = run("vite.config.mjs", { OJ_OBSERVED_READS_MAX: "1" });
    assert.equal(capped.__ok, true);
    assert.equal(capped.__depsTruncated, true, "a dropped NEW path past the cap flags the emit");
    const whole = run("vite.config.mjs", {});
    assert.equal(whole.__ok, true);
    assert.equal(whole.__depsTruncated, undefined, "no overflow, no flag");
    const real = fs.realpathSync(base);
    assert.ok(whole.__deps.includes(join(real, "b.json")));
    const dupe = run("dupe.config.mjs", { OJ_OBSERVED_READS_MAX: "1" });
    assert.equal(dupe.__ok, true);
    assert.equal(dupe.__depsTruncated, undefined, "re-reading a recorded path at the cap is not truncation");
    assert.ok(dupe.__deps.includes(join(real, "a.json")));
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// Vite sets NODE_ENV (when the environment leaves it unset) BEFORE the config
// file's module evaluation, so a config branching on process.env.NODE_ENV at
// module scope sees a value; the extractor mirrors it on every loader path,
// the vite-less plain import included. The default follows the COMMAND, never
// the mode (Vite: serve resolves with defaultNodeEnv "development", build with
// "production") — a custom `--mode staging` changes neither.
test("a config branching on NODE_ENV during module evaluation sees the command's default", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-nodeenv-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module" }));
    writeFileSync(
      join(base, "vite.config.mjs"),
      'export default { base: process.env.NODE_ENV === "production" ? "/prod/" : "/dev/" };\n',
    );
    const env = { ...process.env };
    delete env.NODE_ENV;
    const prod = JSON.parse(runExtractor(base, "vite.config.mjs", ["build", "production", "explicit"], { env }));
    assert.equal(prod.__ok, true);
    assert.equal(prod.base, "/prod/", "build evaluates the config's prod branch");
    const dev = JSON.parse(runExtractor(base, "vite.config.mjs", ["serve", "development", "default"], { env }));
    assert.equal(dev.base, "/dev/");
    // A CUSTOM mode changes nothing: NODE_ENV derives from the command.
    const buildStaging = JSON.parse(runExtractor(base, "vite.config.mjs", ["build", "staging", "explicit"], { env }));
    assert.equal(buildStaging.base, "/prod/", "build --mode staging still sees NODE_ENV=production");
    const serveStaging = JSON.parse(runExtractor(base, "vite.config.mjs", ["serve", "staging", "explicit"], { env }));
    assert.equal(serveStaging.base, "/dev/", "serve --mode staging still sees NODE_ENV=development");
    // An environment that already names NODE_ENV wins, as under Vite.
    const forced = JSON.parse(
      runExtractor(base, "vite.config.mjs", ["build", "production", "explicit"], {
        env: { ...env, NODE_ENV: "development" },
      }),
    );
    assert.equal(forced.base, "/dev/", "a set NODE_ENV is never overridden");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// Same rule through the real-vite single-eval flow, observed where it matters:
// resolveConfig's own isProduction. The extractor pre-sets NODE_ENV for the
// config file's module evaluation but UN-sets it again before resolveConfig,
// so Vite still computes isNodeEnvSet=false — its VITE_USER_NODE_ENV handling
// (a `.env` file's NODE_ENV=development under build) stays live instead of
// being disabled by the pre-set.
test("real vite: NODE_ENV follows the command under a custom mode and VITE_USER_NODE_ENV stays live", { skip: skipNoVite }, () => {
  const base = realViteDir("oj-vite-extract-nodeenv-cmd-");
  try {
    writeFileSync(
      join(base, "vite.config.mjs"),
      `import { writeFileSync } from "node:fs";
      export default {
        plugins: [{
          name: "prod-probe",
          configResolved(config) {
            writeFileSync(
              new URL("./prod.json", import.meta.url),
              JSON.stringify({ isProduction: config.isProduction, nodeEnv: process.env.NODE_ENV }),
            );
          },
        }],
      };\n`,
    );
    const env = { ...process.env };
    delete env.NODE_ENV;
    const readProbe = () => JSON.parse(fs.readFileSync(join(base, "prod.json"), "utf8"));
    let out = JSON.parse(runExtractor(base, "vite.config.mjs", ["build", "staging", "explicit"], { env }));
    assert.equal(out.__ok, true);
    assert.deepEqual(readProbe(), { isProduction: true, nodeEnv: "production" }, "build --mode staging is a production build");
    out = JSON.parse(runExtractor(base, "vite.config.mjs", ["serve", "staging", "explicit"], { env }));
    assert.equal(out.__ok, true);
    assert.deepEqual(readProbe(), { isProduction: false, nodeEnv: "development" }, "serve --mode staging stays development");
    // The .env file's NODE_ENV=development (Vite's VITE_USER_NODE_ENV) must
    // still win under build: the extractor's pre-set NODE_ENV is unset before
    // resolveConfig, so Vite sees isNodeEnvSet=false and honors it.
    writeFileSync(join(base, ".env"), "NODE_ENV=development\n");
    out = JSON.parse(runExtractor(base, "vite.config.mjs", ["build", "production", "explicit"], { env }));
    assert.equal(out.__ok, true);
    assert.deepEqual(
      readProbe(),
      { isProduction: false, nodeEnv: "development" },
      "a .env NODE_ENV=development still makes a development build (VITE_USER_NODE_ENV handling not disabled)",
    );
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// The pre-set is unset before resolveConfig ONLY while NODE_ENV still equals
// it: a config module ASSIGNING process.env.NODE_ENV at module scope keeps its
// value through the hooks — Vite snapshots isNodeEnvSet BEFORE the load, so
// the assignment reaches resolveConfig's own isProduction — instead of being
// deleted along with oj's untouched pre-set.
test("real vite: a config-module NODE_ENV assignment survives into resolveConfig", { skip: skipNoVite }, () => {
  const base = realViteDir("oj-vite-extract-nodeenv-assign-");
  try {
    writeFileSync(
      join(base, "vite.config.mjs"),
      `import { writeFileSync } from "node:fs";
      process.env.NODE_ENV = "test";
      export default {
        plugins: [{
          name: "prod-probe",
          configResolved(config) {
            writeFileSync(
              new URL("./prod.json", import.meta.url),
              JSON.stringify({ isProduction: config.isProduction, nodeEnv: process.env.NODE_ENV }),
            );
          },
        }],
      };\n`,
    );
    const env = { ...process.env };
    delete env.NODE_ENV;
    const out = JSON.parse(runExtractor(base, "vite.config.mjs", ["build", "production", "explicit"], { env }));
    assert.equal(out.__ok, true);
    assert.deepEqual(
      JSON.parse(fs.readFileSync(join(base, "prod.json"), "utf8")),
      { isProduction: false, nodeEnv: "test" },
      "the module-scope NODE_ENV=test assignment survives the pre-set unset and steers isProduction",
    );
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});


test("Vite server and optimizer settings survive actual config loading", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-adopt-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module" }));
    writeFileSync(join(base, "vite.config.mjs"), `export default {
      server: { fs: { allow: ["../shared"], deny: ["**/*.private"] },
        warmup: { clientFiles: ["./src/*.tsx"], ssrFiles: ["./server.ts"] } },
      optimizeDeps: { noDiscovery: true, rolldownOptions: {
        transform: { define: { __FEATURE__: '"enabled"' }, target: "es2015" },
        resolve: { conditionNames: ["custom"] }
      } }
    }`);
    const config = JSON.parse(runExtractor(base, "vite.config.mjs"));
    assert.equal(config.__ok, true);
    assert.deepEqual(config.serverFlags.fsDeny, ["**/*.private"]);
    assert.deepEqual(config.serverFlags.warmup, { clientFiles: ["./src/*.tsx"], ssrFiles: ["./server.ts"] });
    assert.equal(config.optimizeDeps.noDiscovery, true);
    assert.equal(config.optimizeDeps.rolldownOptions.transform.target, "es2015");
    assert.deepEqual(config.optimizeDeps.rolldownOptions.resolve.conditionNames, ["custom"]);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
