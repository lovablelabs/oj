import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  build: { manifest: true },
  plugins: [
    tanstackStart({
      srcDirectory: "src",
      router: {
        codeSplittingOptions: {
          defaultBehavior: [
            ["component", "loader"],
            ["pendingComponent", "errorComponent", "notFoundComponent"],
          ],
        },
      },
      prerender: { enabled: false },
      sitemap: { enabled: false },
    }),
    react(),
  ],
});
