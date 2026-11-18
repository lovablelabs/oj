// SPDX-License-Identifier: MIT

// `oj dev` on a TanStack Start app whose route tree cannot be generated at
// boot stays up, as `vite dev` does: the router plugin logs the generator's
// error and the server runs on whatever tree is on disk (router-plugin's
// router-generator plugin catches `generate()`). oj's own boot generation
// used to end the process instead, so a sandbox that restarted its dev server
// over a broken route file had no server to show the error or pick up the fix.
//
// Five boots, each run under Vite and under oj, and compared:
//   - two route files claim one path, a tree from the last good run on disk:
//     the pages keep rendering from that tree, which is left as it was;
//   - a route file that does not parse, the same tree on disk: every page
//     answers 500 until the file is fixed;
//   - a route file added since that tree was written, which does not parse:
//     the pages the tree names keep rendering, and the new one appears once
//     the file is fixed;
//   - the same added file, broken in a way only the generator minds (an
//     import declared twice: Babel refuses it, a plain parse does not);
//   - the broken file of the second boot and no tree at all: every page
//     answers 500. (With no tree the two part ways once the file is fixed,
//     and neither is held to the other: Vite's pages stay 500 until a
//     restart.)
// In each, the client is refused, in place of one built on the tree left on
// disk: Vite fails the client's load of the tree (Start's route-tree-client
// plugin), oj fails the client bundle with what the generator said, which
// its overlay shows. A route with only `server` options never reaches oj's
// client bundle.
//
// Under oj alone: a fix written while the server is still booting is picked
// up without another save.
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

const SERVER_ONLY = "server-only-handler-marker";
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
  // Server-only: the client's tree leaves it out.
  write(
    "src/routes/api.ts",
    `import { createFileRoute } from "@tanstack/react-router";
export const Route = createFileRoute("/api")({
  server: { handlers: { GET: async () => new Response("${SERVER_ONLY}") } },
});
`,
  );
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  return app;
}

