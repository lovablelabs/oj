// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A TanStack Start app on a vite that predates rolldown (vite <= 7) has no
// rolldown anywhere in its tree, and oj's Start bundles need one. With
// OJ_VENDORED_ROLLDOWN pointing at a dir whose node_modules holds the pinned
// rolldown (what the nix build embeds), `oj build` completes; with the
// variable set but empty (the explicit opt-out), the error names the
// fallback. Run with a built target/debug/oj; installs a vite-7 variant of
// the start-app fixture (skips only when npm fails for network reasons).
// OJ_TEST_VENDORED_ROLLDOWN points at a prebuilt vendor dir to use instead of
// npm-installing one — nix.yml passes the flake's own vendor derivation so
// the hand-curated store layout is what gets exercised.
import { execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
// Absolute: the builds below spawn with cwd inside the tmp app.
const oj = path.resolve(process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj"));
const fixture = path.join(repo, "e2e", "fixtures", "start-app");

// The one version pin lives in flake.nix (rolldownVersion); installing a
// literal here would keep CI proving the old version after a flake bump.
const rolldownVersion = /rolldownVersion = "([^"]+)"/.exec(
  fs.readFileSync(path.join(repo, "flake.nix"), "utf8"),
)?.[1];
if (!rolldownVersion) throw new Error("rolldownVersion not found in flake.nix");

// npm's failure is only a SKIP when it is network-shaped; a resolver or
// peer-dep failure must fail the run, or this guard goes silently green.
const NETWORK_ERR = /ENOTFOUND|ETIMEDOUT|ECONNRESET|ECONNREFUSED|EAI_AGAIN|ENETUNREACH|EHOSTUNREACH|fetch failed|network/i;
function npmInstall(cwd, ...pkgs) {
  try {
    execSync(`npm install ${pkgs.join(" ")} --no-audit --no-fund --no-package-lock --loglevel=error`, {
      cwd,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
    return true;
  } catch (err) {
    const out = `${err?.message ?? ""}\n${err?.stdout ?? ""}\n${err?.stderr ?? ""}`;
    if (NETWORK_ERR.test(out)) {
      console.log("SKIP vendored-rolldown: npm install failed on a network error");
      return false;
    }
    console.error(out);
    throw new Error(`npm install ${pkgs.join(" ")} failed for a non-network reason`);
  }
}

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vendored-rolldown-app-"));
const scratch = [app];
// process.exit inside the try would skip the finally: SKIPs return instead.
function run() {
  fs.cpSync(fixture, app, {
    recursive: true,
    filter: (src) => !/\/(node_modules|\.oj-cache|dist)(\/|$)/.test(src),
  });
  const pkgPath = path.join(app, "package.json");
  const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8"));
  // A vite that predates rolldown, with the plugin generation that matches it.
  pkg.devDependencies.vite = "~7.3.0";
  pkg.devDependencies["@vitejs/plugin-react"] = "^5";
  fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2));
  if (!npmInstall(app)) return false;

  let vendor = process.env.OJ_TEST_VENDORED_ROLLDOWN && path.resolve(process.env.OJ_TEST_VENDORED_ROLLDOWN);
  if (!vendor) {
    vendor = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vendored-rolldown-dir-"));
    scratch.push(vendor);
    if (!npmInstall(vendor, `rolldown@${rolldownVersion}`)) return false;
  }
  if (fs.existsSync(path.join(app, "node_modules", "rolldown"))) {
    console.log("SKIP vendored-rolldown: the vite-7 app resolved a rolldown of its own; the gap under test is gone");
    return false;
  }

  // Set-but-empty is the explicit opt-out: it beats a nix build's
  // compile-time default too, so the negative case holds on any binary,
  // cargo- or nix-built.
  const env = { ...process.env, OJ_VENDORED_ROLLDOWN: "" };
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
  const dist = path.join(app, "dist");
  if (!fs.existsSync(dist) || !fs.readdirSync(dist).length) {
    throw new Error("vendored rolldown build reported success but wrote no dist");
  }
  return true;
}

let ran;
try {
  ran = run();
} finally {
  for (const d of scratch) fs.rmSync(d, { recursive: true, force: true });
}
if (ran) console.log("vendored-rolldown: ok");
