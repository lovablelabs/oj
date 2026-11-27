import { writeFileSync } from "node:fs";
import path from "node:path";
import { defineConfig } from "vite";

// The bundle a plugin's generateBundle and writeBundle see, as Vite's
// css-post, html and asset plugins fill it: the chunks' stylesheets as
// assets (with their `names` and `originalFileNames`), a chunk's
// `viteMetadata.importedCss`, an asset emitted in buildStart, and the page
// (emitted in a post plugin's generateBundle, so only writeBundle lists it).
function bundleAssets() {
  let root = "";
  const rel = (id) => (id && id.startsWith(root) ? id.slice(root.length + 1) : id);
  return {
    name: "app:bundle-assets",
    apply: "build",
    configResolved(config) {
      root = config.root;
    },
    buildStart() {
      this.emitFile({ type: "asset", fileName: "from-build-start.txt", source: "emitted in buildStart\n" });
    },
    generateBundle(_options, bundle) {
      const chunks = Object.values(bundle)
        .filter((output) => output.type === "chunk")
        .map((chunk) => ({ file: chunk.fileName, css: [...(chunk.viteMetadata?.importedCss ?? [])] }))
        .sort((a, b) => a.file.localeCompare(b.file));
      const assets = Object.values(bundle)
        .filter((output) => output.type === "asset")
        .map((asset) => ({
          file: asset.fileName,
          names: asset.names,
          originalFileNames: asset.originalFileNames.map(rel),
        }))
        .sort((a, b) => a.file.localeCompare(b.file));
      this.emitFile({
        type: "asset",
        fileName: "bundle-assets.json",
        source: JSON.stringify({ chunks, assets }, null, 2) + "\n",
      });
    },
    writeBundle(options, bundle) {
      writeFileSync(path.join(options.dir, "write-bundle.txt"), Object.keys(bundle).sort().join("\n") + "\n");
    },
  };
}

export default defineConfig({
  plugins: [bundleAssets()],
});
