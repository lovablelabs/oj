// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The preseed include extension: dep ids are read from Vite's OWN prior
// _metadata.json (where every committed optimize, including a mid-session
// discovery of plugin-injected deps, persists them), snapshotted to one file,
// and folded into optimizeDeps.include identically by the preseed child and
// the plugin host — Vite's configHash covers `include` sorted and deduped, so
// the child-seeded metadata only validates when both sides apply the same set.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";

const { foldIncludeSnapshot, priorDepIds, writeIncludeSnapshot } = await import(
  pathToFileURL(join(repo, "crates/oj_server/src/assets/discovered-deps.mjs")).href
);

test("prior metadata dep ids round-trip through the snapshot into include", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-preseed-"));
  fs.mkdirSync(path.join(dir, "deps_ssr"), { recursive: true });
  fs.mkdirSync(path.join(dir, "deps"), { recursive: true });
  // Vite's on-disk metadata persists every committed dep — mid-session
  // discoveries included — under `optimized` (it writes no `discovered`
  // section); `chunks` must never be folded.
  fs.writeFileSync(
    path.join(dir, "deps_ssr", "_metadata.json"),
    JSON.stringify({
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
  writeIncludeSnapshot(snapshot, byEnv);
  foldIncludeSnapshot(rc, snapshot);
  assert.deepEqual(
    [...rc.environments.ssr.optimizeDeps.include].sort(),
    ["@cloudflare/unenv-preset/node/console", "existing", "react-dom/client", "unenv/node/process"],
    "set union, deduped",
  );
  assert.equal(rc.environments.client.optimizeDeps, undefined, "untouched env stays untouched");

  // The HOST folds the same file: an identical rc gets an identical include.
  const rc2 = { cacheDir: dir, environments: { ssr: { optimizeDeps: { include: ["react-dom/client", "existing"] } } } };
  foldIncludeSnapshot(rc2, snapshot);
  assert.deepEqual(
    [...rc2.environments.ssr.optimizeDeps.include].sort(),
    [...rc.environments.ssr.optimizeDeps.include].sort(),
    "both sides hash the same set",
  );
  fs.rmSync(dir, { recursive: true, force: true });
});

test("a metadata-less environment keeps its previous snapshot entry", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-preseed-merge-"));
  const snapshot = path.join(dir, "preseed-include.json");
  writeIncludeSnapshot(snapshot, { ssr: ["unenv/node/process"], worker: ["a"] });
  // A `.vite` wipe: priorDepIds finds nothing for ssr, fresh data for worker.
  writeIncludeSnapshot(snapshot, { worker: ["a", "b"] });
  const merged = JSON.parse(fs.readFileSync(snapshot, "utf8"));
  assert.deepEqual(merged.ssr, ["unenv/node/process"], "wiped metadata must not clobber the set");
  assert.deepEqual(merged.worker, ["a", "b"], "metadata-bearing env is authoritative");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("missing snapshot, empty path, and unknown envs are silent no-ops", () => {
  const rc = { cacheDir: "/nonexistent", environments: { ssr: {} } };
  assert.deepEqual(priorDepIds(rc), {});
  foldIncludeSnapshot(rc, undefined);
  foldIncludeSnapshot(rc, "/nonexistent/preseed-include.json");
  assert.deepEqual(rc.environments, { ssr: {} });
});
