// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Start's client bundles apply the app's `resolve.alias` the way Vite does:
// its alias plugin (@rollup/plugin-alias over config.resolve.alias,
// plugins/index.ts) runs ahead of every app plugin and the core resolver, for
// every importer. A tsconfig-paths plugin covers only the files its tsconfig
// includes, so an `@/x` import from a legacy `.jsx` file outside `include`
// resolves only through the alias. Without it the client bundle left the
// specifier bare, the browser threw "Failed to resolve module specifier" and
// the page hydrated to nothing, while SSR rendered fine.

import { spawn, spawnSync, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5402);

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start app alias: fixture deps not installed");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const rm = (p) => fs.rmSync(p, { recursive: true, force: true });
const filesUnder = (dir, ext) => {
  if (!fs.existsSync(dir)) return [];
  return fs
    .readdirSync(dir, { withFileTypes: true, recursive: true })
    .filter((e) => e.isFile() && e.name.endsWith(ext))
    .map((e) => fs.readFileSync(path.join(e.parentPath ?? e.path, e.name), "utf8"));
};
const carries = (dir, text) => filesUnder(dir, ".js").some((c) => c.includes(text));

const configFile = path.join(app, "vite.config.ts");
const originalConfig = fs.readFileSync(configFile, "utf8");
const aboutFile = path.join(app, "src", "routes", "about.tsx");
const originalAbout = fs.readFileSync(aboutFile, "utf8");
const legacyDir = path.join(app, "src", "alias-legacy");
const targetFile = path.join(app, "src", "lib", "alias-target.js");
const dist = path.join(app, "dist");

// Both Vite alias forms: the object shape a shared config returns
// (`"@": <abs>/src`) and an array entry with a RegExp `find` and `$1`.
const aliasedConfig = originalConfig.replace(
  "export default defineConfig({",
  `export default defineConfig({
  resolve: {
    alias: [
      { find: "@", replacement: \`\${process.cwd()}/src\` },
      { find: /^~legacy\\/(.*)$/, replacement: \`\${process.cwd()}/src/alias-legacy/$1\` },
    ],
  },`,
);
must(aliasedConfig !== originalConfig, "fixture vite.config.ts no longer has the expected defineConfig shape");

const about = `import { createRoute } from "@tanstack/react-router";
import { tsxMark } from "@/lib/alias-target";
import { LegacyAlias } from "~legacy/LegacyAlias.jsx";

import { rootRoute } from "./__root";

export const aboutRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/about",
  component: About,
});

function About() {
  return (
    <main>
      <h1 className="fixture-heading">about-page-marker</h1>
      <p data-mark="tsx">{tsxMark}</p>
      <LegacyAlias />
    </main>
  );
}
`;

let srv;
let log = "";
try {
  fs.writeFileSync(configFile, aliasedConfig);
  fs.writeFileSync(aboutFile, about);
  fs.mkdirSync(legacyDir, { recursive: true });
  // A legacy .jsx file the fixture's tsconfig never includes.
  fs.writeFileSync(
    path.join(legacyDir, "LegacyAlias.jsx"),
    `import { jsxMark } from "@/lib/alias-target";\nexport function LegacyAlias() {\n  return <p data-mark="jsx">{jsxMark}</p>;\n}\n`,
  );
  fs.writeFileSync(targetFile, `export const tsxMark = "alias-tsx-mark";\nexport const jsxMark = "alias-jsx-mark";\n`);
  rm(path.join(app, ".oj-cache"));

  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);

  let html = "";
  await settles(async () => {
    try {
      html = await (await fetch(`http://localhost:${PORT}/about`)).text();
      return html.includes("about-page-marker");
    } catch {
      return false;
    }
  });
  must(
    html.includes("alias-tsx-mark") && html.includes("alias-jsx-mark"),
    `SSR did not render through the alias:\n${html.slice(0, 1200)}\n---\n${log.slice(-1500)}`,
  );
  console.log("start dev: SSR resolves resolve.alias");

  const chunks = path.join(app, ".oj-cache");
  await settles(async () => carries(chunks, "alias-jsx-mark"), { timeoutMs: 15000 });
  must(carries(chunks, "alias-tsx-mark"), "the dev client bundle left the .tsx `@/` import unresolved");
  must(carries(chunks, "alias-jsx-mark"), "the dev client bundle left the .jsx `@/` import unresolved");
  must(!/UNRESOLVED_IMPORT/.test(log), `the dev client bundle reported an unresolved import:\n${log.slice(-2000)}`);
  console.log("start dev: client bundle resolves resolve.alias from .tsx and .jsx");
  srv.kill("SIGKILL");
  srv = null;

  rm(dist);
  const built = spawnSync(oj, ["build", app], { encoding: "utf8" });
  const buildLog = `${built.stdout}\n${built.stderr}`;
  must(built.status === 0, `oj build failed:\n${buildLog.slice(-2000)}`);
  must(!/UNRESOLVED_IMPORT/.test(buildLog), `the build reported an unresolved import:\n${buildLog.slice(-2000)}`);
  const client = path.join(dist, "client");
  must(carries(client, "alias-tsx-mark"), "the production client bundle left the .tsx `@/` import unresolved");
  must(carries(client, "alias-jsx-mark"), "the production client bundle left the .jsx `@/` import unresolved");
  console.log("start build: client bundle resolves resolve.alias");

  console.log("\nSTART APP ALIAS PASSED");
} catch (e) {
  console.error("\nSTART APP ALIAS FAILED:", e.message);
  if (log) console.error(`--- server log tail ---\n${log.slice(-2000)}`);
  process.exitCode = 1;
} finally {
  srv?.kill("SIGKILL");
  fs.writeFileSync(configFile, originalConfig);
  fs.writeFileSync(aboutFile, originalAbout);
  rm(legacyDir);
  rm(targetFile);
  rm(dist);
}
