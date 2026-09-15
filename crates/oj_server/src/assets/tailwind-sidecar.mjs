// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { existsSync, fstatSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { basename, dirname } from "node:path";
import readline from "node:readline";

const processors = new Map();

// The config file oj found the way postcss-load-config does (OJ_POSTCSS_CONFIG:
// postcss.config.*, .postcssrc*, or a package.json with a `postcss` key, possibly
// in a parent directory up to the workspace root), read by its kind.
async function readPostcssConfig(base) {
  const cfgPath =
    process.env.OJ_POSTCSS_CONFIG ||
    ["postcss.config.js", "postcss.config.cjs", "postcss.config.mjs"].map((n) => `${base}/${n}`).find((p) => existsSync(p));
  if (!cfgPath) return null;
  const name = basename(cfgPath);
  let config;
  if (name === "package.json") {
    config = JSON.parse(readFileSync(cfgPath, "utf8")).postcss;
  } else if (name === ".postcssrc" || name.endsWith(".json")) {
    config = JSON.parse(readFileSync(cfgPath, "utf8"));
  } else {
    const mod = await import(pathToFileURL(cfgPath).href);
    config = mod.default ?? mod;
  }
  if (typeof config === "function") {
    config = config({ env: process.env.NODE_ENV || "development", cwd: base, options: {} });
  }
  return config ? { config, dir: dirname(cfgPath) } : null;
}

// Resolve `postcss` and plugin packages from the config's own directory first
// (a workspace-root config installs them there), then from the app.
function resolver(dirs) {
  const reqs = dirs.map((d) => createRequire(d + "/package.json"));
  return (spec) => {
    let err;
    for (const req of reqs) {
      try {
        return req.resolve(spec);
      } catch (e) {
        err = e;
      }
    }
    throw err;
  };
}

async function loadPostcss(base) {
  if (processors.has(base)) return processors.get(base);
  let processor = null;
  const found = await readPostcssConfig(base);
  if (found) {
    const resolve = resolver(found.dir === base ? [base] : [found.dir, base]);
    let postcss;
    try {
      postcss = (await import(resolve("postcss"))).default;
    } catch {
      postcss = null;
    }
    if (postcss) {
      const raw = found.config.plugins ?? {};
      const plugins = [];
      if (Array.isArray(raw)) {
        for (const p of raw) if (p) plugins.push(p);
      } else {
        for (const [name, opts] of Object.entries(raw)) {
          if (opts === false) continue;
          const imported = await import(pathToFileURL(resolve(name)).href);
          const factory = imported.default ?? imported;
          plugins.push(typeof factory === "function" ? factory(opts ?? {}) : factory);
        }
      }
      processor = postcss(plugins);
    }
  }
  processors.set(base, processor);
  return processor;
}

async function v4Compile(base, css, from) {
  const req = createRequire(base + "/package.json");
  const tw = await import(req.resolve("@tailwindcss/node"));
  const oxide = await import(req.resolve("@tailwindcss/oxide"));
  const compiler = await tw.compile(css, { base, from, onDependency: () => {} });
  const scanner = new oxide.Scanner({ sources: [{ base, pattern: "**/*", negated: false }] });
  return compiler.build(scanner.scan());
}

async function compileCss(base, css, from) {
  const processor = await loadPostcss(base);
  if (processor) {
    const result = await processor.process(css, { from, map: false });
    return result.css;
  }
  return v4Compile(base, css, from);
}

if (process.argv[2] === "--once") {
  const [file, base] = process.argv.slice(3);
  compileCss(base, readFileSync(file, "utf8"), file)
    .then((css) => { process.stdout.write(css); })
    .catch((err) => { console.error(String(err)); process.exit(1); });
} else {
  const rl = readline.createInterface({ input: process.stdin });
  let inflight = 0;
  let stdinClosed = false;
  const maybeExit = () => { if (stdinClosed && inflight === 0) process.exit(0); };
  try {
    if (fstatSync(0, { bigint: true }).isFIFO()) rl.once("close", () => { stdinClosed = true; maybeExit(); });
  } catch {}
  rl.on("line", async (line) => {
    let msg;
    try { msg = JSON.parse(line); } catch { return; }
    inflight += 1;
    try {
      const css = await compileCss(msg.base, msg.css, msg.from);
      await new Promise((resolve) => process.stdout.write(JSON.stringify({ id: msg.id, css }) + "\n", resolve));
    } catch (err) {
      await new Promise((resolve) => process.stdout.write(JSON.stringify({ id: msg.id, error: String(err) }) + "\n", resolve));
    } finally {
      inflight -= 1;
      maybeExit();
    }
  });
}
