// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A TanStack Start production build honors the build options Vite applies to
// any app: `--out` / `build.outDir`, `base`, `build.sourcemap`, `build.minify`.
// The fixture is built with a config override (its own config plus a sub-path
// base, source maps and no minification) into a temporary outDir, then served
// to check the base-prefixed asset URLs resolve.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");

const installed = fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start-build-options: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-build-options-"));
const out = path.join(tmp, "out");
const config = path.join(tmp, "vite.config.ts");
// The override config imports the fixture's own config (and its plugins), so it
// needs the fixture's dependencies resolvable from where it lives.
fs.symlinkSync(path.join(app, "node_modules"), path.join(tmp, "node_modules"));
fs.writeFileSync(path.join(tmp, "package.json"), '{"type":"module"}\n');
fs.writeFileSync(
  config,
  `import base from ${JSON.stringify(path.join(app, "vite.config"))};\n` +
    `export default { ...base, base: "/app/", build: { ...(base as any).build, sourcemap: true, minify: false } };\n`,
);

// A dist/ left by an earlier fixture build would read as "--out ignored" below;
// the assertion is about this build writing there, so start from a clean slate.
fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
// Likewise the manifest wrapper an earlier build left in the fixture's cache:
// the one checked below is the one this build wrote.
const wrappers = () =>
  fs
    .readdirSync(path.join(app, ".oj-cache"), { recursive: true })
    .map(String)
    .filter((f) => path.basename(f) === "manifest-sheets.ts")
    .map((f) => path.join(app, ".oj-cache", f));
if (fs.existsSync(path.join(app, ".oj-cache"))) for (const file of wrappers()) fs.rmSync(file);

