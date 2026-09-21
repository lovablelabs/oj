// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { execSync, execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { tmpProject } from "./harness.mjs";

const repo = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const oj = path.join(repo, "target", "debug", "oj");
if (!fs.existsSync(oj)) {
  execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
}

// Node raises its soft fd limit to the hard limit at startup
// (nodejs/node#51072), so Node tooling — TanStack's route generator opens
// hundreds of files concurrently — is written against that raised limit. oj
// hosts those workloads in its own process and must do the same: under
// macOS's default soft cap of 256 a big-app boot died in an EMFILE storm
// that never happened under Vite. The probe holds 300 handles open at once
// from a process started with the soft limit forced to 256.
test("the engine runs under a Node-raised fd limit, not the inherited soft cap", { skip: process.platform === "win32" }, () => {
  const fx = tmpProject({ prefix: "oj-fd-limit-" });
  fx.write("package.json", JSON.stringify({ name: "fd-fx", version: "1.0.0" }));
  fx.write(
    "probe.mjs",
    `import { open } from "node:fs/promises";
const handles = await Promise.all(
  Array.from({ length: 300 }, () => open(${JSON.stringify(path.join(fx.root, "package.json"))})),
);
const count = handles.length;
await Promise.all(handles.map((h) => h.close()));
export default count;
`,
  );
  try {
    const out = execFileSync(
      "/bin/bash",
      ["-c", `ulimit -Sn 256; exec "$1" js-eval "$2" --root "$3"`, "bash", oj, path.join(fx.root, "probe.mjs"), fx.root],
      { encoding: "utf8" },
    );
    assert.match(out, /300/, `300 concurrent open handles under a 256 soft limit: ${out}`);
  } finally {
    fx.cleanup();
  }
});
