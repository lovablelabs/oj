// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, basename, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createRequire } from "node:module";
import {
  assetsPlugin,
  makeVitePlugins,
  nodeBuiltinShims,
} from "../../crates/oj_server/src/assets/start/rolldown-assets.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = resolve(here, "..", "fixtures", "start-app");

let build = null;
try {
  const req = createRequire(pathToFileURL(join(fixture, "package.json")).href);
  const m = await import(pathToFileURL(req.resolve("rolldown")).href);
  build = m.build;
} catch {}

const it = build ? test : (name) => test(name, { skip: "fixture rolldown not installed" }, () => {});

const emit = async (abs) => "/assets/" + basename(abs);

function stubContainer() {
  return {
    async resolveId(id) {
      return id === "virtual:info" ? "\0virtual:info" : null;
    },
    async load(id) {
      if (id === "\0virtual:info") return `export const info = "VIRTUAL_INFO_OK";`;
      if (id.endsWith(".svg?react")) return `export default () => "SVG_REACT_COMPONENT";`;
      if (id.endsWith("icon.svg")) return `export default () => "SVG_BARE_COMPONENT";`;
      return null;
    },
    async transform(code, id) {
      if (id.endsWith(".mdx")) return `export default () => ${JSON.stringify("MDX:" + code.trim())};`;
      return null;
    },
    async generateBundle() {},
    publicDir: null,
    pluginCount: 1,
  };
}

