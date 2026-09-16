// SPDX-License-Identifier: MIT

import { importPkg, makeResolver } from "./resolve-pkg.mjs";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve as pathResolve } from "node:path";
import { fileURLToPath } from "node:url";

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";

// Vite's DEFAULT_CONFIG_FILES order (constants.ts): the first present wins.
const CONFIG_FILES = [
  "vite.config.js", "vite.config.mjs", "vite.config.ts", "vite.config.cjs", "vite.config.mts", "vite.config.cts",
  "oj.config.ts", "oj.config.js", "oj.config.mjs",
];

const hookHandler = (h) => (typeof h === "function" ? h : typeof h?.handler === "function" ? h.handler : null);
// Vite fails a module whose plugin hook throws, naming the plugin; the loader
// surfaces this as the request's error instead of silently skipping the plugin.
// Framework plugins oj reimplements (vite:*, tanstack*) run here without the
// lifecycle oj replaced (their generators are never initialized), so their
// failures stay skipped as before; only user plugins fail the module.
function pluginError(e, plugin, id) {
  const err = e instanceof Error ? e : new Error((e && e.message) || String(e));
  if (err.ojDecorated) return err;
  const loc = err.loc && typeof err.loc === "object" ? err.loc : null;
  const where = (loc && loc.file) || err.id || id || "";
  const line = loc ? loc.line : err.line;
  err.message = `[plugin:${plugin?.name || "unknown"}] ${err.message}` +
    (where ? `\n${where}${line != null ? `:${line}` : ""}` : "") +
    (typeof err.frame === "string" && err.frame ? `\n${err.frame}` : "");
  err.ojDecorated = true;
  return err;
}
const hookFilter = (h) => (typeof h === "object" && h ? h.filter : undefined);

const ojReimplemented = (name = "") =>
  name.startsWith("vite:") || /^tanstack[-:]/.test(name) || name.startsWith("@tanstack/");

// Vite's Environment passed to applyToEnvironment carries `config.consumer`
// ("client"/"server"); @vitejs/plugin-react reads it directly, so the bare
// `{ name }` oj used to pass threw and (in the client-bundle process, which has
// no rejection guard) killed the build. An async applyToEnvironment can't be
// awaited here, so a thenable is treated as "allowed".
function envConsumer(environment) {
  return environment === "client" ? "client" : "server";
}
function envAllows(plugin, environment) {
  const f = plugin.applyToEnvironment;
  if (typeof f !== "function") return true;
  const env = { name: environment, config: { consumer: envConsumer(environment) } };
  try {
    const r = f(env);
    if (r && typeof r.then === "function") return true;
    return r !== false;
  } catch {
    return true;
  }
}

// Vite's pluginFilter: a string id filter is a picomatch glob joined to cwd (unless
// it starts with `**` or is absolute); a RegExp is tested on the slash-normalized id.
const slash = (p) => p.replace(/\\/g, "/");
function globToRegExpSource(glob) {
  let re = "";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") {
        i++;
        if (glob[i + 1] === "/") { i++; re += "(?:.*/)?"; } else re += ".*";
      } else re += "[^/]*";
    } else if (c === "?") re += "[^/]";
    else if (c === "{") {
      const j = glob.indexOf("}", i);
      if (j > i) { re += "(?:" + glob.slice(i + 1, j).split(",").map(globToRegExpSource).join("|") + ")"; i = j; }
      else re += "\\{";
    } else if (c === "[") {
      const j = glob.indexOf("]", i);
      if (j > i) { let cls = glob.slice(i + 1, j); if (cls[0] === "!") cls = "^" + cls.slice(1); re += "[" + cls + "]"; i = j; }
      else re += "\\[";
    } else if (c === "\\" && i + 1 < glob.length) re += "\\" + glob[++i];
    else re += c.replace(/[.+^$()|]/g, "\\$&");
  }
  return re;
}
const idGlobCache = new Map();
function matchOne(pat, id) {
  if (pat instanceof RegExp) { const r = pat.test(slash(id)); pat.lastIndex = 0; return r; }
  if (typeof pat !== "string") return false;
  let re = idGlobCache.get(pat);
  if (!re) {
    const glob = pat.startsWith("**") || pat.startsWith("/") ? slash(pat) : slash(join(process.env.OJ_APP_ROOT ?? process.cwd(), pat));
    re = new RegExp("^" + globToRegExpSource(glob) + "$");
    idGlobCache.set(pat, re);
  }
  return re.test(slash(id));
}
function idAllowed(filter, id) {
  if (!filter) return true;
  const f = filter.id ?? filter;
  const inc = f && typeof f === "object" && !(f instanceof RegExp) && !Array.isArray(f) ? f.include : f;
  const exc = f && typeof f === "object" && !(f instanceof RegExp) && !Array.isArray(f) ? f.exclude : undefined;
  const asArr = (x) => (x == null ? [] : Array.isArray(x) ? x : [x]);
  const incs = asArr(inc), excs = asArr(exc);
  if (excs.some((p) => matchOne(p, id))) return false;
  if (incs.length === 0) return true;
  return incs.some((p) => matchOne(p, id));
}

