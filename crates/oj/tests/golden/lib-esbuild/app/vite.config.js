export default {
  esbuild: { minifyIdentifiers: false },
  build: {
    minify: "esbuild",
    lib: { entry: "src/index.js", formats: ["es", "cjs"], fileName: "golden-lib-esbuild" },
  },
};
