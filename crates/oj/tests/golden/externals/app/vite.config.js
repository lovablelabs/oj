export default {
  build: {
    rollupOptions: {
      external: ["some-cdn-lib", /^@scope\//],
    },
  },
};
