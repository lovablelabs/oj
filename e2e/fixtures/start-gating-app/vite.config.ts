// Minority-filter plugins only (no function-form hooks): the shape where the
// SSR/Start hook gate can prove it skips the bridge for every server module
// no filter claims. node_modules is a symlink into ../start-app (the CI
// install step covers both).
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const mdTransform = () => ({
  name: "gate-md-transform",
  transform: {
    filter: { id: /\.md$/ },
    handler(code: string) {
      return { code, map: null };
    },
  },
});
const svgLoad = () => ({
  name: "gate-svg-load",
  load: {
    filter: { id: /\.gate-svg$/ },
    handler() {
      return "export default 'svg';";
    },
  },
});

export default defineConfig({
  plugins: [tanstackStart(), react(), mdTransform(), svgLoad()],
  logLevel: "warn",
});
