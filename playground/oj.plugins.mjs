import { writeFileSync } from "node:fs";
import { resolve } from "node:path";

function lifecyclePlugin() {
  let command = "";
  return {
    name: "oj-lifecycle",
    configResolved(config) {
      command = config.command;
    },
    buildStart() {
      writeFileSync(".oj-cache/plugin-buildstart", command);
    },
    buildEnd() {
      writeFileSync(".oj-cache/plugin-buildend", command);
    },
  };
}

function ctxPlugin() {
  return {
    name: "oj-ctx",
    buildStart() {
      this.emitFile({ type: "asset", name: "oj-plugin-emitted.txt", source: "emitted-by-plugin" });
    },
    async transform(code, id) {
      if (!id.endsWith("App.tsx")) return null;
      let out = code;
      if (out.includes("__RESOLVED__")) {
        const r = await this.resolve("@/Counter", id);
        const base = r && r.id ? r.id.split(/[\\/]/).pop() : "unresolved";
        out = out.replace("__RESOLVED__", base);
      }
      if (out.includes("__MODINFO__")) {
        const r = await this.resolve("@/Counter", id);
        const loaded = r ? await this.load({ id: r.id }) : null;
        const info = r ? this.getModuleInfo(r.id) : null;
        const n = info ? info.importedIds.length : -1;
        const hasHook = loaded ? loaded.code.includes("useState") : false;
        out = out.replace("__MODINFO__", `${n}:${hasHook}`);
      }
      if (out.includes("__MODULEIDS__")) {
        const r = await this.resolve("@/Counter", id);
        if (r) await this.load({ id: r.id });
        const ids = [...this.getModuleIds()];
        const present = [id, r && r.id].filter((x) => x && ids.includes(x)).length;
        out = out.replace("__MODULEIDS__", String(present));
      }
      return out === code ? null : out;
    },
  };
}

function watchPlugin() {
  return {
    name: "oj-watch",
    transform(code, id) {
      if (!id.endsWith("App.tsx")) return null;
      this.addWatchFile(resolve("plugin-watched.txt"));
      return null; // side-effect only
    },
  };
}

function middlewarePlugin() {
  return {
    name: "oj-middleware",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (req.url === "/__oj_health") {
          res.setHeader("content-type", "text/plain");
          res.end("oj-plugin-mw-ok");
          return;
        }
        next();
      });
    },
  };
}

function envNamePlugin() {
  return {
    name: "oj-env-name",
    transform(code, id) {
      if (!id.endsWith("App.tsx") || !code.includes("__ENV_NAME__")) return null;
      return code.replace("__ENV_NAME__", this.environment.name);
    },
  };
}

function envPlugin(name, marker) {
  return {
    name: `oj-env-${name}`,
    applyToEnvironment(environment) {
      return environment.name === name;
    },
    transform(code, id) {
      if (!id.endsWith("App.tsx")) return null;
      return code.replace(marker, `${name}-ran`);
    },
  };
}

function reportPlugin() {
  return {
    name: "oj-report",
    renderChunk(code, chunk) {
      if (!chunk.isEntry) return null;
      return { code: `globalThis.__OJ_RC="oj-rc-ran";${code}` };
    },
    generateBundle(_options, bundle) {
      const chunks = Object.keys(bundle).filter((f) => bundle[f].type === "chunk");
      this.emitFile({ type: "asset", fileName: "oj-build-report.txt", source: `chunks:${chunks.length}` });
      for (const f of chunks) {
        if (bundle[f].isEntry) bundle[f].code = "/*oj-gb-banner*/" + bundle[f].code;
      }
    },
    renderStart() {
      writeFileSync(".oj-cache/plugin-renderstart", "render-start");
    },
    writeBundle(_options, bundle) {
      writeFileSync(".oj-cache/plugin-writebundle", Object.keys(bundle).sort().join(","));
    },
    closeBundle() {
      writeFileSync(".oj-cache/plugin-closebundle", "close-bundle");
    },
  };
}

function graphPlugin() {
  const parsed = new Set();
  return {
    name: "oj-graph",
    moduleParsed(info) {
      parsed.add(info.id);
    },
    watchChange(id, change) {
      writeFileSync(".oj-cache/plugin-watchchange", `${change.event}:${id.split(/[\\/]/).pop()}`);
    },
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (req.url === "/__oj_parsed") {
          res.setHeader("content-type", "text/plain");
          res.end(
            [...parsed]
              .map((p) => p.split(/[\\/]/).pop())
              .sort()
              .join(","),
          );
          return;
        }
        next();
      });
    },
  };
}

