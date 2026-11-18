// SPDX-License-Identifier: MIT

// `oj dev` on a TanStack Start app whose route tree cannot be generated at
// boot stays up, as `vite dev` does: the router plugin logs the generator's
// error and the server runs on whatever tree is on disk (router-plugin's
// router-generator plugin catches `generate()`). oj's own boot generation
// used to end the process instead, so a sandbox that restarted its dev server
// over a broken route file had no server to show the error or pick up the fix.
//
// Four boots, each run under Vite and under oj, and compared:
//   - two route files claim one path, a tree from the last good run on disk:
//     the pages keep rendering from that tree, which is left as it was;
//   - a route file that does not parse, the same tree on disk: every page
//     answers 500 until the file is fixed;
//   - a route file added since that tree was written, which does not parse:
//     the pages the tree names keep rendering, and the new one appears once
//     the file is fixed;
//   - the broken file of the second boot and no tree at all: every page answers 500. (Under
//     both, the pages keep answering 500 once the file is fixed, until the
//     server restarts: the first tree is written, but the server side has
//     already failed to resolve it. Not checked here.)
// Run with a built target/debug/oj (or OJ_BIN).
import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { rmrf, settles, sleep } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 6921);

if (!fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start boot route errors: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oj-boot-routes-")));

const routeFile = (route, marker) => `import { createFileRoute } from "@tanstack/react-router";
export const Route = createFileRoute("${route}")({ component: Page });
function Page() {
  return (
    <main>
      <h1>${marker}</h1>
    </main>
  );
}
`;

// A file-routed Start app, on the fixture's installed packages.
function makeApp(name) {
  const app = path.join(tmp, name);
  const write = (file, content) => {
    const full = path.join(app, file);
    fs.mkdirSync(path.dirname(full), { recursive: true });
    fs.writeFileSync(full, content);
  };
  const installed = JSON.parse(fs.readFileSync(path.join(fixture, "package.json"), "utf8"));
  const pick = (from, names) => Object.fromEntries(names.map((n) => [n, from[n]]));
  write(
    "package.json",
    JSON.stringify({
      name,
      private: true,
      type: "module",
      dependencies: pick(installed.dependencies, [
        "@tanstack/react-router",
        "@tanstack/react-start",
        "react",
        "react-dom",
      ]),
      devDependencies: pick(installed.devDependencies, ["@vitejs/plugin-react", "vite"]),
    }),
  );
  write(
    "tsconfig.json",
    JSON.stringify({
      compilerOptions: { jsx: "react-jsx", module: "ESNext", moduleResolution: "Bundler", target: "ES2022" },
    }),
  );
  write(
    "vite.config.ts",
    `import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
export default defineConfig({ plugins: [tanstackStart(), react()] });
`,
  );
  write(
    "src/router.tsx",
    `import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";
export function getRouter() {
  return createRouter({ routeTree });
}
`,
  );
  write(
    "src/routes/__root.tsx",
    `import { HeadContent, Outlet, Scripts, createRootRoute } from "@tanstack/react-router";
export const Route = createRootRoute({ component: Root });
function Root() {
  return (
    <html>
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
`,
  );
  write("src/routes/index.tsx", routeFile("/", "home-marker"));
  write("src/routes/about.tsx", routeFile("/about", "about-marker"));
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  return app;
}

// What breaks the tree, and what mends it.
const BREAKS = {
  // `index.tsx` and `_auth/index.tsx` are both "/".
  conflict: {
    apply(app) {
      const file = path.join(app, "src", "routes", "_auth", "index.tsx");
      fs.mkdirSync(path.dirname(file), { recursive: true });
      fs.writeFileSync(file, routeFile("/_auth/", "conflict-marker"));
    },
    mend: (app) => fs.rmSync(path.join(app, "src", "routes", "_auth"), { recursive: true }),
    logged: /Conflicting configuration paths were found/,
  },
  // An unclosed element.
  syntax: {
    apply(app) {
      const file = path.join(app, "src", "routes", "about.tsx");
      fs.writeFileSync(file, routeFile("/about", "about-marker").replace("<main>", "<main><div>"));
    },
    mend: (app) => fs.writeFileSync(path.join(app, "src", "routes", "about.tsx"), routeFile("/about", "about-marker")),
    touch: (app) => () => BREAKS.syntax.mend(app),
    logged: /Error transforming route file .*about\.tsx/,
  },
  // A route file added since the tree was written, with the same error:
  // the tree does not name it, so the pages it does name still render, and
  // the new page appears once the file parses (the tree is regenerated,
  // though the set of route files did not change).
  added: {
    apply(app) {
      const file = path.join(app, "src", "routes", "contact.tsx");
      fs.writeFileSync(file, routeFile("/contact", "contact-marker").replace("<main>", "<main><div>"));
    },
    mend: (app) =>
      fs.writeFileSync(path.join(app, "src", "routes", "contact.tsx"), routeFile("/contact", "contact-marker")),
    touch: (app) => () => BREAKS.added.mend(app),
    logged: /Error transforming route file .*contact\.tsx/,
    mended: ["/contact", "contact-marker"],
  },
};

const servers = [];
function serve(kind, app) {
  const [cmd, args] =
    kind === "vite"
      ? [path.join(app, "node_modules", ".bin", "vite"), ["dev", "--port", String(PORT), "--strictPort"]]
      : [oj, ["dev", app, "--port", String(PORT)]];
  const proc = spawn(cmd, args, {
    cwd: app,
    stdio: ["ignore", "pipe", "pipe"],
    detached: true,
    // No editor flush gate: nothing here flushes.
    env: { ...process.env, LOVABLE_DEV_SERVER: "0", OJ_HMR_GATE: "0" },
  });
  const server = { proc, log: "" };
  proc.stdout.on("data", (c) => (server.log += c));
  proc.stderr.on("data", (c) => (server.log += c));
  servers.push(server);
  return server;
}
async function stop(server) {
  if (server.proc.exitCode !== null || server.proc.signalCode !== null) return;
  const exited = new Promise((r) => server.proc.once("exit", r));
  try {
    process.kill(-server.proc.pid, "SIGKILL");
  } catch {}
  await exited;
}
const status = async (route) => {
  try {
    const res = await fetch(`http://localhost:${PORT}${route}`, { signal: AbortSignal.timeout(30000) });
    const body = await res.text();
    return { status: res.status, body };
  } catch {
    return { status: 0, body: "" };
  }
};
// Answering at all, with any status; false once the process has exited.
const answers = (server, timeoutMs) =>
  settles(async () => server.proc.exitCode === null && (await status("/")).status !== 0, {
    timeoutMs,
    pollMs: 250,
  });
// `touch` writes the fix again on the way: a server answers before its
// watcher has registered every watch (see `settles`).
const renders = (route, marker, { timeoutMs = 90000, touch } = {}) =>
  settles(
    async () => {
      const got = await status(route);
      return got.status === 200 && got.body.includes(marker);
    },
    { timeoutMs, pollMs: 250, touch },
  );
const tail = (server) => server.log.replace(/\x1b\[[0-9;]*m/g, "").slice(-2500);

// One boot over a broken tree: what the server did with it.
async function boot(kind, broken, withTree) {
  const name = `${kind}-${broken}-${withTree ? "tree" : "no-tree"}`;
  const app = makeApp(name);
  const tree = path.join(app, "src", "routeTree.gen.ts");
  const read = () => (fs.existsSync(tree) ? fs.readFileSync(tree, "utf8") : null);
  if (withTree) {
    // A good run leaves its tree.
    const good = serve(kind, app);
    if (!(await renders("/about", "about-marker")))
      throw new Error(`${name}: the good boot never rendered:\n${tail(good)}`);
    await stop(good);
    if (read() === null) throw new Error(`${name}: the good boot left no route tree`);
  }
  const before = read();
  BREAKS[broken].apply(app);
  const server = serve(kind, app);
  const seen = { up: await answers(server, 90000) };
  if (seen.up) {
    // The generator's error has been printed once the server has had its say.
    seen.logged = await settles(() => BREAKS[broken].logged.test(server.log), { timeoutMs: 30000 });
    seen.home = (await status("/")).status;
    seen.about = (await status("/about")).status;
    seen.alive = server.proc.exitCode === null;
    seen.tree = read() === null ? "absent" : read() === before ? "as it was" : "rewritten";
    if (withTree) {
      BREAKS[broken].mend(app);
      const [route, marker] = BREAKS[broken].mended ?? ["/about", "about-marker"];
      seen.mended = await renders(route, marker, { timeoutMs: 60000, touch: BREAKS[broken].touch?.(app) });
    }
  }
  const out = { name, seen, log: tail(server), app, server };
  return out;
}

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
let failed = false;
try {
  for (const [broken, withTree, want] of [
    ["conflict", true, { up: true, logged: true, home: 200, about: 200, alive: true, tree: "as it was", mended: true }],
    ["syntax", true, { up: true, logged: true, home: 500, about: 500, alive: true, tree: "as it was", mended: true }],
    ["added", true, { up: true, logged: true, home: 200, about: 200, alive: true, tree: "as it was", mended: true }],
    ["syntax", false, { up: true, logged: true, home: 500, about: 500, alive: true, tree: "absent" }],
  ]) {
    const what = `${broken}, ${withTree ? "a tree on disk" : "no tree on disk"}`;
    const vite = await boot("vite", broken, withTree);
    await stop(vite.server);
    // The premise: what this checks oj against is what Vite does.
    if (!same(vite.seen, want)) {
      throw new Error(
        `${what}: vite dev no longer does what this test holds oj to\n` +
          `want ${JSON.stringify(want)}\ngot  ${JSON.stringify(vite.seen)}\n--- vite log tail\n${vite.log}`,
      );
    }
    const got = await boot("oj", broken, withTree);
    if (!same(got.seen, vite.seen)) {
      throw new Error(
        `${what}: oj dev differs from vite dev\n` +
          `vite ${JSON.stringify(vite.seen)}\noj   ${JSON.stringify(got.seen)}\n--- oj log tail\n${got.log}`,
      );
    }
    await stop(got.server);
    console.log(`ok: ${what}: oj dev stays up as vite dev does ${JSON.stringify(got.seen)}`);
  }
  console.log("\nSTART BOOT ROUTE ERRORS PASSED");
} catch (e) {
  failed = true;
  console.error("\nSTART BOOT ROUTE ERRORS FAILED:", e.message);
} finally {
  for (const server of servers) await stop(server);
  await sleep(200);
  await rmrf(tmp);
}
process.exit(failed ? 1 : 0);
