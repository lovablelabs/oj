// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A TanStack Start build, the build hooks a collecting plugin relies on:
//
// 1. Every environment runs on the same plugin instances: Start's config hook
//    sets Vite's `builder.sharedPlugins`, so state a plugin collects in the
//    client build is still there in the server build. oj loaded the config
//    once per environment, and a plugin that wrote what it collected from its
//    last closeBundle only knew the server half.
// 2. moduleParsed fires for every module the bundler parses, node_modules
//    included, with the resolved importedIds (rolldown's ModuleInfo) and the
//    module's JS (vite:oxc has lowered TS and JSX by then). oj never called it
//    in a Start build. The node_modules checks use the Start packages, which
//    TanStack keeps bundled in ssr (`resolve.noExternal`), as Vite would.
// 3. generateBundle runs for the server environment too, with each chunk's
//    rendered modules. oj ran it for the client and the Cloudflare Worker only.
// 4. Vite's order: configResolved, buildApp pre hooks, then each environment
//    builds and closes (closeBundle before the next one starts), then the
//    buildApp post hooks. A throwing buildApp hook fails the build. oj never
//    called buildApp and closed every environment at the end.
// 5. Off the client `build.emitAssets` is false, and vite:asset's unordered
//    generateBundle deletes the bundle's assets (JS source maps aside). It
//    runs after every `order: "pre"` hook and the enforce:"pre" plugins'
//    unordered ones, so what those emit for the server is dropped and what
//    the rest emit is kept.
// 6. A chunk generateBundle rewrites is written rewritten. oj handed the
//    hook rolldown's getter-only output chunks, so the assignment threw.
// 7. config, configEnvironment and configResolved run on Vite's minimal
//    plugin context (`this.meta` with the Vite and rolldown versions, no
//    `this.environment`), config with the full configEnv, configEnvironment
//    once per environment. oj called them with the plugin as `this`.

import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start-shared-plugins: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-shared-plugins-"));
const out = path.join(tmp, "out");
const config = path.join(tmp, "vite.config.ts");
const report = path.join(tmp, "seen.json");
const events = path.join(tmp, "events.json");
fs.symlinkSync(path.join(app, "node_modules"), path.join(tmp, "node_modules"));
fs.writeFileSync(path.join(tmp, "package.json"), '{"type":"module"}\n');
// The collectors live in one closure: each closeBundle writes what the hooks
// saw, and the post buildApp hook writes the order they ran in.
fs.writeFileSync(
  config,
  `import base from ${JSON.stringify(path.join(app, "vite.config"))};
import { writeFileSync } from "node:fs";

const START_PKG = /\\/node_modules\\/@tanstack\\/(?:react-)?start/;

const collectors = () => {
  const seen: Record<string, Set<string>> = {};
  const add = (hook: string, value: string) => (seen[hook] ??= new Set()).add(value);
  const order: string[] = [];
  let resolved = false;
  const main = {
    name: "shared-collector",
    apply: "build" as const,
    config(this: any, _config: any, env: any) {
      add("config", [!!this.meta?.viteVersion, !!this.meta?.rolldownVersion, env.command, env.isSsrBuild, env.isPreview].join(","));
    },
    configEnvironment(this: any, name: string, _options: any, env: any) {
      add("configEnvironment", name + ":" + !!this.meta?.rolldownVersion + ":" + env.isSsrTargetWebworker);
    },
    configResolved(this: any) {
      resolved = true;
      add("configResolved", String(this.environment === undefined && this.meta?.watchMode === false));
    },
    moduleParsed(info: any) {
      const env = this.environment.name;
      if (START_PKG.test(info.id)) add("moduleParsedStart", env);
      if (
        /\\/src\\/routes\\/__root\\.tsx$/.test(info.id) &&
        info.importedIds.some((d: string) => d.includes("@tanstack/react-router")) &&
        info.code.includes("react/jsx-runtime")
      )
        add("moduleParsedImports", env);
    },
    generateBundle(_o: any, bundle: any) {
      const ids = Object.values(bundle).flatMap((o: any) => (o.type === "chunk" ? Object.keys(o.modules) : []));
      if (ids.some((id) => START_PKG.test(id))) add("generateBundle", this.environment.name);
      for (const o of Object.values(bundle) as any[])
        if (o.type === "chunk" && o.isEntry) o.code = "/* rewritten-" + this.environment.name + " */\\n" + o.code;
      this.emitFile({ type: "asset", fileName: "normal-" + this.environment.name + ".txt", source: "normal" });
    },
    writeBundle() {
      add("writeBundle", this.environment.name);
      order.push("writeBundle:" + this.environment.name);
    },
    closeBundle() {
      order.push("closeBundle:" + this.environment.name);
      const out = Object.fromEntries(Object.entries(seen).map(([k, v]) => [k, [...v].sort()]));
      writeFileSync(${JSON.stringify(report)}, JSON.stringify(out));
    },
    buildApp: { order: "pre" as const, handler: () => void order.push("buildApp:pre resolved=" + resolved) },
  };
  const post = {
    name: "shared-collector-post",
    apply: "build" as const,
    buildApp: {
      order: "post" as const,
      handler(builder: any) {
        const built = Object.values(builder.environments).map((e: any) => e.name + "=" + e.isBuilt);
        order.push("buildApp:post " + built.sort().join(","));
        writeFileSync(${JSON.stringify(events)}, JSON.stringify(order));
        if (process.env.OJ_E2E_BUILDAPP_THROW) this.error("post buildApp refused the build");
      },
    },
  };
  return [main, post];
};

const emitter = (name: string, enforce: "pre" | undefined, order: "pre" | "post" | undefined, file: string) => ({
  name,
  apply: "build" as const,
  ...(enforce ? { enforce } : {}),
  generateBundle: {
    ...(order ? { order } : {}),
    handler(this: any) {
      this.emitFile({ type: "asset", fileName: file + "-" + this.environment.name + ".txt", source: file });
    },
  },
});

const preEmitter = {
  name: "pre-emitter",
  apply: "build" as const,
  enforce: "pre" as const,
  generateBundle(this: any) {
    this.emitFile({ type: "asset", fileName: "pre-" + this.environment.name + ".txt", source: "pre" });
    this.emitFile({ type: "asset", fileName: "pre-" + this.environment.name + ".js.map", source: "{}" });
  },
};

export default {
  ...base,
  plugins: [
    ...(base as any).plugins,
    preEmitter,
    emitter("hook-pre-emitter", undefined, "pre", "hookpre"),
    emitter("pre-late-emitter", "pre", "post", "prepost"),
    ...collectors(),
  ],
};
`,
);

