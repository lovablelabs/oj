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
//
// Replayed ids never outlive the install state they were discovered under.
// Vite cannot be poisoned by stale dep ids because it never carries them
// forward: its metadata is keyed by a lockfile hash and discarded wholesale
// on mismatch, and a fresh session re-discovers from live imports. This
// replay re-RESOLVES old ids, where a dep bump can turn one toxic: an include
// id whose package still exists but whose subpath its exports map no longer
// answers makes the optimizer THROW (only unresolvable ids warn), and since
// a committed optimize is the one thing that ages an id out, the failure is
// self-sustaining. So the snapshot records the same lockfile hash Vite keys
// its metadata by, and every reader applies Vite's own rule: ids from another
// install state are dropped, costing one discovery session — exactly what
// Vite pays after a dependency change. A merely removed dep under the SAME
// install state still costs only Vite's "present in optimizeDeps.include"
// warning and ages out at the next metadata-bearing boot.
// Expected shape: the boot after a discovery session re-optimizes ONCE
// (in the child — the grown include changes configHash), then the set reaches
// its fixed point and every later boot validates.

import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

// Vite's lockfileFormats (optimizer/index.ts): the package managers' INSTALL
// STATE files, not the repo lockfiles, so a manual install without a lockfile
// edit still changes the hash. The user-agent sort and the parent-directory
// walk are Vite's own (lookupFile), so this hash flips exactly when Vite's
// does — a workspace's sentinel lives at the monorepo root, not the app's.
const LOCKFILE_FORMATS = [
  { path: "node_modules/.pnpm/lock.yaml", checkPatchesDir: false, manager: "pnpm" },
  { path: "node_modules/.package-lock.json", checkPatchesDir: "patches", manager: "npm" },
  { path: "node_modules/.yarn-state.yml", checkPatchesDir: false, manager: "yarn" },
  { path: "bun.lock", checkPatchesDir: "patches", manager: "bun" },
  { path: ".rush/temp/shrinkwrap-deps.json", checkPatchesDir: false, manager: "pnpm" },
  { path: "aube-lock.yaml", checkPatchesDir: false, manager: "aube" },
  { path: "nub.lock", checkPatchesDir: "patches", manager: "nub" },
  { path: ".pnp.cjs", checkPatchesDir: ".yarn/patches", manager: "yarn" },
  { path: ".pnp.js", checkPatchesDir: ".yarn/patches", manager: "yarn" },
  { path: "node_modules/.yarn-integrity", checkPatchesDir: "patches", manager: "yarn" },
  { path: "bun.lockb", checkPatchesDir: "patches", manager: "bun" },
].sort((_, { manager }) => (process.env.npm_config_user_agent?.startsWith(manager) ? 1 : -1));

function tryStat(p) {
  try {
    return statSync(p);
  } catch {
    return undefined;
  }
}

