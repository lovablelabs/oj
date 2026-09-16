// SPDX-License-Identifier: MIT

// Build with Nitro instead of oj's Node server. oj runs Nitro's hooks to
// clear the output, then builds the client and SSR service. Nitro's final
// hooks copy public assets, prerender and bundle the server for its preset.
// This module detects Nitro and adds Vite's defaults to its rolldown options.

const NITRO_MAIN = "nitro:main";
const VITE_EXTENSIONS = [".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".json"];

// Check for nitro/vite before config hooks run.
export function declaresNitro(config) {
  return (config?.plugins ?? []).flat(Infinity).some((p) => p?.name === NITRO_MAIN);
}

// Check that the resolved config has Nitro's plugin and environment.
export function usesNitro(config) {
  return declaresNitro(config) && !!config?.environments?.nitro;
}

// Supply the SSR input Nitro needs to register its service.
// Start's config hook normally sets this, but oj skips that hook.
export function ssrServiceSeed(serverEntry) {
  const bundlerOptions = { input: { index: serverEntry } };
  return {
    environments: {
      ssr: { consumer: "server", build: { ssr: true, rollupOptions: bundlerOptions, rolldownOptions: bundlerOptions } },
    },
  };
}

// Convert either Vite alias format to rolldown's object format.
// Skip regex aliases, which rolldown does not support.
function aliasObject(alias) {
  if (!alias) return {};
  if (!Array.isArray(alias)) return { ...alias };
  return Object.fromEntries(
    alias.filter((a) => a && typeof a.find === "string").map((a) => [a.find, a.replacement]),
  );
}

// Give Nitro's buildApp hook a Vite-like builder. Mark oj's environments as
// built so Nitro skips them; Nitro skips other environments as having no
// input. build(env) bundles only the Nitro environment, with oj's plugins
// (from `plugins`, called at build time) ahead of Nitro's.
export function nitroBuilder({ config, build, envDefine, plugins }) {
  const environments = {};
  for (const [name, options] of Object.entries(config.environments ?? {})) {
    const bundler = options?.build?.rolldownOptions ?? options?.build?.rollupOptions;
    if (!["client", "ssr", "nitro"].includes(name) && bundler?.input) {
      throw new Error(`oj: nitro service "${name}" is not supported; oj builds only the client and ssr environments`);
    }
    environments[name] = {
      name,
      config: {
        ...options,
        consumer: options?.consumer ?? (name === "client" ? "client" : "server"),
        // Vite resolves the bundler options to {} when unset, and Nitro reads
        // their input. oj writes client assets to assets/, so Nitro caches that
        // directory as immutable.
        build: { ...options?.build, rollupOptions: bundler ?? {}, rolldownOptions: bundler ?? {}, assetsDir: "assets" },
      },
      isBuilt: name === "client" || name === "ssr",
    };
  }
  return {
    environments,
    async build(env) {
      if (env.name !== "nitro") throw new Error(`oj: nitro asked to build the "${env.name}" environment, which oj builds itself`);
      const ojPlugins = typeof plugins === "function" ? await plugins(env) : plugins;
      const output = await build(nitroRolldownOptions(env.config, config, envDefine, ojPlugins));
      env.isBuilt = true;
      return output;
    },
  };
}

// Add Vite's defaults to Nitro's rolldown options: Node platform, aliases,
// resolve conditions, defines (with import.meta.env), minification and source
// maps. `plugins` (oj's plugin bridge and transforms) go ahead of Nitro's, as
// Vite runs its own and the app's plugins before `rolldownOptions.plugins`.
export function nitroRolldownOptions(envConfig, config, envDefine = {}, plugins = []) {
  const build = envConfig.build ?? {};
  const declared = build.rolldownOptions ?? build.rollupOptions ?? {};
  const { onwarn, output = {}, resolve = {}, transform = {}, plugins: nitroPlugins = [], ...rest } = declared;
  const define = Object.fromEntries(
    Object.entries({ ...envDefine, ...config.define, ...envConfig.define })
      .map(([key, value]) => [key, typeof value === "string" ? value : JSON.stringify(value)])
      // JSON.stringify returns undefined for undefined and functions, which rolldown rejects.
      .filter(([, value]) => typeof value === "string"),
  );
  const conditions = envConfig.resolve?.conditions;
  return {
    platform: "node",
    ...rest,
    plugins: [...[plugins].flat(Infinity), ...[nitroPlugins].flat(Infinity)].filter(Boolean),
    resolve: {
      // Use Vite's default extensions to resolve Nitro's extensionless preset entry.
      extensions: envConfig.resolve?.extensions?.length ? envConfig.resolve.extensions : VITE_EXTENSIONS,
      ...resolve,
      alias: { ...aliasObject(config.resolve?.alias), ...aliasObject(envConfig.resolve?.alias), ...resolve.alias },
      ...(Array.isArray(conditions) && conditions.length ? { conditionNames: conditions } : {}),
    },
    transform: { ...transform, define: { ...define, ...transform.define } },
    onLog(level, log, defaultHandler) {
      if (level === "warn" && onwarn) return onwarn(log, (w) => defaultHandler("warn", w));
      defaultHandler(level, log);
    },
    output: {
      format: "esm",
      // Vite's server default: dead-code elimination only.
      minify: build.minify ? true : "dce-only",
      sourcemap: build.sourcemap,
      ...output,
    },
  };
}
