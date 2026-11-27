import { writeFileSync } from "node:fs";
import path from "node:path";
import { defineConfig } from "vite";

// A virtual module behind a `\0` id, with object-form hooks and filters.
function buildInfo() {
  return {
    name: "app:build-info",
    resolveId: {
      filter: { id: /^virtual:build-info$/ },
      handler: () => "\0virtual:build-info",
    },
    load: {
      filter: { id: /^\0virtual:build-info$/ },
      handler: () => `export const channel = "golden";\nexport const marker = "__RENDER_MARK__";`,
    },
  };
}

// A pre transform owning a file type of its own (a content compiler): the
// text as a default export, no map.
function textModules() {
  return {
    name: "app:text-modules",
    enforce: "pre",
    transform: {
      filter: { id: /\.note$/ },
      handler: (code) => ({ code: `export default ${JSON.stringify(code.trimEnd())};`, map: null }),
    },
  };
}

// A build-only transform replacing a token in the app's sources.
function buildTag() {
  return {
    name: "app:build-tag",
    apply: "build",
    transform(code, id) {
      if (!id.endsWith("/src/main.js") || !code.includes("__BUILD_TAG__")) return;
      return { code: code.replaceAll("__BUILD_TAG__", '"tag-from-transform"'), map: null };
    },
  };
}

// outputOptions wrapping the chunk name pattern: a chunk whose name holds a
// word URL filters block is written as "x" (ad blockers failing a route).
function neutralFileNames() {
  return {
    name: "app:neutral-file-names",
    apply: "build",
    enforce: "post",
    applyToEnvironment: (environment) => environment.config.consumer === "client",
    outputOptions(options) {
      const original = options.chunkFileNames ?? "assets/[name]-[hash].js";
      return {
        ...options,
        chunkFileNames: (chunk) => {
          const pattern = typeof original === "function" ? original(chunk) : original;
          return pattern.replace("[name]", chunk.name.replace(/tracking|analytics/g, "x"));
        },
      };
    },
  };
}

// renderChunk filling a marker with the chunk's own name, as a plugin
// stamping chunks does, and generateBundle reading the bundle (the graph a
// preload plugin builds: imports, dynamic imports, entries, facades, the
// modules) plus the module graph, written as an asset. The stylesheets and
// other assets a hook's bundle lists are the plugins-bundle-assets case.
function chunkGraph() {
  let root = "";
  const rel = (id) => (id && id.startsWith(root) ? id.slice(root.length + 1) : id);
  return {
    name: "app:chunk-graph",
    apply: "build",
    configResolved(config) {
      root = config.root;
    },
    renderChunk(code, chunk) {
      if (!code.includes("__RENDER_MARK__")) return null;
      return { code: code.replaceAll("__RENDER_MARK__", `rendered:${chunk.name}`), map: null };
    },
    generateBundle(_options, bundle) {
      const chunks = Object.values(bundle)
        .filter((output) => output.type === "chunk")
        .map((chunk) => ({
          file: chunk.fileName,
          name: chunk.name,
          isEntry: chunk.isEntry,
          isDynamicEntry: chunk.isDynamicEntry,
          facade: rel(chunk.facadeModuleId),
          imports: chunk.imports,
          dynamicImports: chunk.dynamicImports,
          modules: Object.keys(chunk.modules).map(rel).sort(),
          exports: chunk.exports,
          bytes: chunk.code.length,
        }))
        .sort((a, b) => a.file.localeCompare(b.file));
      const entry = chunks.find((c) => c.isEntry);
      const entryId = Object.keys(bundle[entry.file].modules).find((id) => id.endsWith("/src/main.js"));
      const info = this.getModuleInfo(entryId);
      const graph = {
        chunks,
        moduleCount: [...this.getModuleIds()].filter((id) => !id.startsWith("\0")).length,
        entry: {
          id: rel(info.id),
          importedIds: info.importedIds.map(rel).sort(),
          dynamicallyImportedIds: info.dynamicallyImportedIds.map(rel).sort(),
          isEntry: info.isEntry,
        },
      };
      this.emitFile({ type: "asset", fileName: "chunk-graph.json", source: JSON.stringify(graph, null, 2) + "\n" });
    },
    writeBundle(options, bundle) {
      const chunks = Object.values(bundle)
        .filter((output) => output.type === "chunk")
        .map((chunk) => chunk.fileName);
      writeFileSync(path.join(options.dir, "write-bundle.txt"), chunks.sort().join("\n") + "\n");
    },
  };
}

export default defineConfig({
  plugins: [buildInfo(), textModules(), buildTag(), neutralFileNames(), chunkGraph()],
});
