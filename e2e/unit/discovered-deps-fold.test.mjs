// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The discovered-deps ledger folds into optimizeDeps.include identically in
// the preseed child and the plugin host: Vite's configHash covers `include`
// sorted and deduped (optimizer/index.ts getConfigHash), so both sides
// applying the same SET is what lets the child-seeded metadata validate.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";

const { foldDiscoveredDeps } = await import(
  pathToFileURL(join(repo, "crates/oj_server/src/assets/optimize-env.mjs")).href
);

test("folds ledger ids into include as a set union, per environment", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-ledger-"));
  const ledger = path.join(dir, "discovered-deps.json");
  fs.writeFileSync(
    ledger,
    JSON.stringify({
      ssr: ["unenv/node/process", "react-dom/client", "react-dom/client", 42],
      ghost: ["never-applied"],
    }),
  );
  const rc = {
    environments: {
      ssr: { optimizeDeps: { include: ["react-dom/client", "existing"] } },
      client: {},
    },
  };
  foldDiscoveredDeps(rc, ledger);
  assert.deepEqual(
    [...rc.environments.ssr.optimizeDeps.include].sort(),
    ["existing", "react-dom/client", "unenv/node/process"],
    "union, deduped, non-strings dropped",
  );
  assert.equal(rc.environments.client.optimizeDeps, undefined, "untouched env stays untouched");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("missing file, empty path, and unknown envs are silent no-ops", () => {
  const rc = { environments: { ssr: {} } };
  foldDiscoveredDeps(rc, undefined);
  foldDiscoveredDeps(rc, "/nonexistent/discovered-deps.json");
  assert.deepEqual(rc, { environments: { ssr: {} } });
});
