// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The preseed include extension: dep ids are read from Vite's OWN prior
// _metadata.json (where every committed optimize, including a mid-session
// discovery of plugin-injected deps, persists them), snapshotted to one file,
// and folded into optimizeDeps.include identically by the preseed child and
// the plugin host — Vite's configHash covers `include` sorted and deduped, so
// the child-seeded metadata only validates when both sides apply the same set.
// Replay is bounded by install state: the snapshot records the lockfile hash
// Vite keys its metadata by, and ids from another install state are dropped
// (write, fold and source-metadata alike), because a replayed id that a dep
// bump turned unexportable makes the optimizer THROW rather than warn.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";

const { dropSnapshotEnvironments, foldIncludeSnapshot, priorDepIds, viteLockfileHash, writeIncludeSnapshot } =
  await import(pathToFileURL(join(repo, "crates/oj_server/src/assets/discovered-deps.mjs")).href);

// A root with a deterministic install-state sentinel (bun.lock is in Vite's
// lockfileFormats and needs no node_modules), so the walk stops here instead
// of finding some real lockfile above the tmpdir.
function makeRoot(lockContent = "lock-v1") {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-preseed-"));
  fs.writeFileSync(path.join(dir, "bun.lock"), lockContent);
  return dir;
}

test("prior metadata dep ids round-trip through the snapshot into include", () => {
  const dir = makeRoot();
  const hash = viteLockfileHash(dir);
  fs.mkdirSync(path.join(dir, "deps_ssr"), { recursive: true });
  fs.mkdirSync(path.join(dir, "deps"), { recursive: true });
  // Vite's on-disk metadata persists every committed dep — mid-session
  // discoveries included — under `optimized` (it writes no `discovered`
  // section); `chunks` must never be folded.
  fs.writeFileSync(
    path.join(dir, "deps_ssr", "_metadata.json"),
    JSON.stringify({
      lockfileHash: hash,
      optimized: {
        "react-dom/client": {},
        "unenv/node/process": {},
        "@cloudflare/unenv-preset/node/console": {},
      },
      chunks: { "chunk-ABC123": {} },
    }),
  );
  fs.writeFileSync(path.join(dir, "deps", "_metadata.json"), "not json");

  const rc = {
    root: dir,
    cacheDir: dir,
    environments: {
      ssr: { optimizeDeps: { include: ["react-dom/client", "existing"] } },
      client: {},
    },
  };
  const byEnv = priorDepIds(rc);
  assert.deepEqual(Object.keys(byEnv), ["ssr"], "unreadable metadata is skipped");
  assert.deepEqual(byEnv.ssr, ["@cloudflare/unenv-preset/node/console", "react-dom/client", "unenv/node/process"]);

  const snapshot = path.join(dir, "preseed-include.json");
  writeIncludeSnapshot(snapshot, byEnv, dir);
  const added = foldIncludeSnapshot(rc, snapshot);
  assert.deepEqual(
    [...rc.environments.ssr.optimizeDeps.include].sort(),
    ["@cloudflare/unenv-preset/node/console", "existing", "react-dom/client", "unenv/node/process"],
    "set union, deduped",
  );
  assert.deepEqual(
    added.ssr.sort(),
    ["@cloudflare/unenv-preset/node/console", "unenv/node/process"],
    "the fold reports exactly the ids it added, never the user's own",
  );
  assert.equal(rc.environments.client.optimizeDeps, undefined, "untouched env stays untouched");

  // The HOST folds the same file: an identical rc gets an identical include.
  const rc2 = {
    root: dir,
    cacheDir: dir,
    environments: { ssr: { optimizeDeps: { include: ["react-dom/client", "existing"] } } },
  };
  foldIncludeSnapshot(rc2, snapshot);
  assert.deepEqual(
    [...rc2.environments.ssr.optimizeDeps.include].sort(),
    [...rc.environments.ssr.optimizeDeps.include].sort(),
    "both sides hash the same set",
  );
  fs.rmSync(dir, { recursive: true, force: true });
});

