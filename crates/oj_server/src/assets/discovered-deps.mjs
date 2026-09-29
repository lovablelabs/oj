// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The discovered-deps ledger, shared by the plugin host and the preseed
// optimizer child (both are materialized into the same cache directory and
// import this file as a sibling). Deps that only the runtime discovers (a
// plugin injecting scanner-invisible imports — Cloudflare's unenv polyfills
// are the recurring case) get recorded by the host and folded into
// optimizeDeps.include by BOTH processes on the next boot: Vite's configHash
// covers `include` sorted and deduped (optimizer/index.ts getConfigHash), so
// the child-seeded metadata only validates when both sides apply the same
// SET. A stale entry (dep since removed) costs only Vite's "present in
// optimizeDeps.include" warning.

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

export function foldDiscoveredDeps(rc, ledgerPath) {
  if (!ledgerPath) return;
  let ledger;
  try {
    ledger = JSON.parse(readFileSync(ledgerPath, "utf8"));
  } catch {
    return;
  }
  for (const [name, ids] of Object.entries(ledger ?? {})) {
    const env = rc.environments?.[name];
    if (!env || !Array.isArray(ids) || ids.length === 0) continue;
    const optimizeDeps = (env.optimizeDeps ??= {});
    const merged = new Set(optimizeDeps.include ?? []);
    for (const id of ids) if (typeof id === "string") merged.add(id);
    optimizeDeps.include = [...merged];
  }
}

// Capped, debounced, written atomically; bare specifiers only (a relative or
// absolute discovered id is app source, not a dependency).
const state = { byEnv: new Map(), timer: null, loaded: false };
export function recordDiscoveredDep(ledgerPath, envName, id) {
  if (typeof id !== "string" || id.startsWith("/") || id.startsWith(".")) return;
  if (!state.loaded) {
    state.loaded = true;
    try {
      for (const [n, ids] of Object.entries(JSON.parse(readFileSync(ledgerPath, "utf8")) ?? {})) {
        if (Array.isArray(ids)) state.byEnv.set(n, new Set(ids));
      }
    } catch {}
  }
  const ids = state.byEnv.get(envName) ?? new Set();
  state.byEnv.set(envName, ids);
  if (ids.has(id) || ids.size >= 200) return;
  ids.add(id);
  clearTimeout(state.timer);
  state.timer = setTimeout(() => {
    const out = {};
    for (const [n, set] of state.byEnv) out[n] = [...set].sort();
    try {
      mkdirSync(dirname(ledgerPath), { recursive: true });
      const tmp = `${ledgerPath}.tmp-${process.pid}`;
      writeFileSync(tmp, JSON.stringify(out, null, 2));
      renameSync(tmp, ledgerPath);
    } catch {}
  }, 500);
  if (state.timer.unref) state.timer.unref();
}