// Vite's transform `filter.code`: a string is a substring, a RegExp is tested
// (stateful flags reset). Unlike `filter.id`, never a path glob.
function codeAllowed(filter, code) {
  const f = filter?.code;
  if (f == null) return true;
  const one = (pat) => {
    if (pat instanceof RegExp) { const r = pat.test(code); pat.lastIndex = 0; return r; }
    return typeof pat === "string" && code.includes(pat);
  };
  const plain = f instanceof RegExp || typeof f === "string" || Array.isArray(f);
  const asArr = (x) => (x == null ? [] : Array.isArray(x) ? x : [x]);
  const incs = asArr(plain ? f : f.include), excs = asArr(plain ? undefined : f.exclude);
  if (excs.some(one)) return false;
  return incs.length === 0 || incs.some(one);
}

// Vite's getSortedPluginHooks: a hook's own `order` ("pre" | "post") ranks it
// before or after the normal band, stable within a band (so `enforce` order holds).
function byHook(plugins, name) {
  const rank = (p) => { const h = p[name]; return h?.order === "pre" ? -1 : h?.order === "post" ? 1 : 0; };
  return [...plugins].sort((a, b) => rank(a) - rank(b));
}

function applyMatches(plugin, command, mode) {
  const a = plugin.apply;
  if (!a) return true;
  if (typeof a === "function") {
    try { return !!a({}, { command, mode }); } catch { return true; }
  }
  return a === command;
}

function ordered(plugins) {
  const pre = [], normal = [], post = [];
  for (const p of plugins) {
    if (p.enforce === "pre") pre.push(p);
    else if (p.enforce === "post") post.push(p);
    else normal.push(p);
  }
  return [...pre, ...normal, ...post];
}