test("a metadata-less environment keeps its previous snapshot entry", () => {
  const dir = makeRoot();
  const snapshot = path.join(dir, "preseed-include.json");
  writeIncludeSnapshot(snapshot, { ssr: ["unenv/node/process"], worker: ["a"] }, dir);
  // A `.vite` wipe: priorDepIds finds nothing for ssr, fresh data for worker.
  writeIncludeSnapshot(snapshot, { worker: ["a", "b"] }, dir);
  const merged = JSON.parse(fs.readFileSync(snapshot, "utf8"));
  assert.deepEqual(merged.environments.ssr, ["unenv/node/process"], "wiped metadata must not clobber the set");
  assert.deepEqual(merged.environments.worker, ["a", "b"], "metadata-bearing env is authoritative");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("ids never cross an install-state change: write discards, fold ignores", () => {
  const dir = makeRoot();
  const snapshot = path.join(dir, "preseed-include.json");
  writeIncludeSnapshot(snapshot, { ssr: ["left-over/from-old-install"] }, dir);

  // The dep bump: the install-state sentinel changes, so the hash changes.
  fs.writeFileSync(path.join(dir, "bun.lock"), "lock-v2");
  const rc = { root: dir, cacheDir: dir, environments: { ssr: { optimizeDeps: { include: ["user"] } } } };
  assert.deepEqual(foldIncludeSnapshot(rc, snapshot), {}, "a stale snapshot folds nothing");
  assert.deepEqual(rc.environments.ssr.optimizeDeps.include, ["user"], "include left exactly as configured");

  // The write under the new state starts from scratch, never merging the old.
  writeIncludeSnapshot(snapshot, { ssr: ["fresh"] }, dir);
  const rewritten = JSON.parse(fs.readFileSync(snapshot, "utf8"));
  assert.deepEqual(rewritten.environments, { ssr: ["fresh"] }, "old-state ids do not leak forward");
  assert.deepEqual(foldIncludeSnapshot(rc, snapshot).ssr, ["fresh"], "the fresh snapshot folds again");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("metadata from another install state contributes no ids", () => {
  const dir = makeRoot();
  fs.mkdirSync(path.join(dir, "deps_ssr"), { recursive: true });
  // Vite would discard this file at its next optimizer init (lockfileHash
  // mismatch); its ids were resolved under an install that no longer exists.
  fs.writeFileSync(
    path.join(dir, "deps_ssr", "_metadata.json"),
    JSON.stringify({ lockfileHash: "00000000", optimized: { "gone/subpath": {} } }),
  );
  const rc = { root: dir, cacheDir: dir, environments: { ssr: {} } };
  assert.deepEqual(priorDepIds(rc), {}, "stale metadata is not a source of replayable ids");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("the pre-gate flat snapshot shape is dropped, not folded", () => {
  const dir = makeRoot();
  const snapshot = path.join(dir, "preseed-include.json");
  // What every snapshot in the wild looked like before the lockfile gate:
  // environment names at the top level, no recorded install state.
  fs.writeFileSync(snapshot, JSON.stringify({ ssr: ["possibly/poisoned"] }));
  const rc = { root: dir, cacheDir: dir, environments: { ssr: {} } };
  assert.deepEqual(foldIncludeSnapshot(rc, snapshot), {}, "unvalidatable ids are not replayed");
  assert.equal(rc.environments.ssr.optimizeDeps, undefined);
  writeIncludeSnapshot(snapshot, { ssr: ["fresh"] }, dir);
  assert.deepEqual(
    JSON.parse(fs.readFileSync(snapshot, "utf8")).environments,
    { ssr: ["fresh"] },
    "the legacy content is replaced wholesale",
  );
  fs.rmSync(dir, { recursive: true, force: true });
});

test("dropSnapshotEnvironments removes only the named environments", () => {
  const dir = makeRoot();
  const snapshot = path.join(dir, "preseed-include.json");
  writeIncludeSnapshot(snapshot, { ssr: ["toxic/id"], worker: ["fine"] }, dir);
  assert.equal(dropSnapshotEnvironments(snapshot, ["ssr"]), true, "a landed drop reports success");
  const healed = JSON.parse(fs.readFileSync(snapshot, "utf8"));
  assert.deepEqual(healed.environments, { worker: ["fine"] });
  const rc = { root: dir, cacheDir: dir, environments: { ssr: {}, worker: {} } };
  const added = foldIncludeSnapshot(rc, snapshot);
  assert.deepEqual(added, { worker: ["fine"] }, "the healed env replays nothing, the healthy one still does");
  // Dropping on a missing or legacy file is a no-op that still reports
  // success: there is nothing there the host could fold either.
  assert.equal(dropSnapshotEnvironments(path.join(dir, "absent.json"), ["ssr"]), true);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("a drop whose write fails reports failure and leaves the file intact", () => {
  const dir = makeRoot();
  const snapdir = path.join(dir, "snap");
  fs.mkdirSync(snapdir);
  const snapshot = path.join(snapdir, "preseed-include.json");
  writeIncludeSnapshot(snapshot, { ssr: ["toxic/id"] }, dir);
  fs.chmodSync(snapdir, 0o555);
  try {
    assert.equal(
      dropSnapshotEnvironments(snapshot, ["ssr"]),
      false,
      "an unwritable snapshot still replays the toxic set: the caller must fail the seed",
    );
    assert.deepEqual(
      JSON.parse(fs.readFileSync(snapshot, "utf8")).environments,
      { ssr: ["toxic/id"] },
      "the file is untouched",
    );
  } finally {
    fs.chmodSync(snapdir, 0o755);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("missing snapshot, empty path, and unknown envs are silent no-ops", () => {
  const rc = { root: "/nonexistent", cacheDir: "/nonexistent", environments: { ssr: {} } };
  assert.deepEqual(priorDepIds(rc), {});
  assert.deepEqual(foldIncludeSnapshot(rc, undefined), {});
  assert.deepEqual(foldIncludeSnapshot(rc, "/nonexistent/preseed-include.json"), {});
  assert.deepEqual(rc.environments, { ssr: {} });
});
