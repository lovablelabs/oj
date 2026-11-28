import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  resolve: { tsconfigPaths: true },
  build: { manifest: true },
  plugins: [
    tanstackStart({
      srcDirectory: "src",
      prerender: { enabled: false },
      sitemap: { enabled: false },
    }),
    react(),
  ],
});
