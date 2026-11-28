import mdx from "@mdx-js/rollup";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import svgr from "vite-plugin-svgr";

// The plugin set of a React app: MDX compiled ahead of the JSX lowering, an
// svg as a component by default (`?url` and `?raw` keep their asset forms),
// React's plugin with the automatic runtime.
export default defineConfig({
  plugins: [
    { enforce: "pre", ...mdx() },
    svgr({ svgrOptions: { exportType: "default" }, include: ["src/**/*.svg"] }),
    react(),
  ],
});