function makeConfigResolver(config) {
  const root = config.root ?? process.cwd();
  const aliases = Array.isArray(config.resolve?.alias)
    ? config.resolve.alias.map(({ find, replacement }) => [find, replacement])
    : Object.entries(config.resolve?.alias ?? {});
  const extensions = Array.isArray(config.resolve?.extensions) && config.resolve.extensions.length
    ? config.resolve.extensions
    : [".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".cjs", ".cts", ".json"];
  const resolvePackage = makeResolver(root);

  return () => async (id, importer) => {
    if (!id || id.startsWith("\0") || id.startsWith("/@")) return undefined;
    let specifier = id.split("?", 1)[0];
    for (const [find, replacement] of aliases) {
      if (typeof replacement !== "string") continue;
      if (find instanceof RegExp) {
        find.lastIndex = 0;
        if (find.test(specifier)) { specifier = specifier.replace(find, replacement); break; }
      } else if (typeof find === "string") {
        const prefix = find.endsWith("/") ? find.slice(0, -1) : find;
        if (specifier === prefix) { specifier = replacement; break; }
        if (specifier.startsWith(`${prefix}/`)) {
          specifier = join(replacement, specifier.slice(prefix.length + 1));
          break;
        }
      }
    }

    if (specifier.startsWith(".") || isAbsolute(specifier)) {
      const base = importer ? dirname(importer.startsWith("file://") ? fileURLToPath(importer) : importer) : root;
      const path = isAbsolute(specifier) ? specifier : pathResolve(base, specifier);
      const stat = statSync(path, { throwIfNoEntry: false });
      if (stat?.isFile()) return path;
      for (const extension of extensions) {
        const candidate = stat?.isDirectory() ? join(path, `index${extension}`) : `${path}${extension}`;
        if (statSync(candidate, { throwIfNoEntry: false })?.isFile()) return candidate;
      }
      return undefined;
    }

    try { return resolvePackage(specifier); } catch {}
    // An export reachable only under the "import" condition (a package's ESM-only
    // subpath); this module lives in the app's cache dir, so Node's ESM resolver
    // walks the app's node_modules from here.
    try {
      const url = import.meta.resolve(specifier);
      if (url.startsWith("file:")) return fileURLToPath(url);
    } catch {}
    return undefined;
  };
}

const HERE = dirname(fileURLToPath(import.meta.url));

async function withBuildStartLock(fn) {
  const lock = join(HERE, "buildstart.lock");
  for (;;) {
    try { mkdirSync(lock); break; }
    catch {
      let pid = 0;
      try { pid = Number(readFileSync(join(lock, "holder"), "utf8")); } catch {}
      let alive = false;
      if (pid) { try { process.kill(pid, 0); alive = true; } catch {} }
      let expired = false;
      try { expired = Date.now() - statSync(lock).mtimeMs > 300_000; } catch {}
      if ((pid && !alive) || expired) { try { rmSync(lock, { recursive: true, force: true }); } catch {} }
      else await new Promise((r) => setTimeout(r, 50));
    }
  }
  try { writeFileSync(join(lock, "holder"), String(process.pid)); } catch {}
  try { return await fn(); }
  finally { try { rmSync(lock, { recursive: true, force: true }); } catch {} }
}

export function findConfig(app) {
  for (const f of CONFIG_FILES) {
    const p = join(app, f);
    if (existsSync(p)) return p;
  }
  return null;
}

export function createPluginContainer(vite, allPlugins, {
  command = "serve", mode = command === "build" ? "production" : "development", environment = "client", config = {},
} = {}) {
  const plugins = ordered(
    allPlugins.filter(
      (p) => (p.buildStart || p.resolveId || p.load || p.transform || p.moduleParsed || p.generateBundle || p.configResolved || p.renderChunk || p.writeBundle || p.closeBundle || p.buildEnd || p.renderStart)
        && applyMatches(p, command, mode),
    ),
  );

  const parse = typeof vite?.parseAst === "function"
    ? (code, opts) => vite.parseAst(code, opts)
    : () => ({});

  // Vite exposes this.environment.config with `consumer` ("client" | "server")
  // and `command` on every hook context; plugins branch on them (e.g. return an
  // empty dev manifest when consumer !== "server"). Without config, a plugin
  // reading `this.environment.config.consumer` throws, the hook is swallowed,
  // and oj serves an export-less stub — surfacing downstream as an undefined
  // import. Mirror Vite's shape so those plugins take their intended branch.
  const consumer = environment === "client" ? "client" : "server";
  const watchFiles = new Set();
  const moduleInfo = new Map();
  const resolvedConfig = {
    root: process.cwd(),
    base: "/",
    build: {},
    server: {},
    resolve: {},
    define: {},
    optimizeDeps: {},
    ssr: {},
    env: {},
    experimental: {},
    environments: { client: {}, ssr: {} },
    ...config,
    command,
    mode,
    plugins: allPlugins,
    isProduction: mode === "production",
  };
  // Vite's resolveConfig runs resolveEnvironmentResolveOptions for EVERY
  // environment, so each `environments[name].resolve` is a complete object
  // (Vite's configDefaults.resolve: external:[], noExternal:[], dedupe:[],
  // alias:[], extensions, plus consumer-based conditions/mainFields). Plugins
  // read it in configResolved -- e.g. @cloudflare/vite-plugin's
  // validateWorkerEnvironmentOptions inspects
  // `environments[worker].resolve.external` and throws when `resolve` is
  // undefined. Mirror Vite: layer the top-level resolve over the structural
  // defaults for every environment (the env's own resolve wins). The actual SSR
  // resolution conditions are applied by oj's resolver, not read from here, so
  // the array defaults are the structural shape plugins inspect.
  for (const name of Object.keys(resolvedConfig.environments)) {
    const envc = resolvedConfig.environments[name] ?? {};
    envc.resolve = {
      conditions: [],
      mainFields: [],
      extensions: [],
      dedupe: [],
      noExternal: [],
      external: [],
      externalConditions: [],
      alias: [],
      preserveSymlinks: false,
      ...(resolvedConfig.resolve ?? {}),
      ...(envc.resolve ?? {}),
    };
    resolvedConfig.environments[name] = envc;
  }
  // Vite resolves `build.outDir` ("dist") and an absolute `publicDir` ("" when
  // disabled); build plugins compute output paths from both.
  resolvedConfig.build = { outDir: "dist", ...resolvedConfig.build };
  if (typeof resolvedConfig.publicDir !== "string" || !isAbsolute(resolvedConfig.publicDir)) {
    resolvedConfig.publicDir = resolvedConfig.publicDir === false
      ? ""
      : pathResolve(resolvedConfig.root, typeof resolvedConfig.publicDir === "string" ? resolvedConfig.publicDir : "public");
  }
  // Vite's config.createResolver, for plugins that resolve through the config
  // (aliases, extensions, packages) rather than through this.resolve.
  if (typeof resolvedConfig.createResolver !== "function") {
    resolvedConfig.createResolver = makeConfigResolver(resolvedConfig);
  }
  // this.environment.config is the environment's resolved config: the top-level
  // options with `environments[name]` (build, resolve, define, ...) layered on.
  const environmentOptions = resolvedConfig.environments?.[environment] ?? {};
  const environmentConfig = {
    ...resolvedConfig,
    ...environmentOptions,
    build: { ...resolvedConfig.build, ...environmentOptions.build },
    resolve: { ...resolvedConfig.resolve, ...environmentOptions.resolve },
    define: { ...resolvedConfig.define, ...environmentOptions.define },
    consumer,
  };
  const environmentModules = new Map();
  const modulesByUrl = new Map();
  const modulesByFile = new Map();
  const moduleGraph = {
    environment,
    idToModuleMap: environmentModules,
    urlToModuleMap: modulesByUrl,
    fileToModulesMap: modulesByFile,
    getModuleById(id) { return environmentModules.get(id); },
    async getModuleByUrl(url) { return modulesByUrl.get(url); },
    getModulesByFile(file) { return modulesByFile.get(file); },
  };

  function trackEnvironmentModule(id, code) {
    let node = environmentModules.get(id);
    if (!node) {
      const file = id.split("?")[0];
      node = {
        id,
        url: id,
        file,
        environment,
        type: file.endsWith(".css") ? "css" : "js",
        importedModules: new Set(),
        importers: new Set(),
      };
      environmentModules.set(id, node);
      modulesByUrl.set(id, node);
      if (!modulesByFile.has(file)) modulesByFile.set(file, new Set());
      modulesByFile.get(file).add(node);
    }
    node.transformResult = { code, map: null };
    return node;
  }

  const ctx = {
    environment: {
      name: environment,
      mode: command === "build" ? "build" : "dev",
      config: environmentConfig,
      moduleGraph,
    },
    meta: pluginMeta(vite, command),
    warn() {}, info() {}, debug() {},
    error(m) { throw new Error(typeof m === "string" ? m : m?.message ?? String(m)); },
    emitFile() { return "oj-emit-ref"; },
    setAssetSource() {}, getFileName() { return ""; },
    addWatchFile(id) { watchFiles.add(String(id)); }, getWatchFiles() { return [...watchFiles]; },
    getModuleInfo(id) { return moduleInfo.get(id) ?? null; },
    getModuleIds() { return moduleInfo.keys(); },
    async resolve() { return null; },
    async load(options) {
      const id = typeof options === "string" ? options : options.id;
      const code = await load(id);
      return code == null ? null : { id, code };
    },
    parse,
  };

  let initialization;
  function initializePlugins() {
    initialization ??= (async () => {
      for (const plugin of byHook(plugins, "configResolved")) {
        if (ojReimplemented(plugin.name)) continue;
        const hook = hookHandler(plugin.configResolved);
        if (!hook) continue;
        try { await hook.call(ctx, resolvedConfig); }
        catch (error) {
          process.stderr.write(`oj: plugin "${plugin.name || "?"}" configResolved failed (skipped): ${error?.message ?? error}\n`);
        }
      }
    })();
    return initialization;
  }

  // Vite's this.resolve runs the whole pipeline, ending in file and package
  // resolution; a build plugin that hands back the path of a real module (a
  // polyfill, the worker entry) needs that tail. The dev loader resolves files
  // itself, so it keeps the plugins-only answer.
  const fileResolver = command === "build" ? resolvedConfig.createResolver() : null;
  function pluginContext(plugin, base = ctx, ambientSkipCalls = null) {
    return Object.assign(Object.create(base), {
      async resolve(source, importer, options = {}) {
        // Vite's cumulative `skipCalls` (see plugin-host.mjs ctx.resolve): a
        // skip covering only the direct caller lets two plugins that both
        // this.resolve the same id with skipSelf re-enter each other forever.
        let skipCalls = ambientSkipCalls;
        if (options.skipSelf !== false) {
          const prior = ambientSkipCalls ? ambientSkipCalls.findIndex((c) => c.id === source && c.importer === importer && c.plugin === plugin) : -1;
          if (prior !== -1) {
            skipCalls = ambientSkipCalls.slice();
            skipCalls[prior] = { ...skipCalls[prior], called: true };
          } else {
            skipCalls = ambientSkipCalls ? [...ambientSkipCalls, { id: source, importer, plugin }] : [{ id: source, importer, plugin }];
          }
        }
        const resolved = await resolveIdResult(source, importer, skipCalls);
        if (resolved) return resolved;
        if (!fileResolver || source.startsWith("\0") || /^[a-z]+:/i.test(source) && !isAbsolute(source)) return null;
        try {
          const file = await fileResolver(source, importer);
          return file ? { id: file } : null;
        } catch {
          return null;
        }
      },
    });
  }

  // The full resolveId answer ({ id, external }); resolveId keeps the id-only form.
  async function resolveIdResult(id, importer, skipCalls) {
    await initializePlugins();
    for (const p of byHook(plugins, "resolveId")) {
      // Vite merges skipCalls into the skip set: skipped on the same
      // id + importer, or outright once re-entered (`called`).
      if (skipCalls && skipCalls.some((c) => c.plugin === p && (c.called || (c.id === id && c.importer === importer)))) continue;
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.resolveId);
      if (!h || !idAllowed(hookFilter(p.resolveId), id)) continue;
      let r;
      try { r = await h.call(pluginContext(p, ctx, skipCalls), id, importer, { isEntry: false, ssr: environment === "ssr" }); } catch (e) { if (ojReimplemented(p.name)) continue; throw pluginError(e, p, importer || id); }
      if (r != null) return typeof r === "string" ? { id: r } : { id: r.id, external: r.external };
    }
    return null;
  }

  async function resolveId(id, importer, skipCalls) {
    const r = await resolveIdResult(id, importer, skipCalls);
    return r ? r.id : null;
  }

  async function load(id) {
    await initializePlugins();
    for (const p of byHook(plugins, "load")) {
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.load);
      if (!h || !idAllowed(hookFilter(p.load), id)) continue;
      let r;
      try { r = await h.call(pluginContext(p), id, { ssr: environment === "ssr" }); } catch (e) { if (ojReimplemented(p.name)) continue; throw pluginError(e, p, id); }
      if (r != null) {
        const code = typeof r === "string" ? r : r.code;
        moduleInfo.set(id, { id, code, importedIds: [], meta: {} });
        trackEnvironmentModule(id, code);
        return code;
      }
    }
    return null;
  }

  async function moduleParsed(id, code) {
    const info = { id, code, importedIds: [], meta: {} };
    moduleInfo.set(id, info);
    for (const plugin of byHook(plugins, "moduleParsed")) {
      const handler = hookHandler(plugin.moduleParsed);
      if (handler && envAllows(plugin, environment)) await handler.call(ctx, info);
    }
  }

  async function transform(code, id) {
    await initializePlugins();
    moduleInfo.set(id, { id, code, importedIds: [], meta: {} });
    trackEnvironmentModule(id, code);
    let current = code, changed = false;
    for (const p of byHook(plugins, "transform")) {
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.transform);
      const filter = hookFilter(p.transform);
      if (!h || !idAllowed(filter, id) || !codeAllowed(filter, current)) continue;
      let r;
      try { r = await h.call(pluginContext(p), current, id, { ssr: environment === "ssr" }); } catch (e) { if (ojReimplemented(p.name)) continue; throw pluginError(e, p, id); }
      const next = r == null ? null : typeof r === "string" ? r : r.code;
      if (next != null) { current = next; changed = true; }
    }
    trackEnvironmentModule(id, current);
    await moduleParsed(id, current);
    return changed ? current : null;
  }

  async function transformUserCode(code, id) {
    await initializePlugins();
    moduleInfo.set(id, { id, code, importedIds: [], meta: {} });
    trackEnvironmentModule(id, code);
    const ssr = environment === "ssr";
    let current = code, changed = false;
    for (const p of byHook(plugins, "transform")) {
      if (ojReimplemented(p.name) || !envAllows(p, environment)) continue;
      const h = hookHandler(p.transform);
      const filter = hookFilter(p.transform);
      if (!h || !idAllowed(filter, id) || !codeAllowed(filter, current)) continue;
      let r;
      try { r = await h.call(pluginContext(p), current, id, { ssr }); } catch (e) { throw pluginError(e, p, id); }
      const next = r == null ? null : typeof r === "string" ? r : r.code;
      if (next != null) { current = next; changed = true; }
    }
    trackEnvironmentModule(id, current);
    await moduleParsed(id, current);
    return changed ? current : null;
  }

  async function renderChunk(code, chunk) {
    let current = code, changed = false;
    for (const p of plugins) {
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.renderChunk);
      if (!h) continue;
      let result;
      try { result = await h.call(ctx, current, chunk, { format: "es" }); } catch { continue; }
      const next = typeof result === "string" ? result : result?.code;
      if (next != null) { current = next; changed = true; }
    }
    return changed ? current : null;
  }

  async function generateBundle(emit, bundle = {}) {
    await initializePlugins();
    const genCtx = { ...ctx, emitFile: (f) => (emit(f), "oj-emit-ref") };
    for (const p of byHook(plugins, "generateBundle")) {
      const h = hookHandler(p.generateBundle);
      if (!h || !envAllows(p, environment)) continue;
      try { await h.call(pluginContext(p, genCtx), { format: "es" }, bundle, false); } catch {}
    }
  }

  async function writeBundle(bundle) {
    for (const p of plugins) {
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.writeBundle);
      if (!h) continue;
      try { await h.call(ctx, { format: "es" }, bundle, true); } catch {}
    }
  }

  async function closeBundle() {
    for (const plugin of plugins) {
      if (!envAllows(plugin, environment)) continue;
      const hook = hookHandler(plugin.closeBundle);
      if (!hook) continue;
      try { await hook.call(ctx); } catch {}
    }
  }

  async function buildEnd(error) {
    for (const plugin of plugins) {
      if (!envAllows(plugin, environment)) continue;
      const hook = hookHandler(plugin.buildEnd);
      if (!hook) continue;
      try { await hook.call(ctx, error); } catch {}
    }
  }

  async function renderStart(outputOptions, inputOptions) {
    for (const plugin of plugins) {
      if (!envAllows(plugin, environment)) continue;
      const hook = hookHandler(plugin.renderStart);
      if (!hook) continue;
      try { await hook.call(ctx, outputOptions, inputOptions); } catch {}
    }
  }

  // Vite runs buildStart once before any module loads; plugins that compile
  // sources (e.g. i18n message compilers) populate their state here and serve
  // it from load(). oj's SSR loader is a separate process with its own plugin
  // instances, so it must run buildStart itself. Scoped to non-reimplemented
  // (user) plugins, like transformUserCode, so framework plugins oj already
  // handles don't newly execute a hook they never ran under oj before.
  let buildStarted = false;
  async function buildStart() {
    if (buildStarted) return;
    await initializePlugins();
    if (buildStarted) return;
    buildStarted = true;
    await withBuildStartLock(async () => {
      for (const p of byHook(plugins, "buildStart")) {
        if (ojReimplemented(p.name) || !envAllows(p, environment)) continue;
        const h = hookHandler(p.buildStart);
        if (!h) continue;
        // Degrade gracefully: oj's plugin context is minimal (e.g. a stubbed
        // this.resolve), so some plugins' buildStart throw here though they'd
        // succeed under full Vite. Log and continue rather than abort the whole
        // dev server — a plugin that genuinely needed buildStart will surface as
        // its own load() output being wrong, which is strictly better than one
        // unsupported plugin taking down every other plugin's startup.
        try { await h.call(pluginContext(p), {}); }
        catch (e) {
          process.stderr.write(`oj: plugin "${p.name || "?"}" buildStart failed (skipped): ${(e && e.message) || e}\n`);
        }
      }
    });
  }

  return {
    resolveId, resolveIdResult, load, transform, transformUserCode, buildStart, renderChunk, generateBundle, pluginCount: plugins.length, watchFiles, writeBundle, closeBundle, buildEnd, renderStart,
  };
}