// What breaks the tree, and what mends it. `touch` fires the watcher again
// after the fix: a server answers before its watcher has registered every
// watch (see `settles`).
const BREAKS = {
  // `index.tsx` and `_auth/index.tsx` are both "/".
  conflict: {
    apply(app) {
      const file = path.join(app, "src", "routes", "_auth", "index.tsx");
      fs.mkdirSync(path.dirname(file), { recursive: true });
      fs.writeFileSync(file, routeFile("/_auth/", "conflict-marker"));
    },
    mend: (app) => fs.rmSync(path.join(app, "src", "routes", "_auth"), { recursive: true, force: true }),
    touch: (app) => () => fs.writeFileSync(path.join(app, "src", "routes", "index.tsx"), routeFile("/", "home-marker")),
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
  // The added file with an import declared twice: the generator's transform
  // (Babel) refuses it, so no tree is written, while the file parses.
  redeclared: {
    apply(app) {
      const file = path.join(app, "src", "routes", "contact.tsx");
      const twice = 'import { useState } from "react";\nimport { useState } from "react";\n';
      fs.writeFileSync(file, twice + routeFile("/contact", "contact-marker"));
    },
    mend: (app) => BREAKS.added.mend(app),
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
// Whether the client is refused, in place of one built on the tree left on
// disk. Vite: its request for the tree fails (with the generator's error
// when the routes conflict, "Crawling result not available" when a file
// could not be transformed). oj: the client bundle fails, and no entry is
// served.
async function clientRefused(kind, app) {
  if (kind === "vite") return (await status("/src/routeTree.gen.ts")).status === 500;
  return clientError(app) !== null && (await status("/@oj-start/client-entry.js")).status === 404;
}
// Vite: the client's request for the tree is answered. oj: the entry is
// served and no failed bundle stands.
async function clientServed(kind, app) {
  if (kind === "vite") return (await status("/src/routeTree.gen.ts")).status === 200;
  return clientError(app) === null && (await status("/@oj-start/client-entry.js")).status === 200;
}
// What oj's overlay shows for the failed client bundle.
function clientError(app) {
  const record = path.join(startCache(app), "client-error.json");
  return fs.existsSync(record) ? JSON.parse(fs.readFileSync(record, "utf8")).message : null;
}
function startCache(app) {
  const cache = path.join(app, ".oj-cache");
  return path.join(
    cache,
    fs.readdirSync(cache).find((d) => /^v\d+$/.test(d)),
    "start",
  );
}
// The client chunks oj has written that hold the server-only route's handler.
function serverOnlyChunks(app) {
  const dir = path.join(startCache(app), "client-chunks");
  if (!fs.existsSync(dir)) return [];
  return fs
    .readdirSync(dir, { recursive: true })
    .map(String)
    .filter(
      (f) =>
        fs.statSync(path.join(dir, f)).isFile() && fs.readFileSync(path.join(dir, f), "utf8").includes(SERVER_ONLY),
    );
}
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
    seen.tree = read() === null ? "absent" : read() === before ? "as it was" : "rewritten";
    seen.clientRefused = await settles(() => clientRefused(kind, app), { timeoutMs: 30000 });
    if (kind === "oj") {
      const leaked = serverOnlyChunks(app);
      if (leaked.length)
        throw new Error(`${name}: the server-only route is in the client bundle (${leaked.join(", ")})`);
      // The overlay names what the generator said.
      if (seen.clientRefused && !BREAKS[broken].logged.test(clientError(app) ?? "")) {
        throw new Error(`${name}: the failed client bundle does not carry the generator's error: ${clientError(app)}`);
      }
    }
    if (withTree) {
      BREAKS[broken].mend(app);
      const [route, marker] = BREAKS[broken].mended ?? ["/about", "about-marker"];
      seen.mended = await renders(route, marker, { timeoutMs: 60000, touch: BREAKS[broken].touch?.(app) });
      // The client is served again: the fix reached it, not the pages alone.
      seen.clientBack = await settles(() => clientServed(kind, app), {
        timeoutMs: 60000,
        touch: BREAKS[broken].touch?.(app),
      });
    }
  }
  const out = { name, seen, log: tail(server), app, server };
  return out;
}

// oj alone: the route file is fixed as soon as the generator has failed,
// before the server listens, and never saved again. (Whether the fix reaches
// the server through the watcher, already subscribed by then, or through
// the rebundle worker's first run depends on the machine; either way it
// must be picked up.)
async function fixedWhileBooting() {
  const app = makeApp("oj-added-fixed-while-booting");
  const good = serve("oj", app);
  if (!(await renders("/about", "about-marker"))) throw new Error(`the good boot never rendered:\n${tail(good)}`);
  await stop(good);
  BREAKS.added.apply(app);
  const server = serve("oj", app);
  const failedAt = await settles(() => /route tree generation failed/.test(server.log), {
    timeoutMs: 60000,
    pollMs: 10,
  });
  if (!failedAt) throw new Error(`the boot never said the generation failed:\n${tail(server)}`);
  if (/oj dev \(/.test(server.log))
    throw new Error("the server was already up when the generation failed: nothing to check");
  BREAKS.added.mend(app);
  if (!(await renders("/contact", "contact-marker", { timeoutMs: 90000 }))) {
    throw new Error(`a route file fixed while oj dev booted was never picked up:\n${tail(server)}`);
  }
  await stop(server);
}

const UP = { up: true, logged: true };
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
let failed = false;
// An interrupted run leaves no server and no app behind.
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    for (const server of servers) {
      try {
        process.kill(-server.proc.pid, "SIGKILL");
      } catch {}
    }
    fs.rmSync(tmp, { recursive: true, force: true });
    process.exit(1);
  });
}
try {
  for (const [broken, withTree, want] of [
    [
      "conflict",
      true,
      { ...UP, home: 200, about: 200, tree: "as it was", clientRefused: true, mended: true, clientBack: true },
    ],
    [
      "syntax",
      true,
      { ...UP, home: 500, about: 500, tree: "as it was", clientRefused: true, mended: true, clientBack: true },
    ],
    [
      "added",
      true,
      { ...UP, home: 200, about: 200, tree: "as it was", clientRefused: true, mended: true, clientBack: true },
    ],
    [
      "redeclared",
      true,
      { ...UP, home: 200, about: 200, tree: "as it was", clientRefused: true, mended: true, clientBack: true },
    ],
    ["syntax", false, { ...UP, home: 500, about: 500, tree: "absent", clientRefused: true }],
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
  await fixedWhileBooting();
  console.log("ok: a route file fixed while oj dev booted is picked up without another save");
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
