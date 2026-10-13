// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// oj bundles Start clients with its own pinned rolldown, the way vite 8
// (rolldown-vite) ships its bundler as its own dependency instead of taking
// whatever the app holds. A configured vendor (OJ_VENDORED_ROLLDOWN, what
// the nix build embeds at compile time) is authoritative: a vendor dir with
// no rolldown must fail the build naming the dir even though the app tree
// carries a copy of its own, never silently build with the app's — the
// cache key already recorded the vendor as the builder. Set-but-empty is
// the explicit opt-out onto the app's copy. Run with a built
// target/debug/oj. The app is the start-app fixture, installed with
// `npm ci` against its own committed lockfile, so registry publishes cannot
// change what this test installs (a floating vite-7 variant of the fixture
// broke overnight when a fresh @tanstack/start-plugin-core dropped vite-7
// peer support; oj keeps no legacy app shape to test against). Skips only
// when npm fails for network reasons.
// OJ_TEST_VENDORED_ROLLDOWN points at a prebuilt vendor dir to use instead
// of npm-installing one — nix.yml passes the flake's own vendor derivation
// so the hand-curated store layout is what gets exercised. OJ_TEST_EMBEDDED=1
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
const rolldownVersion = /rolldownVersion = "([^"]+)"/.exec(fs.readFileSync(path.join(repo, "flake.nix"), "utf8"))?.[1];
if (!rolldownVersion) throw new Error("rolldownVersion not found in flake.nix");

// npm's failure is only a SKIP when it is network-shaped; a resolver or
// peer-dep failure must fail the run, or this guard goes silently green.
// Specific codes only: npm's advice text mentions the word "network" in
// plenty of non-network failures (ERESOLVE included), so no bare substring.
const NETWORK_ERR =
  /ENOTFOUND|ETIMEDOUT|ERR_SOCKET_TIMEOUT|ECONNRESET|ECONNREFUSED|ECONNABORTED|EAI_AGAIN|ENETUNREACH|EHOSTUNREACH|EPROTO|ERR_TLS|fetch failed|Bad Gateway|Gateway Timeout|Service Unavailable|Internal Server Error/;
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
// Deterministic install for the app: the fixture's committed lockfile is the
// resolver, so registry drift cannot change what this test proves. Same skip
// contract as npmInstall.
function npmCi(cwd) {
  try {
    execSync("npm ci --no-audit --no-fund --loglevel=error", {
      cwd,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
    return true;
  } catch (err) {
    const out = `${err?.message ?? ""}\n${err?.stdout ?? ""}\n${err?.stderr ?? ""}`;
    if (NETWORK_ERR.test(out)) {
      console.log("SKIP vendored-rolldown: npm ci failed on a network error");
      return false;
    }
    console.error(out);
    throw new Error("npm ci against the fixture lockfile failed for a non-network reason");
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
  if (!npmCi(app)) return false;
  // The authority phases below only prove something against an app that has
  // a rolldown of its own to fall back to (vite 8 always installs one).
  if (!fs.existsSync(path.join(app, "node_modules", "rolldown"))) {
    throw new Error("the fixture tree carries no rolldown; the vendor-authority phases would prove nothing");
  }

  let vendor = process.env.OJ_TEST_VENDORED_ROLLDOWN && path.resolve(process.env.OJ_TEST_VENDORED_ROLLDOWN);
  if (!vendor) {
    vendor = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vendored-rolldown-dir-"));
    scratch.push(vendor);
    // Pin npm's project root: without a package.json, npm walks up from cwd
    // and can install into an ancestor, leaving the vendor dir empty.
    fs.writeFileSync(path.join(vendor, "package.json"), JSON.stringify({ name: "oj-vendor", private: true }));
    if (!npmInstall(vendor, `rolldown@${rolldownVersion}`)) return false;
  }

  // A configured vendor is authoritative: one that holds no rolldown must
  // fail loudly (naming the dir) even though the app's own copy sits right
  // there — vite 8 never swaps its bundler for the app's either.
  const brokenVendor = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vendored-rolldown-broken-"));
  scratch.push(brokenVendor);
  const broken = spawnSync(oj, ["build"], {
    cwd: app,
    encoding: "utf8",
    timeout: 300_000,
    env: { ...process.env, OJ_VENDORED_ROLLDOWN: brokenVendor },
  });
  if (broken.status === 0 || !`${broken.stderr}`.includes(brokenVendor)) {
    console.error(broken.stdout ?? "", broken.stderr ?? "");
    throw new Error("a broken vendor must fail the build naming its path, not fall back to the app's rolldown");
  }

  const dist = path.join(app, "dist");
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const withVendor = spawnSync(oj, ["build"], {
    cwd: app,
    encoding: "utf8",
    timeout: 300_000,
    env: { ...process.env, OJ_VENDORED_ROLLDOWN: vendor },
  });
  if (withVendor.status !== 0) {
    console.error(withVendor.stdout ?? "", withVendor.stderr ?? "");
    throw new Error(`vendored rolldown build failed: status ${withVendor.status}, signal ${withVendor.signal}`);
  }
  if (!fs.existsSync(dist) || !fs.readdirSync(dist).length) {
    throw new Error("vendored rolldown build reported success but wrote no dist");
  }

  // Set-but-empty is the explicit opt-out onto the app's own copy: it beats
  // a nix build's compile-time default too, so it holds on any binary,
  // cargo- or nix-built.
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.rmSync(dist, { recursive: true, force: true });
  const optOut = spawnSync(oj, ["build"], {
    cwd: app,
    encoding: "utf8",
    timeout: 300_000,
    env: { ...process.env, OJ_VENDORED_ROLLDOWN: "" },
  });
  if (optOut.status !== 0 || !fs.existsSync(dist) || !fs.readdirSync(dist).length) {
    console.error(optOut.stdout ?? "", optOut.stderr ?? "");
    throw new Error("the opt-out build must serve from the app's own rolldown");
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
