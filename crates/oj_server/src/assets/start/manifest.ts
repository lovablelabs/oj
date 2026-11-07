// The union of the manifest shapes across start-server-core versions, like
// manifest-dev.ts: 1.168+ emits the client entry script tag from `scripts`,
// 1.167 and earlier from the top-level `clientEntry`. Each version copies
// only the fields it knows, so the extras never double-inject.
export const tsrStartManifest = () => ({
  routes: {
    __root__: {
      preloads: ["/@oj-start/client-entry.js"],
      scripts: [{ attrs: { type: "module", async: true, src: "/@oj-start/client-entry.js" } }],
    },
  },
  clientEntry: "/@oj-start/client-entry.js",
});
