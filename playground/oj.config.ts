import { defineConfig } from "oj";
export default defineConfig({
  virtualModules: {
    "virtual:oj-info": 'export const tool = "oj"; export default { version: 9 };',
  },
  define: {
    __OJ_DEFINE_GLOBAL__: JSON.stringify("global-define"),
  },
  build: {
    manifest: true,
    prerender: ["/", "/about"],
  },
  environments: {
    client: {
      define: { __OJ_DEFINE_CLIENT__: JSON.stringify("client-define") },
      build: { sourcemap: true },
    },
    ssr: {
      define: { __OJ_DEFINE_SSR__: JSON.stringify("ssr-define") },
      build: { sourcemap: false },
    },
  },
  server: {
    proxy: {
      "/api": { target: "http://localhost:8899", changeOrigin: true, rewrite: { from: "^/api", to: "" } },
    },
  },
});