// Vite's getLockfileHash + getHash, matched in VALUE (sha256 hex cut to 8),
// not just in change detection: priorDepIds compares the metadata's own
// `lockfileHash` field against it. If a future Vite changes its hash, every
// comparison turns into a mismatch and the replay degrades to one discovery
// boot per session — never to replaying unvalidated ids.
export function viteLockfileHash(root) {
  let lockfilePath;
  let dir = root;
  walk: while (dir) {
    for (const { path } of LOCKFILE_FORMATS) {
      const full = join(dir, path);
      if (tryStat(full)?.isFile()) {
        lockfilePath = full;
        break walk;
      }
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  let content = "";
  if (lockfilePath) {
    try {
      content = readFileSync(lockfilePath, "utf8");
    } catch {}
    const normalized = lockfilePath.replaceAll("\\", "/");
    const format = LOCKFILE_FORMATS.find((f) => normalized.endsWith(f.path));
    if (format?.checkPatchesDir) {
      const baseDir = lockfilePath.slice(0, -format.path.length);
      const stat = tryStat(join(baseDir, format.checkPatchesDir));
      if (stat?.isDirectory()) content += stat.mtimeMs.toString();
    }
  }
  return createHash("sha256").update(content).digest("hex").substring(0, 8);
}

// The dep ids of every environment's LAST committed optimize, read from
// Vite's own metadata — but only metadata Vite itself would still serve: a
// file whose `lockfileHash` is not the current one is a leftover from another
// install state (Vite discards it at its next optimizer init), and ids
// resolved back then may not resolve at all now.
export function priorDepIds(rc) {
  const out = {};
  const current = viteLockfileHash(rc.root);
  for (const name of Object.keys(rc.environments ?? {})) {
    const dir = join(rc.cacheDir, name === "client" ? "deps" : `deps_${name}`);
    let metadata;
    try {
      metadata = JSON.parse(readFileSync(join(dir, "_metadata.json"), "utf8"));
    } catch {
      continue;
    }
    if (metadata?.lockfileHash !== current) continue;
    // On-disk metadata carries every committed dep — including mid-session
    // discoveries — under `optimized` (stringifyDepsOptimizerMetadata writes
    // no `discovered` section). Capped deterministically: sorted, so the
    // include set is stable across boots even past the cap.
    const ids = new Set(Object.keys(metadata?.optimized ?? {}));
    if (ids.size > 0) out[name] = [...ids].sort().slice(0, 500);
  }
  return out;
}

// {lockfile, environments} with a tolerant reader: the pre-gate flat shape
// (environment names at the top level, no recorded install state) reads as
// unvalidatable and is dropped, flushing any poison already in the wild at
// the cost of one discovery boot.
function readSnapshot(snapshotPath) {
  try {
    const raw = JSON.parse(readFileSync(snapshotPath, "utf8"));
    if (raw && typeof raw === "object" && raw.environments && typeof raw.environments === "object") {
      return { lockfile: typeof raw.lockfile === "string" ? raw.lockfile : "", environments: raw.environments };
    }
  } catch {}
  return undefined;
}

function writeSnapshot(snapshotPath, snapshot) {
  mkdirSync(dirname(snapshotPath), { recursive: true });
  const tmp = `${snapshotPath}.tmp-${process.pid}`;
  writeFileSync(tmp, JSON.stringify(snapshot, null, 2));
  renameSync(tmp, snapshotPath);
}

// Written by the preseed child BEFORE it resolves its config (atomic rename;
// the host may already be reading a previous snapshot).
// MERGED per environment, never replaced wholesale: an environment with
// metadata on disk is authoritative (its dep set is the fixed-point superset,
// and replacement is what lets removed deps age out), but an environment with
// NO metadata keeps its previous snapshot entry — otherwise a `.vite` wipe
// with a surviving `.oj-cache` (a node_modules reinstall) would clobber the
// accumulated set with nothing and bring back the discovery-restart double
// boot for exactly the case this exists for. The merge only spans ONE install
// state: a previous snapshot under another lockfile hash is dropped, never
// merged, so its ids cannot leak into the current one.
export function writeIncludeSnapshot(snapshotPath, byEnv, root) {
  try {
    const lockfile = viteLockfileHash(root);
    const previous = readSnapshot(snapshotPath);
    const kept = previous && previous.lockfile === lockfile ? previous.environments : {};
    writeSnapshot(snapshotPath, { lockfile, environments: { ...kept, ...byEnv } });
  } catch {}
}

// Folds the snapshot's ids into each environment's optimizeDeps.include and
// returns what it added per environment ({name: [ids]}), so the caller can
// tell a replayed id from a user-configured one when an optimize fails. A
// snapshot from another install state folds nothing: Vite would not serve
// metadata across that boundary, so oj does not replay ids across it.
export function foldIncludeSnapshot(rc, snapshotPath) {
  const added = {};
  if (!snapshotPath || !existsSync(snapshotPath)) return added;
  const snapshot = readSnapshot(snapshotPath);
  if (!snapshot || snapshot.lockfile !== viteLockfileHash(rc.root)) return added;
  for (const [name, ids] of Object.entries(snapshot.environments)) {
    const env = rc.environments?.[name];
    if (!env || !Array.isArray(ids) || ids.length === 0) continue;
    const optimizeDeps = (env.optimizeDeps ??= {});
    const merged = new Set(optimizeDeps.include ?? []);
    const news = [];
    for (const id of ids) {
      if (typeof id !== "string" || merged.has(id)) continue;
      merged.add(id);
      news.push(id);
    }
    optimizeDeps.include = [...merged];
    if (news.length > 0) added[name] = news;
  }
  return added;
}

// The self-heal half: an environment whose optimize FAILED with replayed ids
// folded in drops its snapshot entry, so neither this child's retry nor the
// host (which folds the same file, strictly after the child exits) replays
// the set that just proved toxic. The entry is rebuilt from scratch at the
// next committed optimize's metadata.
export function dropSnapshotEnvironments(snapshotPath, names) {
  try {
    const snapshot = readSnapshot(snapshotPath);
    if (!snapshot) return;
    for (const name of names) delete snapshot.environments[name];
    writeSnapshot(snapshotPath, snapshot);
  } catch {}
}
