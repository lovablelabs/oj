import type { Plugin } from "vite";

import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// A config hook adding a define; an outputOptions hook renaming chunks whose
// name holds a word URL filters block, on the client only; a generateBundle
// reading the client bundle's graph and each chunk's stylesheets into an
// asset, as a preload-header plugin does.
function appPlugins(): Plugin[] {
  return [
    {
      name: "app:flags",
      config: () => ({ define: { __FLAG__: JSON.stringify("on") } }),
    },
    {
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
    },
    {
      name: "app:client-graph",
      apply: "build",
      applyToEnvironment: (environment) => environment.config.consumer === "client",
      generateBundle(_options, bundle) {
        const chunks = Object.values(bundle)
          .filter((o) => o.type === "chunk")
          .map((c) => ({
            file: c.fileName,
            name: c.name,
            isEntry: c.isEntry,
            isDynamicEntry: c.isDynamicEntry,
            imports: c.imports,
            dynamicImports: c.dynamicImports,
            css: [...(c.viteMetadata?.importedCss ?? [])],
          }))
          .sort((a, b) => a.file.localeCompare(b.file));
        this.emitFile({ type: "asset", fileName: "client-graph.json", source: JSON.stringify(chunks, null, 2) + "\n" });
      },
    },
  ];
}

export default defineConfig({
  build: { manifest: true },
  plugins: [
    ...appPlugins(),
    tanstackStart({
      srcDirectory: "src",
      prerender: { enabled: false },
      sitemap: { enabled: false },
    }),
    react(),
  ],
});
