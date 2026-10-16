// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  existsSync,
  readdirSync,
  rmSync,
  realpathSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  needsCssCompile,
  workspaceRoot,
  pnpmStorePaths,
  contentHashEmitter,
  makeVitePlugins,
  appAliasPlugin,
  assetsPlugin,
} from "../../crates/oj_server/src/assets/start/rolldown-assets.mjs";
import { repo, testWithRolldown } from "./harness.mjs";

const tmp = (p) => mkdtempSync(join(tmpdir(), "oj-assets-" + p + "-"));

test("needsCssCompile detects Tailwind markers only", () => {
  assert.ok(needsCssCompile('@import "tailwindcss";'));
  assert.ok(needsCssCompile("@tailwind base;"));
  assert.ok(needsCssCompile('@plugin "@tailwindcss/typography";'));
  assert.ok(needsCssCompile(".b{@apply flex}"));
  assert.ok(!needsCssCompile(".b{color:red}"));
  assert.ok(!needsCssCompile("@font-face{src:url(x.woff2)}"));
});

test("workspaceRoot returns the farthest ancestor with node_modules", () => {
  const base = tmp("ws");
  try {
    mkdirSync(join(base, "node_modules"), { recursive: true });
    mkdirSync(join(base, "web", "node_modules"), { recursive: true });
    mkdirSync(join(base, "web", "src"), { recursive: true });
    assert.equal(workspaceRoot(join(base, "web")), base);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("pnpmStorePaths lists every .pnpm package node_modules dir", () => {
  const base = tmp("pnpm");
  try {
    const store = join(base, "node_modules", ".pnpm");
    mkdirSync(join(store, "react@19", "node_modules"), { recursive: true });
    mkdirSync(join(store, "@babel+runtime@7", "node_modules"), { recursive: true });
    mkdirSync(join(store, "no-nm-here"), { recursive: true });
    const paths = pnpmStorePaths(base);
    assert.equal(paths.length, 2);
    assert.ok(paths.some((p) => p.endsWith(join("react@19", "node_modules"))));
    assert.ok(paths.some((p) => p.endsWith(join("@babel+runtime@7", "node_modules"))));
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("emit copies a file under a content hash and returns its URL", async () => {
  const base = tmp("emit");
  try {
    const src = join(base, "src");
    mkdirSync(src, { recursive: true });
    writeFileSync(join(src, "logo.png"), "PNGDATA");
    const emit = contentHashEmitter(join(base, "client"));
    const url = await emit(join(src, "logo.png"));
    assert.match(url, /^\/assets\/logo-[0-9a-f]{8}\.png$/);
    assert.ok(existsSync(join(base, "client", "assets", url.slice("/assets/".length))));
    assert.equal(await emit(join(src, "logo.png")), url);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("emit rewrites CSS url() refs and emits the referenced assets", async () => {
  const base = tmp("css");
  try {
    const styles = join(base, "styles");
    mkdirSync(join(styles, "fonts"), { recursive: true });
    writeFileSync(join(styles, "fonts", "font.woff2"), "WOFF2");
    writeFileSync(
      join(styles, "app.css"),
      "@font-face{font-family:x;src:url(./fonts/font.woff2)}.d{background:url(data:image/gif;base64,AA)}",
    );
    const emit = contentHashEmitter(join(base, "client"));
    const url = await emit(join(styles, "app.css"));
    const out = readFileSync(join(base, "client", "assets", url.slice("/assets/".length)), "utf8");
    assert.match(out, /url\("\/assets\/font-[0-9a-f]{8}\.woff2"\)/);
    assert.ok(readdirSync(join(base, "client", "assets")).some((f) => /^font-[0-9a-f]{8}\.woff2$/.test(f)));
    assert.match(out, /url\(data:image\/gif;base64,AA\)/);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("emit compiles CSS via compileCss before rewriting when it needs it", async () => {
  const base = tmp("tw");
  try {
    mkdirSync(join(base, "styles"), { recursive: true });
    writeFileSync(join(base, "styles", "globals.css"), '@import "tailwindcss";');
    let called = 0;
    const compileCss = async (from, src) => {
      called++;
      assert.ok(src.includes("tailwindcss"));
      return ".compiled{color:red}";
    };
    const emit = contentHashEmitter(join(base, "client"), compileCss);
    const url = await emit(join(base, "styles", "globals.css"));
    const out = readFileSync(join(base, "client", "assets", url.slice("/assets/".length)), "utf8");
    assert.equal(called, 1);
    assert.equal(out, ".compiled{color:red}");
    assert.ok(!out.includes('@import "tailwindcss"'));
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("emit records emitted stylesheet urls (not other assets)", async () => {
  const base = tmp("cssurls");
  try {
    const dir = join(base, "s");
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "a.css"), ".a{color:red}");
    writeFileSync(join(dir, "b.css"), ".b{color:blue}");
    writeFileSync(join(dir, "logo.png"), "PNG");
    const emit = contentHashEmitter(join(base, "client"));
    const aUrl = await emit(join(dir, "a.css"));
    await emit(join(dir, "logo.png"));
    const bUrl = await emit(join(dir, "b.css"));
    await emit(join(dir, "a.css"));
    const urls = emit.cssUrls();
    assert.deepEqual(urls, [aUrl, bUrl], "only stylesheets, in order, deduped");
    assert.ok(!urls.some((u) => u.endsWith(".png")), "non-css assets excluded");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("emit does not compile plain CSS (no markers)", async () => {
  const base = tmp("plain");
  try {
    mkdirSync(join(base, "styles"), { recursive: true });
    writeFileSync(join(base, "styles", "plain.css"), ".x{color:blue}");
    let called = 0;
    const emit = contentHashEmitter(join(base, "client"), async () => {
      called++;
      return "";
    });
    const url = await emit(join(base, "styles", "plain.css"));
    const out = readFileSync(join(base, "client", "assets", url.slice("/assets/".length)), "utf8");
    assert.equal(called, 0);
    assert.equal(out, ".x{color:blue}");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("the Rolldown adapter forwards build completion only to its active container", async () => {
  const completed = [];
  const failure = new Error("synthetic bundle failure");
  const adapter = makeVitePlugins({
    container: {
      buildEnd(error) {
        completed.push({ environment: "active", error });
      },
    },
    fallback: {
      buildEnd(error) {
        completed.push({ environment: "fallback", error });
      },
    },
  });

  await adapter.buildEnd(failure);

  assert.deepEqual(completed, [{ environment: "active", error: failure }]);
});

test("the Rolldown adapter forwards render initialization only to its active container", async () => {
  const rendered = [];
  const output = { format: "es" };
  const input = { input: "/synthetic/entry.ts" };
  const adapter = makeVitePlugins({
    container: {
      renderStart(...options) {
        rendered.push({ environment: "active", options });
      },
    },
    fallback: {
      renderStart(...options) {
        rendered.push({ environment: "fallback", options });
      },
    },
  });

  await adapter.renderStart(output, input);

  assert.deepEqual(rendered, [{ environment: "active", options: [output, input] }]);
});

// Drives the alias plugin's resolveId with a resolver that echoes the rewritten
// id, as the rest of the pipeline would resolve it.
const aliasResolve = async (alias, source, warnings = []) => {
  const plugin = appAliasPlugin({ resolve: { alias } });
  if (!plugin) return null;
  const seen = [];
  const ctx = {
    resolve: async (id, importer, options) => {
      seen.push(options);
      return id.startsWith("/missing/") || id.startsWith("missing/") ? null : { id };
    },
    warn: (msg) => warnings.push(msg),
  };
  const r = await plugin.resolveId.call(ctx, source, "/app/src/a.jsx", { kind: "import-statement" });
  if (r) assert.ok(seen.every((o) => o.skipSelf === true && o.kind === "import-statement"));
  return r?.id ?? null;
};

test("the app alias matches like @rollup/plugin-alias: the key itself or the key plus a slash", async () => {
  const alias = { "@": "/app/src" };
  assert.equal(await aliasResolve(alias, "@/lib/x"), "/app/src/lib/x");
  assert.equal(await aliasResolve(alias, "@"), "/app/src");
  // A scoped package shares the `@` prefix but never the `@/` one.
  assert.equal(await aliasResolve(alias, "@tanstack/react-router"), null);
  assert.equal(await aliasResolve(alias, "./@/x"), null);
});

test("the app alias takes Vite's entry forms: arrays in order, RegExp finds, trailing slashes", async () => {
  // First matching entry wins, in array order.
  const ordered = [
    { find: "@/lib", replacement: "/app/lib" },
    { find: "@", replacement: "/app/src" },
  ];
  assert.equal(await aliasResolve(ordered, "@/lib/x"), "/app/lib/x");
  assert.equal(await aliasResolve(ordered, "@/ui/x"), "/app/src/ui/x");
  assert.equal(await aliasResolve([{ find: /^~(.*)$/, replacement: "/app/src/$1" }], "~ui/b"), "/app/src/ui/b");
  // Vite's normalizeSingleAlias drops the trailing slash from both sides.
  assert.equal(await aliasResolve({ "src/": "/app/src/" }, "src/x"), "/app/src/x");
  // Entries without a string replacement (a customResolver-only shape) are not aliases here.
  assert.equal(appAliasPlugin({ resolve: { alias: [{ find: "x", replacement: null }] } }), null);
  assert.equal(appAliasPlugin({}), null);
  assert.equal(appAliasPlugin(undefined), null);
});

test("an alias whose target does not resolve keeps the rewritten id, as Vite's alias plugins do", async () => {
  // The rewritten id stands so the import fails to load, instead of the bare
  // `@/x` falling through to be externalized and shipped to the browser.
  const absolute = [];
  assert.equal(await aliasResolve({ "@": "/missing" }, "@/x", absolute), "/missing/x");
  assert.deepEqual(absolute, []);
  // A relative rewrite nothing handled gets plugin-alias's duplicate-module warning.
  const relative = [];
  assert.equal(await aliasResolve({ "@": "missing" }, "@/x", relative), "missing/x");
  assert.equal(relative.length, 1);
  assert.match(relative[0], /rewrote @\/x to missing\/x but was not an absolute path/);
});

// Asset imports are classified by the resolved id at load, as Vite's asset,
// css and worker plugins do, never by a resolve-time marker: rolldown swaps
// `custom` between concurrent this.resolve calls, which once handed a bare
// `/abs/x.png` to rolldown to read as UTF-8 source.
testWithRolldown(test)("asset imports load by their resolved id, whichever resolver answered", async () => {
  const base = realpathSync(tmp("plain"));
  try {
    const png = join(base, "logo.png");
    writeFileSync(png, Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe, 0x00]));
    writeFileSync(join(base, "pov.css"), ".pov{color:red}");
    writeFileSync(join(base, "note.txt"), "hello raw");
    writeFileSync(join(base, "w.ts"), "self.postMessage(1);");
    writeFileSync(join(base, "star.svg"), "<svg/>");
    writeFileSync(
      join(base, "entry.js"),
      [
        'import a from "@/logo.png";',
        'import b from "./logo.png?url";',
        'import c from "@/note.txt?raw";',
        'import d from "./logo.png?no-inline";',
        'import w from "@/w.ts?worker&url";',
        'import s from "./star.svg?react";',
        'import "@/pov.css";',
        'import "./pov.css";',
        // A resolver answering with the bare file, as a swapped `custom` did.
        'import e from "bare:logo.png";',
        'import "bare:pov.css";',
        "export { a, b, c, d, e, w, s };",
      ].join("\n"),
    );
    const { rolldown } = await import(join(repo, "e2e/fixtures/start-app/node_modules/rolldown/dist/index.mjs"));
    const cssUrls = [];
    const bundle = await rolldown({
      input: join(base, "entry.js"),
      plugins: [
        { name: "bare", resolveId: (s) => (s.startsWith("bare:") ? join(base, s.slice(5)) : null) },
        appAliasPlugin({ resolve: { alias: { "@": base } } }),
        makeVitePlugins({ appRoot: base, mode: "dev" }),
        assetsPlugin({ mode: "dev", cssUrls, root: base }),
      ],
    });
    const { output } = await bundle.generate({ format: "esm" });
    await bundle.close();
    const code = output[0].code;
    const fs = (p) => JSON.stringify("/@oj-start/fs" + join(base, p));
    assert.ok(code.includes(fs("logo.png")), code);
    assert.ok(code.includes(JSON.stringify("hello raw")), code);
    assert.ok(code.includes(fs("star.svg")), code);
    assert.ok(code.includes(JSON.stringify("/w.ts")), code);
    assert.deepEqual(cssUrls, ["/@oj-start/fs" + join(base, "pov.css")]);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
