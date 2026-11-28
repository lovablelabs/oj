export default {
  esbuild: { drop: ["console", "debugger"], legalComments: "none" },
  build: { minify: "esbuild" },
};