async function bundle(files, plugins) {
  const dir = mkdtempSync(join(tmpdir(), "oj-route-"));
  try {
    for (const [name, contents] of Object.entries(files)) {
      const p = join(dir, name);
      mkdirSync(dirname(p), { recursive: true });
      writeFileSync(p, contents);
    }
    const res = await build({
      input: join(dir, "entry.js"),
      write: false,
      output: { format: "esm" },
      plugins: plugins(dir),
    });
    return { text: res.output.map((f) => f.code || "").join("\n"), dir };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

it("routes ?raw, ?url, ?inline, and bare-asset imports (prod)", async () => {
  const files = {
    "notes.txt": "RAW_FILE_MARKER contents",
    "logo.png": "PNGDATA-logo",
    "tiny.png": "PNGDATA-tiny",
    "pic.png": "PNGDATA-pic",
    "entry.js": [
      'import raw from "./notes.txt?raw";',
      'import url from "./logo.png?url";',
      'import inline from "./tiny.png?inline";',
      'import bare from "./pic.png";',
      "export { raw, url, inline, bare };",
    ].join("\n"),
  };
  const { text } = await bundle(files, () => [assetsPlugin({ mode: "prod", server: false, emit })]);
  assert.ok(text.includes("RAW_FILE_MARKER contents"), "?raw should inline the text");
  assert.ok(text.includes("/assets/logo.png"), "?url should emit an /assets URL");
  assert.ok(text.includes("/assets/pic.png"), "bare asset should emit an /assets URL");
  assert.match(text, /data:image\/png[;,]/, "?inline should be a data URI");
});

it("?url resolves to the dev fs route in dev mode", async () => {
  const files = {
    "logo.png": "PNGDATA",
    "entry.js": 'import url from "./logo.png?url"; export { url };',
  };
  const { text } = await bundle(files, () => [assetsPlugin({ mode: "dev", server: false, fsBase: "/@oj-start/fs" })]);
  assert.match(text, /\/@oj-start\/fs[^"']*logo\.png/, "dev ?url should point at the fs route");
  assert.doesNotMatch(text, /\/assets\/logo\.png/, "dev should not emit a hashed /assets url");
});

it("css import is a no-op module and records the url (dev client)", async () => {
  const files = {
    "styles.css": ".a{color:red}",
    "entry.js": 'import "./styles.css"; export const ok = 1;',
  };
  const server = await bundle(files, () => [assetsPlugin({ mode: "prod", server: true, emit })]);
  assert.doesNotMatch(server.text, /createElement\("link"\)/, "server css import must not touch the DOM");

  const cssUrls = [];
  const client = await bundle(files, () => [
    assetsPlugin({ mode: "dev", server: false, fsBase: "/@oj-start/fs", cssUrls }),
  ]);
  assert.doesNotMatch(client.text, /createElement\("link"\)/, "dev client must not inject a link");
  assert.ok(
    cssUrls.some((u) => u.startsWith("/@oj-start/fs") && u.endsWith("styles.css")),
    `recorded: ${cssUrls}`,
  );
});

it("prod client css import emits the stylesheet but injects no link", async () => {
  const files = {
    "styles.css": ".a{color:red}",
    "entry.js": 'import "./styles.css"; export const ok = 1;',
  };
  const emitted = [];
  const spyEmit = async (abs) => (emitted.push(abs), "/assets/" + basename(abs));
  const out = await bundle(files, () => [assetsPlugin({ mode: "prod", server: false, emit: spyEmit })]);
  assert.doesNotMatch(out.text, /createElement\("link"\)/, "prod client must not inject a link");
  assert.ok(
    emitted.some((p) => p.endsWith("styles.css")),
    "css still emitted in prod",
  );
});

it("routes virtual: ids and .mdx through the plugin container", async () => {
  const files = {
    "doc.mdx": "# Title\nmdx body",
    "entry.js": ['import { info } from "virtual:info";', 'import Doc from "./doc.mdx";', "export { info, Doc };"].join(
      "\n",
    ),
  };
  const { text } = await bundle(files, (dir) => [
    makeVitePlugins({ container: stubContainer(), appRoot: dir, mode: "prod", emit }),
    assetsPlugin({ mode: "prod", server: false, emit }),
  ]);
  assert.ok(text.includes("VIRTUAL_INFO_OK"), "virtual: id should load from the container");
  assert.ok(text.includes("MDX:# Title"), ".mdx should be transformed by the container");
});

it("routes bare .svg and .svg?react through the container, URL fallback otherwise", async () => {
  const files = {
    "icon.svg": "<svg><rect/></svg>",
    "plain.svg": "<svg><circle/></svg>",
    "entry.js": [
      'import Icon from "./icon.svg";',
      'import IconReact from "./icon.svg?react";',
      'import plain from "./plain.svg";',
      "export { Icon, IconReact, plain };",
    ].join("\n"),
  };
  const { text } = await bundle(files, (dir) => [
    makeVitePlugins({ container: stubContainer(), appRoot: dir, mode: "prod", emit }),
    assetsPlugin({ mode: "prod", server: false, emit }),
  ]);
  assert.ok(text.includes("SVG_BARE_COMPONENT"), "bare .svg should reach the container");
  assert.ok(text.includes("SVG_REACT_COMPONENT"), ".svg?react should reach the container with the query");
  assert.ok(text.includes("/assets/plain.svg"), "an unclaimed svg should fall back to a URL");
});

it("shims node builtins for the browser bundle", async () => {
  const files = {
    "entry.js": [
      'import { AsyncLocalStorage } from "node:async_hooks";',
      'import stream from "stream";',
      "export const als = new AsyncLocalStorage();",
      "export { stream };",
    ].join("\n"),
  };
  const { text } = await bundle(files, () => [nodeBuiltinShims({ production: true })]);
  assert.match(text, /AsyncLocalStorage/, "async_hooks should be shimmed");
  assert.match(text, /getStore\(\)/, "the ALS shim should expose getStore");
  assert.match(text, /PassThrough/, "bare node builtins should resolve to the stream shim");
});

it("named imports from a shimmed builtin bundle instead of link-failing", async () => {
  const files = {
    "entry.js": [
      'import { createHmac } from "node:crypto";',
      'import { setTimeout as sleep } from "timers/promises";',
      "export const probes = [typeof createHmac, typeof sleep];",
    ].join("\n"),
  };
  // The CJS stub interops the named import into a property read; before the
  // shims were CommonJS this was a rolldown MISSING_EXPORT build failure,
  // and the bare subpath spelling was not shimmed at all.
  const { text } = await bundle(files, () => [nodeBuiltinShims({ production: true })]);
  assert.match(text, /probes/, "the client bundle should link");
});

it("an installed package sharing a builtin's name wins over the shim", async () => {
  const files = {
    "entry.js": ['import { ucs2decode } from "punycode";', "export const out = ucs2decode('a');"].join("\n"),
    "node_modules/punycode/package.json": JSON.stringify({
      name: "punycode",
      version: "2.3.1",
      main: "index.js",
      type: "module",
    }),
    "node_modules/punycode/index.js": "export const ucs2decode = (s) => 'REAL_PUNYCODE:' + s;",
  };
  // Vite's resolution order: the browser-external stub only exists for
  // builtins node resolution cannot find, so the userland punycode package
  // (widely depended on) must not be shadowed by the identity shim.
  const { text } = await bundle(files, () => [nodeBuiltinShims({ production: true })]);
  assert.match(text, /REAL_PUNYCODE/, "the installed package should win over the builtin shim");
});
