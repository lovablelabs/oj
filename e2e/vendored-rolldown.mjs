// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Old TanStack Start apps (vite <= 7, the Lovable corpus's cluster-2 class:
// ~12% of TSS projects) carry no rolldown anywhere in their tree, and oj's
// Start bundles import it from the app — every build and client bundle died
// with "cannot resolve 'rolldown'" on every oj version. Guards the fallback:
// with OJ_VENDORED_ROLLDOWN pointing at a dir whose node_modules holds the
// pinned rolldown (what the nix build embeds), `oj build` completes; without
// it, the error names the fallback. Run with a built target/debug/oj;
// installs a vite-7 variant of the start-app fixture (skips when offline or
// when the fixture is not installed).
import { execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const fixture = path.join(repo, "e2e", "fixtures", "start-app");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vendored-rolldown-app-"));
const vendor = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vendored-rolldown-dir-"));
const scratch = [app, vendor];
const cleanup = () => {
  for (const d of scratch) fs.rmSync(d, { recursive: true, force: true });
};
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/\/(node_modules|\.oj-cache|dist)(\/|$)/.test(src),
});
const pkgPath = path.join(app, "package.json");
const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8"));
// The cluster-2 shape: a vite that predates rolldown, with the plugin
// generation that matches it.
pkg.devDependencies.vite = "~7.3.0";
pkg.devDependencies["@vitejs/plugin-react"] = "^5";
fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2));
try {
  execSync("npm install --no-audit --no-fund --no-package-lock --loglevel=error", { cwd: app, stdio: "ignore" });
  execSync("npm install rolldown@1.2.1 --no-audit --no-fund --no-package-lock --loglevel=error", {
    cwd: vendor,
    stdio: "ignore",
  });
} catch {
  console.log("SKIP vendored-rolldown: could not install the vite-7 fixture (offline?)");
  cleanup();
  process.exit(0);
}
if (fs.existsSync(path.join(app, "node_modules", "rolldown"))) {
  console.log("SKIP vendored-rolldown: the vite-7 app resolved a rolldown of its own; the gap under test is gone");
  cleanup();
  process.exit(0);
}

// An empty vendor dir (not an unset variable): the runtime variable wins over
// a nix build's compile-time default, so the negative case holds on any
// binary, cargo- or nix-built.
const emptyVendor = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vendored-rolldown-empty-"));
scratch.push(emptyVendor);
const env = { ...process.env, OJ_VENDORED_ROLLDOWN: emptyVendor };
const without = spawnSync(oj, ["build"], { cwd: app, encoding: "utf8", timeout: 300_000, env });
if (without.status === 0 || !`${without.stderr}`.includes("OJ_VENDORED_ROLLDOWN")) {
  console.error(without.stdout ?? "", without.stderr ?? "");
  throw new Error("without a vendor, the build must fail naming OJ_VENDORED_ROLLDOWN");
}

fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
const withVendor = spawnSync(oj, ["build"], {
  cwd: app,
  encoding: "utf8",
  timeout: 300_000,
  env: { ...env, OJ_VENDORED_ROLLDOWN: vendor },
});
if (withVendor.status !== 0) {
  console.error(withVendor.stdout ?? "", withVendor.stderr ?? "");
  throw new Error(`vendored rolldown build failed: status ${withVendor.status}, signal ${withVendor.signal}`);
}
if (!fs.readdirSync(path.join(app, "dist")).length) {
  throw new Error("vendored rolldown build reported success but wrote no dist");
}

cleanup();
console.log("vendored-rolldown: ok");