try {
  const stderr = execSync(`${oj} build ${app} --config ${config} --out ${out} 2>&1 1>/dev/null`, {
    cwd: repo,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
  });
  if (/failed to load config/.test(stderr)) {
    throw new Error("the override config did not load for the plugin containers:\n" + stderr.slice(-800));
  }
  if (fs.existsSync(path.join(app, "dist"))) {
    throw new Error("--out was ignored: the fixture's own dist/ was written");
  }
  for (const f of ["server.mjs", "server-bundle.mjs", "server-bundle.mjs.map", "client"]) {
    if (!fs.existsSync(path.join(out, f))) throw new Error(`outDir is missing ${f}`);
  }
  const assets = path.join(out, "client", "assets");
  const clientJs = fs.readdirSync(assets).find((f) => /^client-.*\.js$/.test(f));
  if (!clientJs) throw new Error("no client entry chunk in outDir");
  if (!fs.existsSync(path.join(assets, clientJs + ".map")))
    throw new Error("build.sourcemap: true emitted no client .map");
  const code = fs.readFileSync(path.join(assets, clientJs), "utf8");
  if (code.split("\n").length < 50) throw new Error("build.minify: false was ignored (client chunk is minified)");
  if (!code.includes("sourceMappingURL=")) throw new Error("client chunk has no sourceMappingURL comment");
  if (!fs.readFileSync(path.join(out, "server.mjs"), "utf8").includes('const BASE = "/app/"')) {
    throw new Error("server.mjs does not know the base");
  }

  // The manifest the built server renders from, called as the server calls
  // it. It is the installed Start's own, as under `vite build`: the root
  // route's `preloads` and `scripts` name the client entry, which is what
  // start-server-core reads from 1.168 on. Start's manifest names no
  // stylesheet of oj's, so oj's wrapper (`manifest-sheets.ts`) adds the
  // client's sheets to the root route's `css`, and, for start-server-core
  // 1.167 and earlier, the fields those read: the top-level `clientEntry` (a
  // missing one renders `import(undefined)` and a page that never hydrates)
  // and the sheets as link tags in the root route's `assets`.
  const clientUrl = `/app/assets/${clientJs}`;
  const manifests = [];
  for (const file of fs.readdirSync(out, { recursive: true }).map(String)) {
    if (!file.endsWith(".mjs") || file.split(path.sep)[0] === "client") continue;
    const full = path.join(out, file);
    if (!/export\s*\{[^}]*\btsrStartManifest(?![\w$])/.test(fs.readFileSync(full, "utf8"))) continue;
    manifests.push((await import(pathToFileURL(full).href)).tsrStartManifest);
  }
  if (manifests.length !== 1) {
    throw new Error(`expected one server chunk exporting tsrStartManifest, found ${manifests.length}`);
  }
  const manifest = manifests[0]();
  const root = manifest.routes?.__root__;
  if (!root) throw new Error("the server manifest has no root route:\n" + JSON.stringify(manifest, null, 2));
  const shown = () => "\n" + JSON.stringify({ ...manifest, routes: { __root__: root } }, null, 2);
  if (!root.preloads?.includes(clientUrl))
    throw new Error("the root route does not preload the client entry" + shown());
  const entryScript = root.scripts?.find((script) => script.attrs?.src === clientUrl);
  if (entryScript?.attrs.type !== "module") {
    throw new Error("the root route has no module script for the client entry" + shown());
  }
  // oj links every stylesheet of the client from the root route: the ones
  // the build wrote, each once, and no other.
  const sheets = fs
    .readdirSync(assets, { recursive: true })
    .map((f) => String(f).split(path.sep).join("/"))
    .filter((f) => f.endsWith(".css"))
    .map((f) => `/app/assets/${f}`);
  if (sheets.length === 0) throw new Error("the fixture built no stylesheet");
  const hrefs = (root.css ?? []).map((sheet) => (typeof sheet === "string" ? sheet : sheet?.href));
  if (new Set(hrefs).size !== hrefs.length) throw new Error("the root route links a stylesheet twice" + shown());
  if (JSON.stringify([...hrefs].sort()) !== JSON.stringify([...sheets].sort())) {
    throw new Error(`the root route's stylesheets are not the ones the build wrote (${sheets.join(", ")})` + shown());
  }
  // The pre-1.168 fields.
  if (manifest.clientEntry !== clientUrl) throw new Error("the manifest lost the pre-1.168 clientEntry" + shown());
  const tagged = (root.assets ?? [])
    .filter((asset) => asset.tag === "link" && asset.attrs?.rel === "stylesheet")
    .map((asset) => asset.attrs.href);
  if (JSON.stringify(tagged) !== JSON.stringify(hrefs)) {
    throw new Error("the root route's pre-1.168 assets are not its stylesheets" + shown());
  }
  // A second call starts from Start's manifest again: nothing piles up.
  const again = manifests[0]().routes.__root__;
  if (again.css.length !== hrefs.length || again.assets.length !== root.assets.length) {
    throw new Error("a second call of the manifest added the stylesheets again");
  }

  // The wrapper over what other Start versions return, as this build wrote
  // it: its import of Start's manifest swapped for each shape in turn, and
  // its list of sheets for three, so that the sheets Start already has and
  // the ones it lacks are both there whatever the fixture builds.
  const written = wrappers();
  if (written.length !== 1) throw new Error(`expected one manifest wrapper in the cache, found ${written.length}`);
  const wrapper = fs.readFileSync(written[0], "utf8");
  const importLine = /^import \{ tsrStartManifest as startManifest \} from "tanstack-start-manifest:v";\n/;
  const sheetsLine = /^const sheets = (\[.*\]);\n/m;
  if (!importLine.test(wrapper)) throw new Error("the manifest wrapper no longer imports Start's manifest by its id");
  if (JSON.stringify(JSON.parse(wrapper.match(sheetsLine)?.[1] ?? "null")) !== JSON.stringify(hrefs)) {
    throw new Error("the manifest wrapper's sheets are not the root route's:\n" + wrapper);
  }
  const wrapped = async (startManifest, ownSheets) => {
    let code = wrapper.replace(importLine, `const startManifest = () => (${JSON.stringify(startManifest)});\n`);
    if (ownSheets) code = code.replace(sheetsLine, `const sheets = ${JSON.stringify(ownSheets)};\n`);
    const mod = await import("data:text/javascript," + encodeURIComponent(code));
    return mod.tsrStartManifest();
  };
  const link = (href) => ({ tag: "link", attrs: { rel: "stylesheet", href } });
  const same = (got, want, what) => {
    if (JSON.stringify(got) !== JSON.stringify(want)) {
      throw new Error(`the manifest wrapper, ${what}: got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
    }
  };
  // A manifest with nothing in it (no Start plugin answered), over the
  // sheets this build wrote.
  const bare = await wrapped({});
  same(bare.clientEntry, clientUrl, "over an empty manifest, clientEntry");
  same(bare.routes.__root__.css, hrefs, "over an empty manifest, css");
  same(bare.routes.__root__.assets, hrefs.map(link), "over an empty manifest, assets");
  const [a, b, c] = ["/app/assets/a.css", "/app/assets/b.css", "/app/assets/c.css"];
  const three = await wrapped({}, [a, b, c]);
  same(three.routes.__root__.css, [a, b, c], "three sheets over an empty manifest, css in link order");
  same(three.routes.__root__.assets, [a, b, c].map(link), "three sheets over an empty manifest, assets");
  // No sheet at all: empty lists, and the entry still.
  const none = await wrapped({}, []);
  same(none.routes.__root__.css, [], "no sheet, css");
  same(none.routes.__root__.assets, [], "no sheet, assets");
  same(none.clientEntry, clientUrl, "no sheet, clientEntry");
  // 1.167 and earlier: its own clientEntry and assets, kept as they are and
  // first; a sheet it already tagged is not tagged twice, the others follow.
  const meta = { tag: "meta", attrs: { name: "x" } };
  const older = await wrapped(
    {
      clientEntry: "/app/assets/its-own.js",
      routes: { __root__: { assets: [link(b), meta] }, "/about": { preloads: ["/a.js"] } },
    },
    [a, b, c],
  );
  same(older.clientEntry, "/app/assets/its-own.js", "over a manifest with a clientEntry");
  same(older.routes.__root__.assets, [link(b), meta, link(a), link(c)], "over a manifest that tagged a sheet, assets");
  same(older.routes.__root__.css, [a, b, c], "over a manifest that tagged a sheet, css");
  same(older.routes["/about"], { preloads: ["/a.js"] }, "another route");
  // 1.168 on: css as hrefs or as objects, a sheet it already links either
  // way, and one of its own that oj did not write.
  const theirs = "/app/assets/theirs.css";
  const newer = await wrapped({ routes: { __root__: { css: [{ href: c }, theirs, a], preloads: [clientUrl] } } }, [
    a,
    b,
    c,
  ]);
  same(newer.routes.__root__.css, [{ href: c }, theirs, a, b], "over a manifest that links sheets, css");
  same(newer.routes.__root__.assets, [a, b, c].map(link), "over a manifest that links sheets, assets");
  same(newer.routes.__root__.preloads, [clientUrl], "the root route's preloads");
  same(newer.clientEntry, clientUrl, "over a manifest without a clientEntry");

  const port = 6510;
  const srv = spawn("node", [path.join(out, "server.mjs")], {
    cwd: out,
    stdio: "ignore",
    env: { ...process.env, PORT: String(port) },
  });
  try {
    await waitUp(`http://localhost:${port}/`);
    const home = await fetch(`http://localhost:${port}/`);
    const html = await home.text();
    if (home.status !== 200) throw new Error(`/ returned ${home.status}`);
    if (!html.includes(`src="/app/assets/${clientJs}"`)) {
      throw new Error("the rendered page does not load the client entry under the base:\n" + html.slice(0, 800));
    }
    // What the server made of the manifest, by the page's link tags read
    // attribute by attribute (whatever their order): the entry's preload and
    // every stylesheet of the root route. React renders a stylesheet with a
    // precedence once per href, so a sheet the manifest named twice would
    // show once here too: that is the manifest check's above.
    const links = [...html.matchAll(/<link\b([^>]*)>/g)].map((tag) =>
      Object.fromEntries([...tag[1].matchAll(/([\w-]+)="([^"]*)"/g)].map((attr) => [attr[1], attr[2]])),
    );
    if (!links.some((tag) => tag.rel === "modulepreload" && tag.href === clientUrl)) {
      throw new Error("the rendered page does not preload the client entry:\n" + html.slice(0, 800));
    }
    for (const href of hrefs) {
      if (!links.some((tag) => tag.rel === "stylesheet" && tag.href === href)) {
        throw new Error(`the rendered page does not link ${href}:\n` + html.slice(0, 800));
      }
      const sheet = await fetch(`http://localhost:${port}${href}`);
      if (sheet.status !== 200) throw new Error(`${href} returned ${sheet.status}`);
    }
    if (/(?:src|href)="\/assets\//.test(html)) throw new Error("an asset URL was emitted without the base prefix");
    const asset = await fetch(`http://localhost:${port}/app/assets/${clientJs}`);
    if (asset.status !== 200) throw new Error(`base-prefixed asset returned ${asset.status}`);
  } finally {
    srv.kill("SIGKILL");
  }
  console.log("start-build-options: outDir, base, sourcemap and minify honored");
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
