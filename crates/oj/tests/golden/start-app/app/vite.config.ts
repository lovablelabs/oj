import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// A TanStack Start app as a large one is built: esbuild minify, hidden maps,
// a manifest for both environments, a define that differs per environment,
// no prerender (it would need the server to run).
export default defineConfig({
  resolve: { tsconfigPaths: true },
  build: { minify: "esbuild", sourcemap: "hidden" },
  define: { __SIDE__: JSON.stringify("either") },
  environments: {
    client: { define: { __SIDE__: JSON.stringify("client") }, build: { manifest: true } },
    ssr: { define: { __SIDE__: JSON.stringify("server") }, build: { manifest: true } },
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
