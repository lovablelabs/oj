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
// the hand-curated store layout is what gets exercised. OJ_TEST_EMBEDDED=1
// (nix.yml) additionally builds with NO env var at all, so the binary's
// compile-time embedded default is what serves — the production nix-install
// path, which env-var inheritance would otherwise keep green by accident.
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
// Specific codes only: npm's advice text mentions the word "network" in
// plenty of non-network failures (ERESOLVE included), so no bare substring.
const NETWORK_ERR = /ENOTFOUND|ETIMEDOUT|ERR_SOCKET_TIMEOUT|ECONNRESET|ECONNREFUSED|ECONNABORTED|EAI_AGAIN|ENETUNREACH|EHOSTUNREACH|EPROTO|ERR_TLS|fetch failed|Bad Gateway|Gateway Timeout|Service Unavailable|Internal Server Error/;
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
    // Pin npm's project root: without a package.json, npm walks up from cwd
    // and can install into an ancestor, leaving the vendor dir empty.
    fs.writeFileSync(path.join(vendor, "package.json"), JSON.stringify({ name: "oj-vendor", private: true }));
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

  // A configured vendor is authoritative: one that holds no rolldown must
  // fail loudly (naming the dir), never silently build with the app's copy —
  // the cache key already recorded the vendor as the builder.
  const brokenVendor = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vendored-rolldown-broken-"));
  scratch.push(brokenVendor);
  const broken = spawnSync(oj, ["build"], {
    cwd: app,
    encoding: "utf8",
    timeout: 300_000,
    env: { ...env, OJ_VENDORED_ROLLDOWN: brokenVendor },
  });
  if (broken.status === 0 || !`${broken.stderr}`.includes(brokenVendor)) {
    console.error(broken.stdout ?? "", broken.stderr ?? "");
    throw new Error("a broken vendor must fail the build naming its path");
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

  // The embedded compile-time default (nix builds): no env var anywhere, the
  // option_env! path pushed by start_script_env is the only route to the
  // vendor. Guards the Rust plumbing that plain env inheritance bypasses.
  if (process.env.OJ_TEST_EMBEDDED) {
    const bare = { ...process.env };
    delete bare.OJ_VENDORED_ROLLDOWN;
    delete bare.OJ_TEST_VENDORED_ROLLDOWN;
    fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
    fs.rmSync(dist, { recursive: true, force: true });
    const embedded = spawnSync(oj, ["build"], { cwd: app, encoding: "utf8", timeout: 300_000, env: bare });
    if (embedded.status !== 0 || !fs.existsSync(dist) || !fs.readdirSync(dist).length) {
      console.error(embedded.stdout ?? "", embedded.stderr ?? "");
      throw new Error("build with no env var must serve from the embedded vendor default");
    }
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
