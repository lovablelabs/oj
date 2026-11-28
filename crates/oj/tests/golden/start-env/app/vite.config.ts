import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// A define that differs per environment (a client-only guard for `import()`s
// the server must never see), one build-time flag, and env vars.
export default defineConfig({
  build: { manifest: true },
  define: {
    "globalThis.__APP_SSR__": "false",
    "globalThis.__PRICING__": JSON.stringify(false),
  },
  environments: {
    ssr: { define: { "globalThis.__APP_SSR__": "true" } },
    client: { define: { "import.meta.env.VITE_BUILD_TIME": "undefined" } },
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
