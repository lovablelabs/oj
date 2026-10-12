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
// A stale entry for a removed package costs only Vite's "present in
// optimizeDeps.include" warning and ages out at the next metadata-bearing
// boot; one whose package no longer exports it fails the optimize outright,
// so the preseed child retries without the carried ids and clears them. Expected shape: the boot after a discovery session re-optimizes ONCE
// (in the child — the grown include changes configHash), then the set reaches
// its fixed point and every later boot validates.

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
    // On-disk metadata carries every committed dep — including mid-session
    // discoveries — under `optimized` (stringifyDepsOptimizerMetadata writes
    // no `discovered` section). Capped deterministically: sorted, so the
    // include set is stable across boots even past the cap.
    const ids = new Set(Object.keys(metadata?.optimized ?? {}));
    if (ids.size > 0) out[name] = [...ids].sort().slice(0, 500);
  }
  return out;
}

// Written by the preseed child BEFORE it resolves its config (atomic rename;
// the host may already be reading a previous snapshot).
// MERGED per environment, never replaced wholesale: an environment with
// metadata on disk is authoritative (its dep set is the fixed-point superset,
// and replacement is what lets removed deps age out), but an environment with
// NO metadata keeps its previous snapshot entry — otherwise a `.vite` wipe
// with a surviving `.oj-cache` (a node_modules reinstall) would clobber the
// accumulated set with nothing and bring back the discovery-restart double
// boot for exactly the case this exists for. Returns whether the file was written.
export function writeIncludeSnapshot(snapshotPath, byEnv) {
  try {
    let previous = {};
    try {
      previous = JSON.parse(readFileSync(snapshotPath, "utf8")) ?? {};
    } catch {}
    const merged = { ...previous, ...byEnv };
    mkdirSync(dirname(snapshotPath), { recursive: true });
    const tmp = `${snapshotPath}.tmp-${process.pid}`;
    writeFileSync(tmp, JSON.stringify(merged, null, 2));
    renameSync(tmp, snapshotPath);
    return true;
  } catch {
    return false;
  }
}

// Returns the ids the snapshot added per environment (those not already in
// its include), for the preseed child's retry without them.
export function foldIncludeSnapshot(rc, snapshotPath) {
  const added = {};
  if (!snapshotPath || !existsSync(snapshotPath)) return added;
  let byEnv;
  try {
    byEnv = JSON.parse(readFileSync(snapshotPath, "utf8"));
  } catch {
    return added;
  }
  for (const [name, ids] of Object.entries(byEnv ?? {})) {
    const env = rc.environments?.[name];
    if (!env || !Array.isArray(ids) || ids.length === 0) continue;
    const optimizeDeps = (env.optimizeDeps ??= {});
    const merged = new Set(optimizeDeps.include ?? []);
    const fresh = [];
    for (const id of ids) {
      if (typeof id !== "string" || merged.has(id)) continue;
      merged.add(id);
      fresh.push(id);
    }
    optimizeDeps.include = [...merged];
    if (fresh.length) added[name] = fresh;
  }
  return added;
}
