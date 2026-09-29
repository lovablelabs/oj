// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Optimizer include extension, shared by the preseed child and the plugin
// host (both are materialized into the same cache directory and import this
// file as a sibling). Deps that only the runtime discovers (a plugin
// injecting scanner-invisible imports — Cloudflare's unenv polyfills are the
// recurring case) force an in-host optimize whose commit some plugins follow
// with server.restart(). Vite persists every committed optimize's dep set in
// _metadata.json, so no hook is needed: the preseed child reads the PRIOR
// metadata, snapshots the per-environment dep ids to one file, and both
// processes fold that same snapshot into optimizeDeps.include. The snapshot
// file (not live metadata) is what keeps the two sides identical: Vite's
// configHash covers `include` sorted and deduped (optimizer/index.ts
// getConfigHash), and the child rewrites metadata between the two reads.
// A stale entry (dep since removed) costs only Vite's "present in
// optimizeDeps.include" warning.

import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

// The dep ids of every environment's LAST committed optimize, read from
// Vite's own metadata (valid or stale — a hash-invalidated file still names
// the deps that were needed).
export function priorDepIds(rc) {
  const out = {};
  for (const name of Object.keys(rc.environments ?? {})) {
    const dir = join(rc.cacheDir, name === "client" ? "deps" : `deps_${name}`);
    let metadata;
    try {
      metadata = JSON.parse(readFileSync(join(dir, "_metadata.json"), "utf8"));
    } catch {
      continue;
    }
    const ids = new Set([...Object.keys(metadata?.optimized ?? {}), ...Object.keys(metadata?.discovered ?? {})]);
    if (ids.size > 0) out[name] = [...ids].sort().slice(0, 500);
  }
  return out;
}

// Written by the preseed child BEFORE it resolves its config (atomic rename;
// the host may already be reading a previous snapshot).
export function writeIncludeSnapshot(snapshotPath, byEnv) {
  try {
    mkdirSync(dirname(snapshotPath), { recursive: true });
    const tmp = `${snapshotPath}.tmp-${process.pid}`;
    writeFileSync(tmp, JSON.stringify(byEnv, null, 2));
    renameSync(tmp, snapshotPath);
  } catch {}
}

export function foldIncludeSnapshot(rc, snapshotPath) {
  if (!snapshotPath || !existsSync(snapshotPath)) return;
  let byEnv;
  try {
    byEnv = JSON.parse(readFileSync(snapshotPath, "utf8"));
  } catch {
    return;
  }
  for (const [name, ids] of Object.entries(byEnv ?? {})) {
    const env = rc.environments?.[name];
    if (!env || !Array.isArray(ids) || ids.length === 0) continue;
    const optimizeDeps = (env.optimizeDeps ??= {});
    const merged = new Set(optimizeDeps.include ?? []);
    for (const id of ids) if (typeof id === "string") merged.add(id);
    optimizeDeps.include = [...merged];
  }
}