try {
  execSync(`${oj} build ${app} --config ${config} --out ${out}`, { cwd: repo, stdio: "inherit" });
  const seen = JSON.parse(fs.readFileSync(report, "utf8"));
  const both = JSON.stringify(["client", "ssr"]);
  for (const [hook, what] of [
    ["writeBundle", "the plugin instance the last closeBundle ran on"],
    ["moduleParsedStart", "moduleParsed for a node_modules module"],
    ["moduleParsedImports", "moduleParsed with the resolved importedIds and lowered JSX"],
    ["generateBundle", "generateBundle with the rendered node_modules modules"],
  ]) {
    if (JSON.stringify(seen[hook] ?? []) !== both) {
      throw new Error(
        `${what} saw ${JSON.stringify(seen[hook] ?? [])}, not both environments: ${JSON.stringify(seen)}`,
      );
    }
  }

  for (const [hook, want, what] of [
    ["config", ["true,true,build,false,false"], "config's this.meta and configEnv"],
    ["configEnvironment", ["client:true:false", "ssr:true:false"], "configEnvironment's context, per environment"],
    ["configResolved", ["true"], "configResolved's minimal context"],
  ]) {
    if (JSON.stringify(seen[hook] ?? []) !== JSON.stringify(want)) {
      throw new Error(`${what}: saw ${JSON.stringify(seen[hook] ?? [])}, Vite gives ${JSON.stringify(want)}`);
    }
  }

  if (!fs.existsSync(events)) throw new Error("the post buildApp hook never ran");
  const order = JSON.parse(fs.readFileSync(events, "utf8"));
  const want = [
    "buildApp:pre resolved=true",
    "writeBundle:client",
    "closeBundle:client",
    "writeBundle:ssr",
    "closeBundle:ssr",
    "buildApp:post client=true,ssr=true",
  ];
  if (JSON.stringify(order) !== JSON.stringify(want)) {
    throw new Error(`hooks ran as ${JSON.stringify(order)}, Vite's order is ${JSON.stringify(want)}`);
  }

  for (const [file, kept] of [
    ["client/pre-client.txt", true],
    ["client/normal-client.txt", true],
    ["client/hookpre-client.txt", true],
    ["pre-ssr.txt", false],
    ["hookpre-ssr.txt", false],
    ["pre-ssr.js.map", true],
    ["prepost-ssr.txt", true],
    ["normal-ssr.txt", true],
  ]) {
    if (fs.existsSync(path.join(out, file)) !== kept) {
      throw new Error(`${file} should ${kept ? "" : "not "}be in the output (build.emitAssets)`);
    }
  }

  // A chunk generateBundle rewrites ships rewritten (Vite's bundle is mutable).
  const files = (dir) =>
    fs
      .readdirSync(dir, { recursive: true })
      .filter((f) => /\.[cm]?js$/.test(f))
      .map((f) => path.join(dir, f));
  for (const env of ["client", "ssr"]) {
    const marker = "/* rewritten-" + env + " */";
    const pool =
      env === "client"
        ? files(path.join(out, "client"))
        : files(out).filter((f) => !f.includes(`${path.sep}client${path.sep}`));
    if (!pool.some((f) => fs.readFileSync(f, "utf8").startsWith(marker)))
      throw new Error(`no ${env} entry chunk carries the generateBundle rewrite`);
  }

  const failed = (() => {
    try {
      execSync(`${oj} build ${app} --config ${config} --out ${out}`, {
        cwd: repo,
        stdio: "ignore",
        env: { ...process.env, OJ_E2E_BUILDAPP_THROW: "1" },
      });
      return false;
    } catch {
      return true;
    }
  })();
  if (!failed) throw new Error("a throwing buildApp hook did not fail the build");
  console.log(
    "start-shared-plugins: shared instances, moduleParsed, generateBundle, buildApp, emitAssets, config context",
  );
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
