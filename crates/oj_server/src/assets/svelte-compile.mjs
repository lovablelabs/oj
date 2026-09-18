// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Svelte compiler for oj's in-process JS engine. The engine loads this module
// once and calls `compile` per request; there is no process protocol here.

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import path from "node:path";

const cache = new Map();

// A resolution failure for the compiler package itself carries this marker so
// the host can print the "is <package> installed?" hint.
function missingPackage(name, err) {
  return new Error(`OJ_MISSING_PACKAGE ${name}: ${(err && err.message) || err}`);
}

async function toolchain(base) {
  if (cache.has(base)) return cache.get(base);
  const req = createRequire(pathToFileURL(path.join(base, "package.json")).href);
  let compilerPath;
  try {
    compilerPath = req.resolve("svelte/compiler");
  } catch (e) {
    throw missingPackage("svelte", e);
  }
  const mod = await import(pathToFileURL(compilerPath).href);
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

export async function compile(request) {
  const { base, css: source, from } = request;
  const dev = request.dev !== false;
  const { compile: compileSvelte, preprocess, preprocessors } = await toolchain(base);
  let code = source;
  if (preprocessors) {
    const pp = await preprocess(source, preprocessors, { filename: from });
    code = pp.code;
  }
  const out = compileSvelte(code, {
    filename: from,
    generate: "client",
    css: "injected",
    dev,
    hmr: dev,
  });
  return out.js.code;
}
