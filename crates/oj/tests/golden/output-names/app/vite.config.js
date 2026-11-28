export default {
  build: {
    assetsDir: "static",
    rollupOptions: {
      input: { main: "src/main.js", admin: "src/admin.js" },
      output: {
        entryFileNames: "js/[name].js",
        chunkFileNames: "js/chunks/[name]-[hash].js",
        assetFileNames: "media/[name]-[hash][extname]",
      },
    },
  },
};
