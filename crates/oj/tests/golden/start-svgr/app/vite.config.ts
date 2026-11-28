import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import svgr from "vite-plugin-svgr";
export default defineConfig({
  base: "/app/",
  build: { manifest: true, assetsInlineLimit: 0 },
  plugins: [
    svgr({ svgrOptions: { exportType: "default" }, include: ["src/**/*.svg?react"] }),
    tanstackStart({
      srcDirectory: "src",
      prerender: { enabled: false },
      sitemap: { enabled: false },
    }),
    react(),
  ],
});
