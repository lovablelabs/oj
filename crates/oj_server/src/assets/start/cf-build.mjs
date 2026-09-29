// SPDX-License-Identifier: MIT

// The Cloudflare production build. When the app's Vite config carries
// @cloudflare/vite-plugin, `vite build` does not produce a Node server: the
// plugin's `config` hook declares one Vite environment per Worker (name from
// the wrangler config, or `viteEnvironment.name`), bundled for workerd with
// `platform: "neutral"`, the `cloudflare:*` modules (and, under nodejs_compat,
// the natively provided `node:*` ones) left external, everything else inlined
// (`noExternal: true`), and its output-config plugin writes the deployable
// `wrangler.json` plus `.wrangler/deploy/config.json` from `generateBundle` /
// `writeBundle`. oj drives those same hooks through its plugin container; this
// module reads the environment the plugin declared and supplies the rolldown
// side of it (externals, node built-ins, the Worker entry).

import { builtinModules } from "node:module";
import { isAbsolute, relative, resolve } from "node:path";

const WORKER_ENTRY = "virtual:cloudflare/worker-entry";
const USER_ENTRY = "virtual:cloudflare/user-entry";
const NODE_BUILTIN = new RegExp(`^(?:node:.+|${builtinModules.filter((m) => !m.startsWith("_")).join("|")})$`);
const NODE_POLYFILL = /^(?:unenv\/|@cloudflare\/unenv-preset\/)/;
// TanStack Start's default Worker entry (`main: "@tanstack/react-start/server-entry"`),
// which oj replaces with its own server entry the way it does for the Node build.
const START_DEFAULT_ENTRY = /[\\/]@tanstack[\\/]react-start[\\/]dist[\\/]default-entry[\\/]/;

// The Worker environment the Cloudflare plugin declared in `config.environments`,
// or null when the plugin is not part of this config. With several Workers the
// entry Worker (the one whose build writes the Vite manifest) wins.
export function cloudflareEnvironment(config) {
  const environments = config?.environments;
  if (!environments || typeof environments !== "object") return null;
  const workers = Object.entries(environments).filter(([name, options]) => {
    if (name === "client" || !options) return false;
    const builtins = options.resolve?.builtins;
    return (
      (Array.isArray(builtins) && builtins.includes("cloudflare:workers")) ||
      options.build?.rolldownOptions?.input?.index === WORKER_ENTRY
    );
  });
  if (!workers.length) return null;
  const [name, options] = workers.find(([, o]) => o.build?.manifest === true) ?? workers[0];
  const builtins = (options.resolve?.builtins ?? []).filter((b) => typeof b === "string" || b instanceof RegExp);
  return {
    name,
    options,
    builtins,
    // The plugin's `resolve.conditions`, then the module-graph conditions rolldown adds.
    conditions: Array.isArray(options.resolve?.conditions)
      ? options.resolve.conditions.filter((c) => !c.includes("|"))
      : ["workerd", "worker", "module", "browser"],
    target: typeof options.build?.target === "string" ? options.build.target : undefined,
    // Rolldown plugins the plugin's `configEnvironment` attached (Vite's
    // esmExternalRequirePlugin turns `require("node:x")` of externals into imports).
    rolldownPlugins: (options.build?.rolldownOptions?.plugins ?? []).flat(Infinity).filter(Boolean),
    nodejsCompat: builtins.some((b) => b === "node:buffer" || b === "buffer"),
    outDir: typeof options.build?.outDir === "string" ? options.build.outDir : null,
  };
}

// Where the Worker files go: the environment's `build.outDir` (root-relative, as
// the plugin resolves it), kept at the same place under oj's output root when
// `--out` moved that root.
export function workerOutDir(env, app, dist, config) {
  const rootOut = resolve(app, config?.build?.outDir ?? "dist");
  const declared = resolve(app, env.outDir ?? relative(app, resolve(rootOut, env.name)));
  const rel = relative(rootOut, declared);
  return rel && !rel.startsWith("..") && !isAbsolute(rel) ? resolve(dist, rel) : declared;
}

const isBuiltin = (builtins, id) => builtins.some((b) => (b instanceof RegExp ? b.test(id) : b === id));

// The rolldown plugin for the Worker bundle. It routes what Vite's resolver and
// the Cloudflare plugin settle between them: `cloudflare:*` and the runtime's
// node built-ins stay external, the other `node:*` imports become the unenv
// polyfills the plugin's nodejs-compat hook picks, and the plugin's virtual
// Worker entry wraps oj's server entry.
export function cloudflareWorkerPlugin({ container, env, serverEntry }) {
  const builtins = env.builtins;
  return {
    name: "oj-cloudflare-worker",
    resolveId: {
      filter: {
        id: {
          include: [
            /^cloudflare:/,
            NODE_BUILTIN,
            NODE_POLYFILL,
            new RegExp(`^${USER_ENTRY.replace(/[/:]/g, "\\$&")}$`),
          ],
        },
      },
      async handler(source, importer) {
        if (source === USER_ENTRY) {
          // The plugin resolves the wrangler `main`; Start's default entry becomes oj's.
          const main = await container.resolveId(source, importer);
          if (!main) throw new Error("could not resolve the Worker's main entry (wrangler config `main`)");
          return START_DEFAULT_ENTRY.test(main) ? serverEntry : main;
        }
        if (source.startsWith("cloudflare:")) return { id: source, external: true };
        const r = await container.resolveIdResult(source, importer);
        if (r) return r.external ? { id: r.id, external: true } : r.id;
        if (isBuiltin(builtins, source)) return { id: source, external: true };
        return null;
      },
    },
  };
}

export const CLOUDFLARE_WORKER_ENTRY = WORKER_ENTRY;
