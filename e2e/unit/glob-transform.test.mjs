// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  transformGlob,
  transformImportMetaUrl,
  stripLiterals,
} from "../../crates/oj_server/src/assets/start/glob-transform.mjs";

function fixture() {
  const dir = mkdtempSync(join(tmpdir(), "oj-glob-"));
  mkdirSync(join(dir, "content"), { recursive: true });
  writeFileSync(join(dir, "content", "a.md"), "# a");
  writeFileSync(join(dir, "content", "b.md"), "# b");
  writeFileSync(join(dir, "content", "index.json"), "{}");
  mkdirSync(join(dir, "pages", "home"), { recursive: true });
  mkdirSync(join(dir, "pages", "about"), { recursive: true });
  writeFileSync(join(dir, "pages", "home", "page.tsx"), "export default 1");
  writeFileSync(join(dir, "pages", "about", "page.tsx"), "export default 2");
  return dir;
}

test("leaves code without import.meta.glob untouched", () => {
  const code = "export const x = 1;\nconsole.log('hi');";
  assert.equal(transformGlob(code, "/app/x.ts"), code);
});

test("expands a lazy glob to a map of dynamic imports", () => {
  const dir = fixture();
  try {
    const out = transformGlob('const m = import.meta.glob("./content/*.md");', join(dir, "index.ts"));
    assert.match(out, /"\.\/content\/a\.md":\s*\(\)\s*=>\s*import\("\.\/content\/a\.md"\)/);
    assert.match(out, /"\.\/content\/b\.md":/);
    assert.doesNotMatch(out, /index\.json/);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("glob wildcards omit hidden paths unless explicitly requested", () => {
  const dir = fixture();
  try {
    writeFileSync(join(dir, "content", ".hidden.md"), "hidden");
    mkdirSync(join(dir, "content", ".draft"), { recursive: true });
    writeFileSync(join(dir, "content", ".draft", "nested.md"), "draft");

    const normal = transformGlob('const modules = import.meta.glob("./content/**/*.md");', join(dir, "index.ts"));
    const explicit = transformGlob('const modules = import.meta.glob("./content/.*.md");', join(dir, "index.ts"));

    assert.match(normal, /\.\/content\/a\.md/);
    assert.doesNotMatch(normal, /\.hidden\.md|\.draft/);
    assert.match(explicit, /\.\/content\/\.hidden\.md/);

    // `exhaustive: true` is Vite's dot: true plus node_modules included.
    mkdirSync(join(dir, "content", "node_modules"), { recursive: true });
    writeFileSync(join(dir, "content", "node_modules", "dep.md"), "dep");
    const exhaustive = transformGlob(
      'const modules = import.meta.glob("./content/**/*.md", { exhaustive: true });',
      join(dir, "index.ts"),
    );
    assert.match(exhaustive, /\.hidden\.md/);
    assert.match(exhaustive, /\.draft\/nested\.md/);
    assert.match(exhaustive, /node_modules\/dep\.md/);
    const plain = transformGlob('const modules = import.meta.glob("./content/**/*.md");', join(dir, "index.ts"));
    assert.doesNotMatch(plain, /node_modules/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("handles the <T> type argument and a single-star segment", () => {
  const dir = fixture();
  try {
    const out = transformGlob('const p = import.meta.glob<string>("./pages/*/page.tsx");', join(dir, "index.ts"));
    assert.match(out, /"\.\/pages\/home\/page\.tsx":/);
    assert.match(out, /"\.\/pages\/about\/page\.tsx":/);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Mirrors web/shared/pulse/components/data.ts, which broke oj's start SSR: a
// nested generic argument the old <[^>]*> regex could not skip.
test("handles a nested generic type argument (Record<string, unknown>)", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const m = import.meta.glob<Record<string, unknown>>(["./content/*.md"]);',
      join(dir, "index.ts"),
    );
    assert.match(out, /"\.\/content\/a\.md":/);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("handles an object-literal generic type argument", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const m = import.meta.glob<{ default: string }>("./content/*.md", { eager: true });',
      join(dir, "index.ts"),
    );
    assert.match(out, /^import\s+\*\s+as\s+\w+\s+from\s+"\.\/content\/a\.md";/m);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("expands every call when a module has several generic globs at module scope", () => {
  const dir = fixture();
  try {
    const code = [
      'const a = import.meta.glob<{ default: Foo }>("./content/*.json");',
      'const b = import.meta.glob<SidecarModule>("./pages/**/*.tsx");',
      'const c = import.meta.glob<Record<string, unknown>>(["./content/*.md"]);',
    ].join("\n");
    const out = transformGlob(code, join(dir, "index.ts"));
    assert.doesNotMatch(out, /import\.meta\.glob/);
    assert.match(out, /"\.\/pages\/home\/page\.tsx":/);
    assert.match(out, /"\.\/content\/a\.md":/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("expands a pattern that climbs above the importer directory", () => {
  const dir = fixture();
  try {
    mkdirSync(join(dir, "app", "deep"), { recursive: true });
    const out = transformGlob(
      'const m = import.meta.glob<Record<string, unknown>>("../../content/*.md");',
      join(dir, "app", "deep", "index.ts"),
    );
    assert.match(out, /"\.\.\/\.\.\/content\/a\.md":/);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("eager + query + import:default produces static imports of the ?query", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const s = import.meta.glob("./content/*.md", { eager: true, query: "?raw", import: "default" });',
      join(dir, "index.ts"),
    );
    assert.match(out, /^import\s+\w+\s+from\s+"\.\/content\/a\.md\?raw";/m);
    assert.match(out, /(?:^|; )import\s+\w+\s+from\s+"\.\/content\/b\.md\?raw";/m);
    assert.doesNotMatch(out, /\(\)\s*=>\s*import/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("non-eager import:default awaits the default export", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const s = import.meta.glob("./content/*.md", { import: "default" });',
      join(dir, "index.ts"),
    );
    assert.match(out, /\(\)\s*=>\s*import\("\.\/content\/a\.md"\)\.then\(\(m\)\s*=>\s*m\.default\)/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("glob query objects are serialized into generated import specifiers", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const modules = import.meta.glob("./content/*.md", { query: { raw: "", locale: "en US" } });',
      join(dir, "index.ts"),
    );

    assert.match(out, /import\("\.\/content\/a\.md\?raw=&locale=en\+US"\)/);
    assert.match(out, /"\.\/content\/a\.md":/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("named glob imports select the requested export in eager and lazy modes", () => {
  const dir = fixture();
  try {
    const lazy = transformGlob(
      'const modules = import.meta.glob("./content/*.md", { import: "metadata" });',
      join(dir, "index.ts"),
    );
    const eager = transformGlob(
      'const modules = import.meta.glob("./content/*.md", { eager: true, import: "metadata" });',
      join(dir, "index.ts"),
    );

    assert.match(lazy, /import\("\.\/content\/a\.md"\)\.then\(\(m\) => m\["metadata"\]\)/);
    assert.match(eager, /^import \{ metadata as __oj_glob0_0 \} from "\.\/content\/a\.md";/m);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("negated patterns exclude matches", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const m = import.meta.glob(["./content/*.md", "!./content/b.md"]);',
      join(dir, "index.ts"),
    );
    assert.match(out, /"\.\/content\/a\.md":/);
    assert.doesNotMatch(out, /"\.\/content\/b\.md":/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an empty glob yields an empty map, not a crash", () => {
  const dir = fixture();
  try {
    const out = transformGlob('const m = import.meta.glob("./nope/*.md");', join(dir, "index.ts"));
    assert.match(out, /const m = \{\}/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the rewrite adds no lines (source maps and stack traces stay aligned)", () => {
  const dir = fixture();
  try {
    const code = [
      "// header",
      "const eager = import.meta.glob(",
      '  "./content/*.md",',
      "  { eager: true },",
      ") as Record<string, unknown>;",
      'const lazy = import.meta.glob("./pages/**/*.tsx");',
      "export const marker = 1;",
    ].join("\n");
    const out = transformGlob(code, join(dir, "index.ts"));
    const lines = out.split("\n");
    assert.equal(lines.length, code.split("\n").length, out);
    assert.match(
      lines[0],
      /^import \* as __oj_glob0_0 from "\.\/content\/a\.md"; import \* as __oj_glob0_1 from "\.\/content\/b\.md"; \/\/ header$/,
    );
    assert.match(lines[1], /^const eager = \{"\.\/content\/a\.md": __oj_glob0_0, "\.\/content\/b\.md": __oj_glob0_1$/);
    assert.match(lines[4], /^\} as Record<string, unknown>;$/);
    assert.match(lines[5], /^const lazy = \{.*\};$/);
    assert.equal(lines[6], "export const marker = 1;");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Vite's `base` option (importMetaGlob.ts toAbsoluteGlob): patterns resolve
// against the base, root-relative when it starts with `/`, and the map keys are
// relative to that base; import specifiers stay relative to the importer.
test("base option resolves patterns and keys against the base", () => {
  const dir = fixture();
  try {
    mkdirSync(join(dir, "app", "deep"), { recursive: true });
    const importer = join(dir, "app", "deep", "index.ts");
    const rel = transformGlob('const m = import.meta.glob("./*.md", { base: "../../content" });', importer, dir);
    assert.match(rel, /"\.\/a\.md":\s*\(\)\s*=>\s*import\("\.\.\/\.\.\/content\/a\.md"\)/);
    assert.match(rel, /"\.\/b\.md":/);
    const root = transformGlob('const m = import.meta.glob("./*.md", { base: "/content" });', importer, dir);
    assert.match(root, /"\.\/a\.md":\s*\(\)\s*=>\s*import\("\.\.\/\.\.\/content\/a\.md"\)/);
    assert.throws(
      () => transformGlob('const m = import.meta.glob("./*.md", { base: "content" });', importer, dir),
      /Option "base" must start with/,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// A `/`-prefixed pattern is project-root relative (not an absolute fs path),
// and its keys are root-relative like Vite's `/src/...` keys.
test("root-relative patterns resolve against the project root with /-keys", () => {
  const dir = fixture();
  try {
    mkdirSync(join(dir, "app"), { recursive: true });
    const out = transformGlob('const m = import.meta.glob("/pages/**/page.tsx");', join(dir, "app", "index.ts"), dir);
    assert.match(out, /"\/pages\/home\/page\.tsx":\s*\(\)\s*=>\s*import\("\.\.\/pages\/home\/page\.tsx"\)/);
    assert.match(out, /"\/pages\/about\/page\.tsx":/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// The deprecated `as` option is `query` by another name; `as: 'raw' | 'url'`
// forces the default import, and mixing `as` with `query` is rejected.
test("as option maps to a query (raw/url force the default import)", () => {
  const dir = fixture();
  try {
    const raw = transformGlob(
      'const m = import.meta.glob("./content/*.md", { as: "raw", eager: true });',
      join(dir, "index.ts"),
      dir,
    );
    assert.match(raw, /^import __oj_glob0_0 from "\.\/content\/a\.md\?raw";/);
    const url = transformGlob(
      'const m = import.meta.glob("./content/*.md", { as: "url" });',
      join(dir, "index.ts"),
      dir,
    );
    assert.match(url, /import\("\.\/content\/a\.md\?url"\)\.then\(\(m\) => m\.default\)/);
    const bare = transformGlob(
      'const m = import.meta.glob("./content/*.md", { query: "raw" });',
      join(dir, "index.ts"),
      dir,
    );
    assert.match(bare, /import\("\.\/content\/a\.md\?raw"\)/);
    assert.throws(
      () =>
        transformGlob(
          'const m = import.meta.glob("./content/*.md", { as: "raw", query: "?x" });',
          join(dir, "index.ts"),
          dir,
        ),
      /"as" and "query" cannot be used together/,
    );
    assert.throws(
      () =>
        transformGlob(
          'const m = import.meta.glob("./content/*.md", { as: "raw", import: "named" });',
          join(dir, "index.ts"),
          dir,
        ),
      /can only be "default" or "\*"/,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("caseSensitive: false matches regardless of case; the importer itself is skipped", () => {
  const dir = fixture();
  try {
    writeFileSync(join(dir, "content", "Upper.MD"), "# u");
    const strict = transformGlob('const m = import.meta.glob("./content/*.md");', join(dir, "index.ts"), dir);
    assert.doesNotMatch(strict, /Upper\.MD/);
    const loose = transformGlob(
      'const m = import.meta.glob("./content/*.md", { caseSensitive: false });',
      join(dir, "index.ts"),
      dir,
    );
    assert.match(loose, /"\.\/content\/Upper\.MD":/);
    assert.match(loose, /"\.\/content\/a\.md":/);
    writeFileSync(join(dir, "pages", "index.ts"), "");
    const self = transformGlob('const m = import.meta.glob("./**/*.ts");', join(dir, "pages", "index.ts"), dir);
    assert.doesNotMatch(self, /"\.\/index\.ts"/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Vite's vite:worker-import-meta-url and vite:asset-import-meta-url, on a
// bundled client module: the URL points at the file's worker/asset import
// instead of staying relative to the chunk.
function urlFixture() {
  const dir = mkdtempSync(join(tmpdir(), "oj-meta-url-"));
  mkdirSync(join(dir, "img"), { recursive: true });
  writeFileSync(join(dir, "draw-worker.ts"), "self.onmessage = () => {};");
  writeFileSync(join(dir, "img", "a.png"), "");
  writeFileSync(join(dir, "img", "b.png"), "");
  return dir;
}
const fsResolve = async (spec, importer) => {
  const { existsSync } = await import("node:fs");
  const { resolve: res, dirname } = await import("node:path");
  // A bundler resolves only a relative spec to a file here; a bare one would
  // be a package.
  if (spec[0] !== ".") return null;
  const abs = res(dirname(importer), spec);
  return existsSync(abs) ? abs : null;
};
// Runs `fn` with console.warn captured.
async function warnings(fn) {
  const seen = [];
  const warn = console.warn;
  console.warn = (m) => seen.push(String(m));
  try {
    return { out: await fn(), seen };
  } finally {
    console.warn = warn;
  }
}

test("new Worker(new URL(...)) imports the worker's ?worker&url", async () => {
  const dir = urlFixture();
  try {
    const id = join(dir, "game.ts");
    const code = 'const w = new Worker(new URL("./draw-worker.ts", import.meta.url), { type: "module" });';
    const out = await transformImportMetaUrl(code, id, fsResolve);
    const spec = JSON.stringify(join(dir, "draw-worker.ts") + "?worker&url");
    assert.ok(out.startsWith(`import __oj_worker_url0 from ${spec};`), out);
    assert.match(
      out,
      /new Worker\(new URL\(\/\* @vite-ignore \*\/ __oj_worker_url0, '' \+ import\.meta\.url\), \{ type: "module" \}\)/,
    );
    assert.doesNotMatch(out, /"\.\/draw-worker\.ts"/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a SharedWorker's URL imports ?sharedworker&url, a query kept", async () => {
  const dir = urlFixture();
  try {
    const code = "new SharedWorker(new URL('./draw-worker.ts?x=1', import.meta.url))";
    const out = await transformImportMetaUrl(code, join(dir, "game.ts"), fsResolve);
    assert.ok(out.includes(JSON.stringify(join(dir, "draw-worker.ts") + "?sharedworker&url&x=1")), out);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("new URL(asset, import.meta.url) imports the asset's ?url", async () => {
  const dir = urlFixture();
  try {
    const code = 'export const a = new URL("./img/a.png", import.meta.url).href;';
    const out = await transformImportMetaUrl(code, join(dir, "game.ts"), fsResolve);
    assert.ok(
      out.startsWith(`import __oj_asset_url0 from ${JSON.stringify(join(dir, "img", "a.png") + "?url")};`),
      out,
    );
    assert.match(out, /new URL\(__oj_asset_url0, '' \+ import\.meta\.url\)\.href/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a template asset URL becomes Vite's eager ?url glob lookup", async () => {
  const dir = urlFixture();
  try {
    const id = join(dir, "game.ts");
    const code = "const n = 'a';\nexport const u = new URL(`./img/${n}.png`, import.meta.url);";
    const out = await transformImportMetaUrl(code, id, fsResolve);
    assert.ok(
      out.includes(
        'new URL((import.meta.glob("./img/*.png", {"eager":true,"import":"default","query":"?url"}))[`./img/${n}.png`], import.meta.url)',
      ),
      out,
    );
    const expanded = transformGlob(out, id);
    assert.match(expanded, /import __oj_glob0_0 from "\.\/img\/a\.png\?url";/);
    assert.match(expanded, /"\.\/img\/b\.png": __oj_glob0_1/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("comments, strings, data: URLs, @vite-ignore and missing files stay as written", async () => {
  const dir = urlFixture();
  try {
    const code = [
      '// new Worker(new URL("./draw-worker.ts", import.meta.url))',
      "const s = \"new URL('./img/a.png', import.meta.url)\";",
      'const d = new URL("data:text/plain,hi", import.meta.url);',
      'const i = new URL(/* @vite-ignore */ "./img/a.png", import.meta.url);',
      'const m = new URL("./img/missing.png", import.meta.url);',
      'const w = new Worker(new URL("./missing-worker.ts", import.meta.url));',
      "",
    ].join("\n");
    const { out, seen } = await warnings(() => transformImportMetaUrl(code, join(dir, "game.ts"), fsResolve));
    assert.equal(out, code);
    // Vite's warning, once for each missing file.
    assert.equal(seen.length, 2, seen.join("\n"));
    assert.match(seen[0], /missing-worker\.ts.*doesn't exist at build time/);
    assert.match(seen[1], /missing\.png.*@vite-ignore/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a dynamic template worker URL is an error, as in Vite", async () => {
  await assert.rejects(
    transformImportMetaUrl("new Worker(new URL(`./${n}.ts`, import.meta.url))", "/app/x.ts", fsResolve),
    /not supported in dynamic template string/,
  );
});

test("the rewrite keeps every line where it was", async () => {
  const dir = urlFixture();
  try {
    const code =
      'a();\nconst w = new Worker(\n  new URL(\n    "./draw-worker.ts",\n    import.meta.url,\n  ),\n);\nb();';
    const out = await transformImportMetaUrl(code, join(dir, "game.ts"), fsResolve);
    assert.equal(out.split("\n").length, code.split("\n").length);
    assert.equal(out.split("\n").at(-1), "b();");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a URL without ./ prefers the importer's directory, as Vite's preferRelative", async () => {
  const dir = urlFixture();
  try {
    const code = 'new Worker(new URL("draw-worker.ts", import.meta.url)); new URL("img/a.png", import.meta.url);';
    const out = await transformImportMetaUrl(code, join(dir, "game.ts"), fsResolve);
    assert.ok(out.includes(JSON.stringify(join(dir, "draw-worker.ts") + "?worker&url")), out);
    assert.ok(out.includes(JSON.stringify(join(dir, "img", "a.png") + "?url")), out);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a / URL is a root file when one exists, else a public URL left as written", async () => {
  const dir = urlFixture();
  try {
    const code =
      'new URL("/img/a.png", import.meta.url); new URL("/favicon.ico", import.meta.url); new URL("https://x.test/a.png", import.meta.url);';
    const out = await transformImportMetaUrl(code, join(dir, "src", "game.ts"), fsResolve, dir);
    assert.ok(
      out.startsWith(`import __oj_asset_url0 from ${JSON.stringify(join(dir, "img", "a.png") + "?url")};`),
      out,
    );
    assert.ok(out.includes('new URL("/favicon.ico", import.meta.url)'), out);
    assert.ok(out.includes('new URL("https://x.test/a.png", import.meta.url)'), out);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an asset URL's query joins its import (Vite's injectQuery), a hash stays on the URL", async () => {
  const dir = urlFixture();
  try {
    const code = 'new URL("./img/a.png?inline", import.meta.url); new URL("./img/b.png#top", import.meta.url);';
    const out = await transformImportMetaUrl(code, join(dir, "game.ts"), fsResolve);
    assert.ok(out.includes(JSON.stringify(join(dir, "img", "a.png") + "?url&inline")), out);
    assert.ok(out.includes(JSON.stringify(join(dir, "img", "b.png") + "?url")), out);
    assert.match(out, /new URL\(__oj_asset_url1 \+ "#top", '' \+ import\.meta\.url\)/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("literals strip like strip-literal: template code stays, regex bodies blank", async () => {
  assert.equal(stripLiterals("a(`x${b('c')}y`) // d"), "a(` ${b(' ')} `)     ");
  assert.equal(stripLiterals("x = /'[/]\\//g; y = a / b / c;"), "x = /      /g; y = a / b / c;");
  const dir = urlFixture();
  try {
    // A call inside a template's ${} is code, as Vite matches it; a quote in
    // a regex literal opens no string.
    const code = [
      "const q = /'/;",
      'const html = `<p>${new URL("./img/a.png", import.meta.url)}</p>`;',
      'const w = new Worker(new URL("./draw-worker.ts", import.meta.url));',
      "",
    ].join("\n");
    const out = await transformImportMetaUrl(code, join(dir, "game.ts"), fsResolve);
    assert.ok(out.includes("${new URL(__oj_asset_url1, '' + import.meta.url)}"), out);
    assert.ok(out.includes("new Worker(new URL(/* @vite-ignore */ __oj_worker_url0"), out);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("public and scheme URLs left as written raise no warning", async () => {
  const dir = urlFixture();
  try {
    const code = [
      'const p = new URL("/only-in-public.png", import.meta.url);',
      'const h = new URL("https://example.com/a.png", import.meta.url);',
    ].join("\n");
    const { out, seen } = await warnings(() => transformImportMetaUrl(code, join(dir, "game.ts"), fsResolve, dir));
    assert.equal(out, code);
    assert.deepEqual(seen, []);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a worker starting itself gets self.location.href, as in Vite", async () => {
  const dir = urlFixture();
  try {
    const id = join(dir, "draw-worker.ts");
    const code = 'new Worker(new URL("./draw-worker.ts", import.meta.url), { type: "module" });';
    const out = await transformImportMetaUrl(code, id, fsResolve, dir, id);
    assert.equal(out, 'new Worker(self.location.href, { type: "module" });');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
