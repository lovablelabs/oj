import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import mdx from "@mdx-js/rollup";
import svgr from "vite-plugin-svgr";
import { defineConfig } from "vite";
import type { Plugin } from "vite";

// A tiny plugin that owns a `virtual:build-info` module (id resolution + load).
// Exercises oj's plugin-container bridge on both the client bundle and the SSR
// loader, exactly as a real app's virtual modules do.
function buildInfoPlugin(): Plugin {
  const VID = "virtual:build-info";
  const RESOLVED = "\0" + VID;
  return {
    name: "fixture-build-info",
    resolveId(id) {
      if (id === VID) return RESOLVED;
    },
    load(id) {
      if (id === RESOLVED) return `export const buildTag = "fixture-virtual-ok";`;
    },
  };
}

// Mirrors a compile-on-startup i18n-style plugin: it compiles its output in
// buildStart, then its load() OVERRIDES a real on-disk .js file. Also reads
// this.environment.name so the served content proves the SSR context. If oj
// skips buildStart or reads the file from disk instead of consulting load,
// the SSR render shows STALE_ON_DISK (or BUILDSTART_SKIPPED) and fails.
function freshModulePlugin(): Plugin {
  let compiled: string | null = null;
  return {
    name: "fixture-fresh-module",
    enforce: "pre",
    buildStart() {
      // Set OJ_TEST_BUILDSTART_THROW to assert oj fails loud (not silently
      // serving half-compiled output) when a plugin's buildStart throws.
      if (process.env.OJ_TEST_BUILDSTART_THROW) throw new Error("boom-from-buildStart");
      compiled = "FRESH_via_buildStart";
    },
    load(id) {
      if (!id.replace(/\\/g, "/").endsWith("/src/generated/stale.js")) return;
      // Read both this.environment.name and this.environment.config.consumer —
      // oj must expose both (Vite shape) or a plugin reading config.consumer
      // throws and its module degrades to an export-less stub.
      const e = (this as { environment?: { name?: string; config?: { consumer?: string } } }).environment;
      const env = `${e?.name ?? "noenv"}-${e?.config?.consumer ?? "noconsumer"}`;
      const val = JSON.stringify(compiled == null ? "BUILDSTART_SKIPPED" : `${compiled}_${env}`);
      // Emit the arbitrary-string export form (as a compiled i18n barrel does),
      // reached by the caller as `m.freshMsg_cta()` — the exact failure shape.
      return `export const LABEL = ${val};\nconst fn = () => ${val};\nexport { fn as "freshMsg_cta" };`;
    },
  };
}

// vite-imagetools' shape: an `enforce: "pre"` object-form load keyed on an
// image's query, which must run before oj's own asset loader claims the id.
function imageVariantPlugin(): Plugin {
  return {
    name: "fixture-image-variant",
    enforce: "pre",
    load: {
      filter: { id: /\.png\?w=\d+/ },
      handler(id) {
        const w = new URL(id, "file:").searchParams.get("w");
        return `export default ${JSON.stringify(`image-variant:${w}`)};`;
      },
    },
  };
}

// A `pre` resolveId that answers a bare import by its query, as a package's
// `styles.css?url` plugin does: it must see the query, in SSR as in the browser.
function sheetUrlPlugin(): Plugin {
  const RESOLVED = "\0fixture-sheet-url";
  return {
    name: "fixture-sheet-url",
    enforce: "pre",
    resolveId: {
      filter: { id: /^fixture-sheet\/styles\.css\?url$/ },
      handler() {
        return RESOLVED;
      },
    },
    load(id) {
      if (id === RESOLVED) return `export default "fixture-sheet-url-ok";`;
    },
  };
}