function markerPlugin() {
  return {
    name: "oj-marker",
    transform(code, id) {
      if (!id.endsWith(".tsx")) return null;
      if (!code.includes("__OJ_PLUGIN_UNTRANSFORMED__")) return null;
      return code.replace("__OJ_PLUGIN_UNTRANSFORMED__", "transformed-by-plugin");
    },
  };
}

function virtualPlugin() {
  const VIRTUAL_ID = "\0virtual:plugin-greeting";
  return {
    name: "oj-virtual",
    resolveId(source) {
      if (source === "virtual:plugin-greeting") return VIRTUAL_ID;
      return null;
    },
    load(id) {
      if (id === VIRTUAL_ID) return `export const info = "hello from plugin";`;
      return null;
    },
  };
}

function configPlugin() {
  let resolved = null;
  return {
    name: "oj-config",
    config(_config, env) {
      return { define: { __OJ_BUILT_BY__: "oj-plugin" } };
    },
    configResolved(config) {
      // { root, base, mode, command, define, server }
      resolved = config;
    },
    transform(code, id) {
      if (!id.endsWith(".tsx") || !code.includes("__OJ_CONFIG_MARKER__")) return null;
      const value = `${resolved.mode}:${resolved.define.__OJ_BUILT_BY__}`;
      return code.replace("__OJ_CONFIG_MARKER__", value);
    },
  };
}

function hmrPlugin() {
  return {
    name: "oj-hmr",
    handleHotUpdate({ file }) {
      if (file.endsWith("hmr-demo.tsx")) return "full-reload";
      return undefined;
    },
  };
}

function htmlPlugin() {
  return {
    name: "oj-html",
    transformIndexHtml(html) {
      return {
        html: html.replace("<title>oj playground</title>", "<title>oj playground (plugin)</title>"),
        tags: [
          { tag: "meta", attrs: { name: "oj-plugin-injected", content: "yes" }, injectTo: "head" },
          // No injectTo -> Vite default is head-prepend (lands right after <head>).
          { tag: "meta", attrs: { name: "oj-html-default", content: "yes" } },
        ],
      };
    },
  };
}

// Two object-form transformIndexHtml hooks with order pre/post. Registered
// post-before-pre in the array, so the sequence is only correct if oj honors
// `order`: pre injects the seq meta, post rewrites it to "pre-post".
function htmlSeqPost() {
  return {
    name: "oj-html-seq-post",
    transformIndexHtml: {
      order: "post",
      handler: (html) =>
        html.includes('name="oj-html-seq" content="pre"') ? html.replace('content="pre"', 'content="pre-post"') : html,
    },
  };
}
function htmlSeqPre() {
  return {
    name: "oj-html-seq-pre",
    transformIndexHtml: {
      order: "pre",
      handler: () => ({ tags: [{ tag: "meta", attrs: { name: "oj-html-seq", content: "pre" } }] }),
    },
  };
}

function envModePlugin() {
  return {
    name: "oj-env-mode",
    transform(code, id) {
      if (!id.endsWith("App.tsx") || !code.includes("__ENV_MODE__")) return null;
      return code.replace("__ENV_MODE__", this.environment.mode);
    },
  };
}

function orderPlugin(tag, enforce) {
  return {
    name: `oj-order-${tag}`,
    enforce,
    transform(code, id) {
      if (!id.endsWith("App.tsx") || !code.includes('data-order="')) return null;
      return code.replace(/(data-order="[^"]*)/, `$1-${tag}`);
    },
  };
}

function applyPlugin(tag, apply) {
  return {
    name: `oj-apply-${tag}`,
    apply,
    transform(code, id) {
      if (!id.endsWith("App.tsx")) return null;
      return code.replace("__APPLY__", tag);
    },
  };
}

export default [
  lifecyclePlugin(),
  ctxPlugin(),
  watchPlugin(),
  middlewarePlugin(),
  graphPlugin(),
  envNamePlugin(),
  envPlugin("client", "__ENV_CLIENT__"),
  envPlugin("ssr", "__ENV_SSR__"),
  reportPlugin(),
  markerPlugin(),
  virtualPlugin(),
  configPlugin(),
  hmrPlugin(),
  htmlPlugin(),
  htmlSeqPost(),
  htmlSeqPre(),
  envModePlugin(),
  orderPlugin("post", "post"),
  orderPlugin("pre", "pre"),
  applyPlugin("serve-only", "serve"),
  applyPlugin("build-only", "build"),
];
