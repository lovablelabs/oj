import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The client environment's chunk policy of a large app: vendor groups by
// test, the entry's static closure pinned by tag and share count, a group
// named by a function, the common-chunk merge off, names as functions.
export default defineConfig({
  resolve: { tsconfigPaths: true },
  environments: {
    client: {
      build: {
        manifest: true,
        rolldownOptions: {
          experimental: { chunkOptimization: { mergeCommonChunks: false } },
          output: {
            chunkFileNames: (chunk) =>
              chunk.name.includes("~") ? `assets/${chunk.name.split("~")[0]}-[hash].js` : "assets/[name]-[hash].js",
            assetFileNames: (asset) => {
              const name = asset.names[0] ?? "";
              return name.endsWith(".css") && name.includes("~")
                ? `assets/${name.split("~")[0]}-[hash][extname]`
                : "assets/[name]-[hash][extname]";
            },
            codeSplitting: {
              groups: [
                { name: "vendor-react", test: /[\\/]node_modules[\\/](react|react-dom|scheduler)[\\/]/, priority: 100 },
                { name: "vendor-router", test: /[\\/]node_modules[\\/]@tanstack[\\/]/, priority: 90 },
                {
                  name: (id: string) => {
                    const m = id.replaceAll("\\", "/").match(/\/i18n\/([a-z]{2})\.ts$/);
                    return m ? `i18n-fn/${m[1]}` : undefined;
                  },
                  priority: 200,
                },
                { name: "app-shared", tags: ["$initial"], minShareCount: 2, priority: 50 },
                { name: "app-boot", tags: ["$initial"], minShareCount: 1, priority: 45 },
                { name: "lazy-commons", minShareCount: 2, priority: 10 },
              ],
            },
          },
        },
      },
    },
  },
  plugins: [
    tanstackStart({
      srcDirectory: "src",
      prerender: { enabled: false },
      sitemap: { enabled: false },
    }),
    react(),
  ],
});
