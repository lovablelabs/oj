export default {
  css: { modules: { localsConvention: "camelCase" } },
  build: {
    manifest: true,
    rolldownOptions: {
      output: {
        codeSplitting: {
          groups: [{ name: (id) => (id.includes("/i18n/") ? "i18n/strings" : undefined), priority: 10 }],
        },
      },
    },
  },
};
