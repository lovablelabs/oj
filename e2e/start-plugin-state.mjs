// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Start dev keeps one client plugin container for the whole session, as Vite
// keeps one per server: buildStart runs once and plugin state survives
// rebundles. The plugin below writes a generated module the client imports
// from buildStart whenever its in-memory copy differs, the way codegen
// plugins do. With a fresh container per rebundle its copy was always empty,
// so every bundle rewrote the file, the watcher fed that write back as the
// next rebundle, and the page reloaded forever. Run with a built
// target/debug/oj (or OJ_BIN).
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 6873;

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start plugin state: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-plugin-state-"));
const app = path.join(tmp, "app");
fs.mkdirSync(app);
for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json", "vite.config.ts"]) {
  fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
}
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");

// The generated module exists before boot with the bytes the plugin writes,
// as a committed codegen output does: every write is a same-bytes rewrite.
const generated = path.join(app, "src", "generated", "state-gen.ts");
const source = `export const stateGen = "state-gen-marker";\n`;
fs.writeFileSync(generated, source);
const aboutFile = path.join(app, "src", "routes", "about.tsx");
fs.writeFileSync(
  aboutFile,
  fs
    .readFileSync(aboutFile, "utf8")
    .replace(
      `import { rootRoute } from "./__root";`,
      `import { rootRoute } from "./__root";\nimport { stateGen } from "../generated/state-gen";`,
    )
    .replace("about-page-marker", "about-page-marker {stateGen}"),
);

// One line per buildStart, outside the app so the log is no watched input.
const starts = path.join(tmp, "build-starts.log");
const configFile = path.join(app, "vite.config.ts");
const config = fs.readFileSync(configFile, "utf8").replace(
  "plugins: [",
  `plugins: [
    (() => {
      let current = "";
      return {
        name: "e2e-generator",
        buildStart() {
          stateFs.appendFileSync(${JSON.stringify(starts)}, "start\\n");
          const next = ${JSON.stringify(source)};
          if (next === current) return;
          current = next;
          stateFs.writeFileSync(${JSON.stringify(generated)}, current);
        },
      };
    })(),`,
);
fs.writeFileSync(configFile, `import * as stateFs from "node:fs";\n${config}`);

let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));

const rebuilds = () => log.split("rebuilding for").length - 1;
const bundles = () => log.split("client bundled").length - 1;
const buildStarts = () => (fs.existsSync(starts) ? fs.readFileSync(starts, "utf8").split("\n").length - 1 : 0);

let failed = false;
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  if (!(await settles(() => log.includes("client bundle cached"), { timeoutMs: 60000 }))) {
    throw new Error(`the boot never cached its client bundle; log tail:\n${log.slice(-3000)}`);
  }

  // Boot settles: each container's first buildStart may write once (the dev
  // host's and the bundler's are separate instances), and then nothing more.
  let quiet = false;
  for (let i = 0; i < 20 && !quiet; i++) {
    const before = rebuilds();
    await sleep(3000);
    quiet = rebuilds() === before;
  }
  if (!quiet) {
    throw new Error(
      `the client keeps rebundling with no edit (${rebuilds()} rebuilds); log tail:\n${log.slice(-3000)}`,
    );
  }
  if (rebuilds() > 2) {
    throw new Error(`boot ran ${rebuilds()} rebuilds before settling; log tail:\n${log.slice(-3000)}`);
  }
  console.log(`plugin state: boot settled after ${rebuilds()} rebuild(s), then stayed quiet`);

  // An edit rebundles once and serves fresh, on the same container: no
  // new buildStart and no generated-file rewrite to feed back.
  const [bundlesBefore, startsBefore] = [bundles(), buildStarts()];
  fs.writeFileSync(aboutFile, fs.readFileSync(aboutFile, "utf8").replace("about-page-marker", "about-page-edited"));
  if (!(await settles(() => bundles() > bundlesBefore, { timeoutMs: 30000 }))) {
    throw new Error(`the route edit did not rebundle; log tail:\n${log.slice(-3000)}`);
  }
  await sleep(4000);
  if (bundles() !== bundlesBefore + 1) {
    throw new Error(`one edit ran ${bundles() - bundlesBefore} bundles; log tail:\n${log.slice(-3000)}`);
  }
  if (buildStarts() !== startsBefore) {
    throw new Error(`the rebundle ran buildStart again (${buildStarts() - startsBefore} more)`);
  }
  console.log("plugin state: an edit rebundled once, buildStart did not run again");
} catch (e) {
  failed = true;
  console.error(`FAIL ${e.message}`);
} finally {
  srv.kill("SIGTERM");
  await new Promise((r) => (srv.exitCode !== null ? r() : srv.once("exit", r)));
  fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
