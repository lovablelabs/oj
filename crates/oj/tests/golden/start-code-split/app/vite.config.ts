import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The splitter's groups restated and the pending component forced out, as a
// large app does to keep its fallbacks off the route's critical path.
export default defineConfig({
  build: { manifest: true },
  plugins: [
    tanstackStart({
      srcDirectory: "src",
      router: {
        codeSplittingOptions: {
          defaultBehavior: [["component"], ["pendingComponent"], ["errorComponent"], ["notFoundComponent"]],
        },
      },
      prerender: { enabled: false },
      sitemap: { enabled: false },
    }),
    react(),
  ],
});
