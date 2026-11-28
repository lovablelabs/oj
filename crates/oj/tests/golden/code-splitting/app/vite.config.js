import path from "node:path";
import { defineConfig } from "vite";

// A large app's build block: esbuild minify, a pinned browser
// floor, the manifest, a public directory of its own, chunk names as a
// function of the chunk, rolldown's chunk groups pinned by tag and share
// count (one named by a function), the common-chunk merge off, a regex alias
// and a define.
export default defineConfig({
  publicDir: path.resolve(__dirname, "static"),
  resolve: {
    alias: [{ find: /^debug$/, replacement: path.resolve(__dirname, "src/shims/debug-browser.js") }],
  },
  define: {
    "globalThis.__APP_SSR__": "false",
    "globalThis.__LOCAL_PRICING__": JSON.stringify(false),
  },
  build: {
    minify: "esbuild",
    manifest: true,
    target: ["chrome111", "edge111", "firefox114", "safari16.4", "ios16.4"],
    rolldownOptions: {
      experimental: {
        chunkOptimization: { mergeCommonChunks: false },
      },
      output: {
        chunkFileNames: (chunk) =>
          chunk.name.includes("~") ? `assets/${chunk.name.split("~")[0]}-[hash].js` : "assets/[name]-[hash].js",
        assetFileNames: "assets/[name]-[hash][extname]",
        codeSplitting: {
          groups: [
            { name: "vendor-charts", test: /[\\/]vendor[\\/]charts[\\/]/, priority: 100 },
            { name: "vendor-dates", test: /[\\/]vendor[\\/]dates[\\/]/, priority: 99, tags: ["$initial"] },
            {
              name: (id) => {
                const match = id.replaceAll("\\", "/").match(/\/i18n\/([a-z]{2})\.js$/);
                return match ? `i18n-fn/${match[1]}` : undefined;
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
});