// Match Vite's plugin metadata using the app's Vite and bundled rolldown versions.
function pluginMeta(vite, command) {
  return {
    viteVersion: typeof vite?.version === "string" ? vite.version : "8.0.0",
    rollupVersion: "4.0.0",
    ...(typeof vite?.rolldownVersion === "string" ? { rolldownVersion: vite.rolldownVersion } : {}),
    watchMode: command !== "build",
    framework: "oj",
  };
}

function mergeConfigValues(current, update) {
  if (Array.isArray(current) && Array.isArray(update)) return [...current, ...update];
  if (current && update && typeof current === "object" && typeof update === "object") {
    const merged = { ...current };
    for (const key of Object.keys(update)) {
      merged[key] = key in current ? mergeConfigValues(current[key], update[key]) : update[key];
    }
    return merged;
  }
  return update === undefined ? current : update;
}

export async function loadPluginContainer(app, opts = {}) {
  // Vite's default mode follows the command (config hooks see it as env.mode).
  const { command = "serve", mode = command === "build" ? "production" : "development" } = opts;
  const configFile = findConfig(app);
  if (!configFile) return null;
  let vite;
  try { vite = await importPkg(app, "vite", ["@tanstack/react-start"]); } catch { return null; }
  if (typeof vite?.loadConfigFromFile !== "function") return null;
  let loaded;
  try {
    loaded = await vite.loadConfigFromFile({ command, mode }, configFile, app);
  } catch {
    return null;
  }
  // Like Vite, run matching config hooks in order and merge their results.
  // Give hooks Vite's basic context as `this`; Nitro checks this.meta.rolldownVersion.
  const configHookContext = {
    meta: pluginMeta(vite, command),
    debug() {}, info() {},
    warn(m) { process.stderr.write(`oj: ${typeof m === "string" ? m : m?.message ?? String(m)}\n`); },
    error(m) { throw m instanceof Error ? m : new Error(typeof m === "string" ? m : m?.message ?? String(m)); },
  };
  let config = loaded?.config ?? {};
  // seedConfig supplies defaults from skipped framework hooks, such as Start's
  // SSR input. User values win, and later hooks see the merged config.
  // A function receives the config before hooks run and returns defaults or nothing.
  const seed = typeof opts.seedConfig === "function" ? opts.seedConfig(config) : opts.seedConfig;
  if (seed) {
    config = typeof vite.mergeConfig === "function"
      ? vite.mergeConfig(seed, config)
      : mergeConfigValues(seed, config);
  }
  const all = (config.plugins ?? []).flat(Infinity).filter(Boolean);
  // Only user plugins: the framework plugins oj reimplements (TanStack, Vite's
  // own, React) never ran hooks under oj, and their config hooks have side
  // effects (the router plugin starts its generator) that fight oj's own.
  for (const plugin of all) {
    if (ojReimplemented(plugin.name)) continue;
    const handler = hookHandler(plugin.config);
    if (!handler || !applyMatches(plugin, command, mode)) continue;
    try {
      const partial = await handler.call(configHookContext, config, { command, mode });
      if (partial) config = typeof vite.mergeConfig === "function"
        ? vite.mergeConfig(config, partial)
        : mergeConfigValues(config, partial);
    } catch (e) {
      process.stderr.write(`oj: plugin "${plugin.name || "?"}" config hook failed (skipped): ${e?.message ?? e}\n`);
    }
  }
  // Then every plugin's `configEnvironment` hook, per environment (Vite's two
  // defaults plus the ones the config hooks declared); its partial merges into
  // that environment's options (e.g. a runtime plugin's `resolve.builtins`).
  const merge = (a, b) => (typeof vite.mergeConfig === "function" ? vite.mergeConfig(a, b) : mergeConfigValues(a, b));
  for (const name of new Set(["client", "ssr", ...Object.keys(config.environments ?? {})])) {
    let environmentOptions = config.environments?.[name] ?? {};
    let changed = false;
    for (const plugin of all) {
      if (ojReimplemented(plugin.name)) continue;
      const handler = hookHandler(plugin.configEnvironment);
      if (!handler || !applyMatches(plugin, command, mode)) continue;
      try {
        const partial = await handler.call(configHookContext, name, environmentOptions, { command, mode, isSsrTargetWebworker: false, isPreview: false });
        if (partial) { environmentOptions = merge(environmentOptions, partial); changed = true; }
      } catch (e) {
        process.stderr.write(`oj: plugin "${plugin.name || "?"}" configEnvironment failed (skipped): ${e?.message ?? e}\n`);
      }
    }
    if (changed) config = { ...config, environments: { ...config.environments, [name]: environmentOptions } };
  }
  if (loaded) loaded.config = config;
  const publicDir = config.publicDir === false
    ? false
    : typeof config.publicDir === "string" ? config.publicDir : null;
  const configDependencies = [configFile, ...(loaded?.dependencies ?? [])];
  // The config's `define` for this environment (top-level, then the
  // environment's own), serialized the way Vite's define plugin does
  // (handleDefineValue: strings verbatim, anything else JSON).
  const defines = (environment) => () => Object.fromEntries(
    Object.entries({ ...config.define, ...config.environments?.[environment]?.define })
      .map(([key, value]) => [key, typeof value === "string" ? value : JSON.stringify(value)])
      .filter(([, value]) => typeof value === "string"),
  );
  // Run matching buildApp hooks in pre/normal/post order, as Vite does.
  // ranks selects phases so oj can build its environments between them.
  const buildApp = async (builder, ranks = ["pre", "normal", "post"]) => {
    const rank = (hook) => (hook?.order === "pre" || hook?.order === "post" ? hook.order : "normal");
    for (const want of ranks) {
      for (const plugin of ordered(all)) {
        if (ojReimplemented(plugin.name) || rank(plugin.buildApp) !== want) continue;
        const handler = hookHandler(plugin.buildApp);
        if (!handler || !applyMatches(plugin, command, mode)) continue;
        await handler.call(configHookContext, builder);
      }
    }
  };
  // Share one resolved config and its plugins across environment containers,
  // like Vite's builder.sharedConfigBuild. Config hooks run once, so all
  // environments use the same Nitro instance.
  const forEnvironment = (environment) => ({
    ...createPluginContainer(vite, all, { ...opts, environment, config: { ...config, root: config.root ?? app } }),
    publicDir, configDependencies, defines: defines(environment), config, buildApp, forEnvironment,
  });
  return forEnvironment(opts.environment ?? "client");
}

export const __test = { matchOne, idAllowed, codeAllowed, byHook, applyMatches, ordered, hookHandler, hookFilter, ojReimplemented, envAllows };
