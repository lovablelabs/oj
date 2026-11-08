// SPDX-License-Identifier: MIT

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join as pathJoin, relative as pathRelative, resolve as pathResolve, sep } from "node:path";

import { importPkg } from "./resolve-pkg.mjs";
import { pathToFileURL } from "node:url";
import { startPkg, routerTarget } from "./start-pkg.mjs";

// Runs on oj's embedded engine (`ScriptEngine`): the values that used to be
// spawn env arrive as `env`; `node <script>` still works for tests.
export async function run(env = null) {
  if (env) for (const [k, v] of Object.entries(env)) process.env[k] = v;
  // The node spawn ran with cwd = the app root; cwd-relative plugin filters
  // (svgr's createFilter globs) depend on it.
  if (env && env.OJ_APP_ROOT) {
    try {
      process.chdir(env.OJ_APP_ROOT);
    } catch {}
  }
  return main();
}

async function main() {
  const root = process.env.OJ_APP_ROOT ?? process.cwd();
  const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
  const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";

  process.removeAllListeners("warning");
  process.on("warning", (w) => {
    const m = String(w?.message ?? w);
    if (!/replaceRouteChunk|circular dependency|non-existent property/.test(m)) {
      process.stderr.write(m + "\n");
    }
  });
  const skipped = [];
  const ROUTE_NOISE =
    /does not export a Route|If this file is not intended|Current configuration:|routeFileIgnore|Rename the file|will not be included in the route tree|to be a route/;
  const origConsole = { warn: console.warn, error: console.error, log: console.log, info: console.info };
  const filterRouteNoise =
    (orig) =>
    (...args) => {
      const msg = args.map((a) => (typeof a === "string" ? a : "")).join(" ");
      const m = msg.match(/Route file "([^"]+)" does not export a Route/);
      if (m) {
        skipped.push(m[1]);
        return;
      }
      if (ROUTE_NOISE.test(msg)) return;
      orig(...args);
    };
  for (const k of Object.keys(origConsole)) console[k] = filterRouteNoise(origConsole[k]);

  const { Generator, getConfig } = await importPkg(root, "@tanstack/router-generator", [
    "@tanstack/react-router",
    "@tanstack/react-start",
    "@tanstack/solid-router",
    "@tanstack/solid-start",
  ]);

  // getConfig merges as { ...tsr.config.json, ...inline }, so anything passed
  // inline cannot be configured by the app. Start passes its routes directory
  // and route tree inline (start-plugin-core's parseStartConfig, from
  // tanstackStart's `srcDirectory` and `router`), so tsr.config.json never sets
  // them under Start; oj passes the same two (OJ_ROUTES_DIRECTORY,
  // OJ_GENERATED_ROUTE_TREE). The rest are defaults under the file.
  const START_PATHS = {
    routesDirectory: process.env.OJ_ROUTES_DIRECTORY || pathResolve(root, "src/routes"),
    generatedRouteTree: process.env.OJ_GENERATED_ROUTE_TREE || pathResolve(root, "src/routeTree.gen.ts"),
  };
  const OJ_DEFAULTS = {
    autoCodeSplitting: false,
    routeFileIgnorePattern: "\\.(test|spec|stories|bench)\\.|\\.d\\.ts$",
  };
  const configured = (() => {
    try {
      return JSON.parse(readFileSync(pathJoin(root, "tsr.config.json"), "utf8"));
    } catch {
      return {};
    }
  })();

  // The footer's import is written relative to wherever the tree lives.
  const GENERATED_ROUTE_TREE = START_PATHS.generatedRouteTree;

  // The framework's own Vite plugin appends a `declare module` block naming the
  // router entry and, when there is one, the start entry (start-plugin-core's
  // route-tree-footer); the app's Register interface, and so every typed route, is
  // keyed off it. Without the same block the tree written here is a different
  // file from the one the framework's tooling produces.
  function routerEntry() {
    try {
      const declared = JSON.parse(readFileSync(pathJoin(root, "package.json"), "utf8")).imports?.[
        "#tanstack-router-entry"
      ];
      if (typeof declared === "string") {
        const p = pathResolve(root, declared);
        if (existsSync(p)) return p;
      }
    } catch {}
    for (const ext of [".tsx", ".ts", ".jsx", ".js"]) {
      const p = pathResolve(root, "src/router" + ext);
      if (existsSync(p)) return p;
    }
    return pathResolve(root, "src/router.tsx");
  }

  // start-plugin-core's resolveStartEntryPlan (the same ladder as oj's
  // `#tanstack-start-entry` alias): null = the framework's default entry.
  function startEntry() {
    try {
      const declared = JSON.parse(readFileSync(pathJoin(root, "package.json"), "utf8")).imports?.[
        "#tanstack-start-entry"
      ];
      if (typeof declared === "string") {
        const p = pathResolve(root, declared);
        if (existsSync(p)) return p;
      }
    } catch {}
    for (const ext of [".ts", ".js", ".mts", ".mjs", ".tsx", ".jsx"]) {
      const p = pathResolve(root, "src/start" + ext);
      if (existsSync(p)) return p;
    }
    return null;
  }

  function importPath(abs) {
    const rel = pathRelative(dirname(pathResolve(root, GENERATED_ROUTE_TREE)), abs)
      .split(sep)
      .join("/");
    return rel.startsWith(".") ? rel : "./" + rel;
  }

  function moduleDeclaration() {
    const start = startEntry();
    return [
      `import type { getRouter } from '${importPath(routerEntry())}'`,
      start
        ? `import type { startInstance } from '${importPath(start)}'`
        : `import type { createStart } from '${startPkg()}'`,
      `declare module '${startPkg()}' {`,
      "  interface Register {",
      "    ssr: true",
      "    router: Awaited<ReturnType<typeof getRouter>>",
      ...(start ? ["    config: Awaited<ReturnType<typeof startInstance.getOptions>>"] : []),
      "  }",
      "}",
    ].join("\n");
  }

  const config = getConfig(
    {
      ...Object.fromEntries(Object.entries(OJ_DEFAULTS).filter(([k]) => !(k in configured))),
      ...START_PATHS,
      target: routerTarget(),
      // The declare-module footer the framework's plugin writes; an app that sets
      // routeTreeFileFooter itself keeps its own.
      ...("routeTreeFileFooter" in configured ? {} : { routeTreeFileFooter: [moduleDeclaration()] }),
    },
    root,
  );
  const generator = new Generator({ config, root });
  await generator.run();
  // The client's tree, as Start's `tanstack-start:route-tree-client-plugin`
  // loads it for the client environment: server-only subtrees pruned, no
  // types, header or footer. The client bundles load it in place of the
  // generated tree.
  if (process.env.OJ_CLIENT_ROUTE_TREE) {
    const crawlingResult = await generator.getCrawlingResult();
    if (crawlingResult) {
      const prunedAcc = pruneServerOnlySubtrees(crawlingResult);
      const acc = { ...crawlingResult.acc, ...prunedAcc };
      const { routeTreeContent } = generator.buildRouteTree({
        ...crawlingResult,
        acc,
        config: {
          disableTypes: true,
          enableRouteTreeFormatting: false,
          routeTreeFileHeader: [],
          routeTreeFileFooter: [],
        },
      });
      writeFileSync(process.env.OJ_CLIENT_ROUTE_TREE, routeTreeContent);
    }
  }
  for (const k of Object.keys(origConsole)) console[k] = origConsole[k];

  if (skipped.length) {
    process.stderr.write(
      `${OJ}${_ojTTY ? "" : ":"} route tree generated (${skipped.length} file(s) under src/routes export no Route, skipped)\n`,
    );
  } else {
    process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} route tree generated\n`);
  }
}

// start-plugin-core's pruneServerOnlySubtrees (not in its exports map), as is:
// a route whose only option is `server`, with no child left, leaves the tree.
function pruneServerOnlySubtrees({ rootRouteNode, acc }) {
  const routeNodes = [];
  const routeTree = prune({ ...rootRouteNode, children: acc.routeTree }, routeNodes)?.children || [];
  routeNodes.pop();
  return { routeTree, routeNodes };
}
function prune(node, collectedRouteNodes) {
  const newChildren = [];
  let allChildrenServerOnly = true;
  for (const child of node.children || []) {
    const newChild = prune(child, collectedRouteNodes);
    if (newChild) {
      newChildren.push(newChild);
      allChildrenServerOnly = false;
    }
  }
  if (node.createFileRouteProps?.has("server") && node.createFileRouteProps.size === 1 && allChildrenServerOnly)
    return null;
  collectedRouteNodes.push(node);
  return { ...node, children: newChildren };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
