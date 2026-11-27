import path from "node:path";
import { defineConfig } from "vite";

// A regex alias to a browser shim (an app keeping a dep's Node build out of
// its bundle), tsconfig `paths` through Vite 8's resolver, and the package's
// own `imports` map, each reaching the same files.
export default defineConfig({
  resolve: {
    alias: [{ find: /^debug$/, replacement: path.resolve(__dirname, "src/shims/debug-browser.js") }],
    tsconfigPaths: true,
  },
});
