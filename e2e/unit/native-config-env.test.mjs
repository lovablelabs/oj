// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { execSync, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { tmpProject } from "./harness.mjs";

const repo = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
if (!process.env.OJ_BIN && !fs.existsSync(oj)) {
  execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
}

// rolldown-vite's native config loader prints a migration-advice warning wall
// (one line per incompatibility in the config graph; 200+ on big monorepos)
// unless VITE_CONFIG_NATIVE_IGNORE_WARNING is set: Vite gates the whole
// compat plugin on `!process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING`
// (config.ts). oj IS the native-loader world, so main() pre-sets the variable
// once and every engine and one-shot child inherits it; a user-set value is
// never overridden.
function envSeenByPlugins(extraEnv, prepare) {
  const fx = tmpProject({ prefix: "oj-native-env-" });
  try {
    fx.write("index.html", '<html><body><script type="module" src="/src/main.js"></script></body></html>');
    fx.write("src/main.js", "export const ok = 1;\n");
    prepare?.(fx);
    const out = path.join(fx.root, "seen.json");
    fx.write(
      "oj.plugins.mjs",
      `import { writeFileSync } from "node:fs";
       export default [{
         name: "env-probe",
         config() {
           writeFileSync(${JSON.stringify(out)}, JSON.stringify({
             ignoreWarning: process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ?? null,
             userAgent: process.env.npm_config_user_agent ?? null,
           }));
         },
       }];\n`,
    );
    const r = spawnSync(oj, ["build", fx.root], {
      encoding: "utf8",
      env: { ...process.env, ...extraEnv },
      timeout: 120_000,
    });
    assert.equal(r.status, 0, `oj build failed:\n${(r.stdout ?? "") + (r.stderr ?? "")}`);
    return JSON.parse(fs.readFileSync(out, "utf8"));
  } finally {
    fx.cleanup();
  }
}

test("children inherit the native config-loader warning suppression", () => {
  const seen = envSeenByPlugins({ VITE_CONFIG_NATIVE_IGNORE_WARNING: undefined });
  assert.equal(seen.ignoreWarning, "true", "main() pre-sets the variable for every child");
});

test("a user-set suppression value is never overridden", () => {
  const seen = envSeenByPlugins({ VITE_CONFIG_NATIVE_IGNORE_WARNING: "0" });
  assert.equal(seen.ignoreWarning, "0", "an explicit value wins over the default");
});

// Vite sorts its lockfile-format preference by npm_config_user_agent; every
// package-manager launch sets it and a bare binary launch does not, which
// reverses the list and lands pnpm apps on the npm entry's mtime-bearing
// hash — one oj's engine computes differently (integer-ms stat). oj presents
// the app's own package manager instead; an already-set agent always wins.
test("oj presents the app's package manager as npm_config_user_agent", () => {
  const seen = envSeenByPlugins({ npm_config_user_agent: undefined }, (fx) =>
    fx.write("pnpm-lock.yaml", "lockfileVersion: 9\n"),
  );
  assert.match(seen.userAgent ?? "", /^pnpm\//, "detected from the app's lockfile");
});

test("the packageManager field's real version is presented", () => {
  // Ecosystem tools version-gate on the agent (yarn classic vs berry is
  // version.startsWith('1.')); the field's version must survive, minus any
  // +sha integrity suffix, and the tail stays a parseable tool/version field.
  const seen = envSeenByPlugins({ npm_config_user_agent: undefined }, (fx) =>
    fx.write("package.json", JSON.stringify({ name: "fx", packageManager: "yarn@1.22.22+sha512.abc" })),
  );
  assert.match(seen.userAgent ?? "", /^yarn\/1\.22\.22 oj\//);
});

test("a bun app that also emits a yarn.lock presents bun", () => {
  const seen = envSeenByPlugins({ npm_config_user_agent: undefined }, (fx) => {
    fx.write("bun.lock", "{}");
    fx.write("yarn.lock", "");
  });
  assert.match(seen.userAgent ?? "", /^bun\//, "bun installs can emit a yarn.lock; the reverse never happens");
});

test("an empty agent counts as unset (Vite treats it as missing)", () => {
  const seen = envSeenByPlugins({ npm_config_user_agent: "" }, (fx) =>
    fx.write("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
  );
  assert.match(seen.userAgent ?? "", /^pnpm\//, "empty must not suppress detection");
});

test("a Berry yarn.lock presents yarn 4, not classic", () => {
  // Berry never dropped yarn.lock; its __metadata: header disambiguates the
  // classic-vs-berry split that version gates care about.
  const seen = envSeenByPlugins({ npm_config_user_agent: undefined }, (fx) =>
    fx.write("yarn.lock", "__metadata:\n  version: 8\n"),
  );
  assert.match(seen.userAgent ?? "", /^yarn\/4\./);
});

test("a real package manager's agent is never overridden", () => {
  const seen = envSeenByPlugins({ npm_config_user_agent: "pnpm/9.12.0 npm/? node/v24.17.0 darwin arm64" }, (fx) =>
    fx.write("package-lock.json", "{}"),
  );
  assert.equal(seen.userAgent, "pnpm/9.12.0 npm/? node/v24.17.0 darwin arm64");
});