// resolveId sees each import once, as written (Vite's import analysis): a
// file answer keeps its own query, a claim wins over an installed package or
// an existing file, a /-prefixed id comes to the pre hooks first, and a claim
// of the bare id alone never takes the queried import.
// In the dev server render only (the client and the build take a virtual
// module): a remote URL answer, which the Start host stubs rather than fetch,
// and a hook that throws, which fails its import.
function devSsrAnswersPlugin(): Plugin {
  let serve = false;
  return {
    name: "fixture-dev-ssr-answers",
    enforce: "pre",
    configResolved(config) {
      serve = config.command === "serve";
    },
    resolveId(id, _importer, options) {
      const devSsr = serve && (options?.ssr || this.environment?.config?.consumer === "server");
      if (id === "fixture-remote/x.css?url") {
        return devSsr ? "https://cdn.example/x.css" : "\0fixture-remote";
      }
      if (id === "fixture-throws/x.css?url") {
        // Only when the Start test asks (OJ_FIXTURE_RESOLVE_THROWS), so the
        // other suites on this fixture, a scan and pre-transforms included,
        // never hit it.
        const throws = process.env.OJ_FIXTURE_RESOLVE_THROWS === "1" && !options?.scan;
        if (devSsr && throws) throw new Error("fixture resolveId hook threw");
        return "\0fixture-throws";
      }
      return null;
    },
    load(id) {
      if (id === "\0fixture-remote") return `export default "remote-client";`;
      if (id === "\0fixture-throws") return `export default "resolved-without-throwing";`;
    },
  };
}

function queryClaimsPlugin(): Plugin {
  const answer = new URL("./src/content/answer.txt", import.meta.url).pathname;
  // Extension-free virtual ids, so no bundler types them by the import's.
  const virtual: Record<string, [string, string]> = {
    "react/index.js?raw": ["installed", "installed-package-claimed-ok"],
    "../content/claimed.txt?raw": ["relative", "relative-file-claimed-ok"],
    "/virtual-probe.txt?raw": ["slash", "slash-claimed-ok"],
  };
  const byId = new Map(Object.values(virtual).map(([key, text]) => [`\0query-claim-${key}`, text]));
  return {
    name: "fixture-query-claims",
    enforce: "pre",
    resolveId(id) {
      if (id === "fixture-missing/notes.txt?raw") return `${answer}?raw`;
      if (id in virtual) return `\0query-claim-${virtual[id][0]}`;
      if (id === "react/package.json") return "\0bare-file-claim";
      return null;
    },
    load(id) {
      if (byId.has(id)) return `export default ${JSON.stringify(byId.get(id))};`;
      if (id === "\0bare-file-claim") return `export default "bare-claim-took-the-raw-import";`;
    },
  };
}

// Echoes a page's import.meta.hot.send back to it (server.ws.on -> send).
function wsEchoPlugin(): Plugin {
  return {
    name: "fixture-ws-echo",
    configureServer(server) {
      server.ws.on("fixture:ping", (data) => server.ws.send("fixture:pong", { echo: data }));
    },
  };
}

export default defineConfig({
  // A non-default publicDir, to exercise oj adopting vite's publicDir.
  publicDir: "public",
  // A config define: must be applied by the SSR loader (dev) and the prod bundles.
  define: { __FIXTURE_DEFINE__: JSON.stringify("fixture-define-marker") },
  // A custom env prefix: FIXTURE_* vars from .env.<mode> reach import.meta.env,
  // unprefixed ones never do.
  envPrefix: ["VITE_", "FIXTURE_"],
  // Per-environment defines: each bundle (dev and prod, client and server) gets
  // its own value.
  environments: {
    client: { define: { __FIXTURE_SIDE__: JSON.stringify("client-side") } },
    ssr: { define: { __FIXTURE_SIDE__: JSON.stringify("server-side") } },
  },
  plugins: [
    buildInfoPlugin(),
    freshModulePlugin(),
    imageVariantPlugin(),
    sheetUrlPlugin(),
    queryClaimsPlugin(),
    devSsrAnswersPlugin(),
    wsEchoPlugin(),
    // .mdx before react/tanstack so its output is plain JSX for them to handle.
    mdx(),
    // exportType:"default" makes a bare `.svg` import a component; the second
    // include pattern also claims the explicit `foo.svg?react` query form.
    svgr({ svgrOptions: { exportType: "default" }, include: ["src/**/*.svg", "src/**/*.svg?react"] }),
    // The app's own server entry (src/ssr-entry.ts), as an SSR error wrapper
    // configures it: dev and the prod bundle must serve through it.
    tanstackStart({ server: { entry: "ssr-entry" } }),
    react(),
  ],
});
