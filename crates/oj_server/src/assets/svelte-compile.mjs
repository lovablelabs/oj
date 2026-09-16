// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { fstatSync } from "node:fs";
import { pathToFileURL } from "node:url";
import path from "node:path";
import readline from "node:readline";

const cache = new Map();

async function toolchain(base) {
  if (cache.has(base)) return cache.get(base);
  const req = createRequire(path.join(base, "package.json"));
  const mod = await import(pathToFileURL(req.resolve("svelte/compiler")).href);
  const svelte = mod.compile ? mod : (mod.default ?? mod);
  let preprocessors = null;
  try {
    const vps = await import(pathToFileURL(req.resolve("@sveltejs/vite-plugin-svelte")).href);
    if (typeof vps.vitePreprocess === "function") preprocessors = vps.vitePreprocess();
  } catch {}
  const entry = { compile: svelte.compile, preprocess: svelte.preprocess, preprocessors };
  cache.set(base, entry);
  return entry;
}

const rl = readline.createInterface({ input: process.stdin });
let inflight = 0;
let stdinClosed = false;

const maybeExit = () => {
  if (!stdinClosed || inflight !== 0) return;

  process.stdout.end((err) => {
    process.exit(err ? 1 : 0)
  })
};

try {
  if (fstatSync(0, { bigint: true }).isFIFO()) rl.once("close", () => { stdinClosed = true; maybeExit(); });
} catch {}
rl.on("line", async (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  const { id, base, css: source, from } = msg;
  const dev = msg.dev !== false;
  inflight += 1;
  try {
    const { compile, preprocess, preprocessors } = await toolchain(base);
    let code = source;
    if (preprocessors) {
      const pp = await preprocess(source, preprocessors, { filename: from });
      code = pp.code;
    }
    const out = compile(code, {
      filename: from,
      generate: "client",
      css: "injected",
      dev,
      hmr: dev,
    });
    process.stdout.write(JSON.stringify({ id, css: out.js.code }) + "\n");
  } catch (e) {
    process.stdout.write(JSON.stringify({ id, error: String((e && e.message) || e) }) + "\n");
  } finally {
    inflight -= 1;
    maybeExit();
  }
});
