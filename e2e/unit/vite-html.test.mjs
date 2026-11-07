// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// vite-html.mjs is a port of Vite's dev HTML transform, so it is held to
// Vite itself: every page below goes through the fixture's real Vite
// (`server.transformIndexHtml`) and through the port with the same resolved
// config, plugins and `/@vite/client` tag, and the two must agree byte for
// byte, html-proxy module code included. Skips without the fixture's Vite.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { asset, repo } from "./harness.mjs";

const { createHtmlProxy, scanHtml, transformDevHtml } = await import(pathToFileURL(asset("vite-html.mjs")).href);

const viteDir = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");
const hasVite = fs.existsSync(viteDir);
const viteTest = hasVite ? test : (name) => test(name, { skip: "fixture vite not installed" }, () => {});

// Hooks in every position Vite orders and every injectTo it honours.
const plugins = () => [
  {
    name: "post-tags",
    transformIndexHtml: {
      order: "post",
      handler: () => [
        { tag: "script", attrs: { src: "/post-body.js" }, injectTo: "body" },
        { tag: "div", attrs: { id: "first" }, children: "x", injectTo: "body-prepend" },
      ],
    },
  },
  {
    name: "pre-env",
    transformIndexHtml: {
      order: "pre",
      handler: (html) => html.replace("<!--pre-->", "%VITE_TITLE%"),
    },
  },
  {
    name: "normal-tags",
    transformIndexHtml: (html, ctx) => ({
      html,
      tags: [
        { tag: "meta", attrs: { name: "page", content: ctx.path }, injectTo: "head" },
        { tag: "link", attrs: { rel: "stylesheet", href: "/n.css", crossorigin: true } },
      ],
    }),
  },
];

const pages = [
  {
    name: "a real index.html with inline modules",
    file: "index.html",
    url: "/index.html",
    originalUrl: "/",
    html: `<!doctype html>
<html lang="en">
  <head lang="en">
    <meta charset="UTF-8" />
    <title><!--pre--> %MODE% %VITE_MISSING%</title>
    <script type="module">
      import React from "react";
      import "./src/a.js";
      console.log(React, "</div>");
    </script>
  </head>
  <body>
    <!-- <script type="module">commented()</script> -->
    <div id="root"></div>
    <script type="module" src="/src/main.ts"></script>
    <script type=module>
      import { b } from "/src/b.js";
    </script>
  </body>
</html>
`,
  },
  {
    name: "a virtual page a middleware serves (the canvas preview shape)",
    url: "/__component/preview/Badge",
    originalUrl: "/__component/preview/Badge",
    html: `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <title>Preview: Badge</title>
</head>
<body>
  <div id="component-root"></div>
  <script type="module">
    import React from "react";
    import ReactDOM from "react-dom/client";
    import "@/styles.css";
  </script>
</body>
</html>
`,
  },
  {
    name: "a SPA fallback for a nested route with relative urls",
    file: "index.html",
    url: "/index.html",
    originalUrl: "/dashboard/settings",
    html: `<html><head>
<link rel="icon" href="./favicon.svg">
<meta property="og:image" content="./og.png">
<meta name="description" content="./not-an-asset">
<script type="importmap">{"imports":{}}</script>
</head>
<body>
<img src="./a.png" srcset="./a.png 1x, ./a@2x.png 2x" alt="a">
<img vite-ignore src="./ignored.png">
<script vite-ignore type="module" src="./ignored.js"></script>
<script type="module" src='./src/main.tsx'></script>
<script>window.go = () => import('./src/lazy.js'); // import('./commented.js')</script>
<template><script type="module">templated()</script></template>
</body></html>
`,
  },
  {
    name: "inline styles and style attributes with urls",
    file: "index.html",
    url: "/index.html",
    originalUrl: "/",
    html: `<html><head>
<style>body { margin: 0 }</style>
<script type="module">import "/src/a.js"</script>
</head><body>
<div style="background: url('/bg.png')"></div>
<p style="color: red"></p>
<script type="module">import "/src/b.js"</script>
<span style='background: url("/a.png"), url(/b.png)'></span>
<style></style>
<style>.b { color: blue }</style>
</body></html>
`,
  },
  {
    name: "a page without head or body",
    url: "/bare.html",
    originalUrl: "/bare.html",
    html: `<script type="module">bare()</script><p>hi</p>`,
  },
];

viteTest("the dev HTML transform matches Vite's byte for byte", async () => {
  const { createServer } = await import(pathToFileURL(path.join(viteDir, "dist/node/index.js")).href);
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oj-vite-html-")));
  fs.writeFileSync(path.join(root, ".env"), "VITE_TITLE=From env\n");
  fs.writeFileSync(path.join(root, "index.html"), "<html></html>");
  const server = await createServer({
    root,
    configFile: false,
    logLevel: "silent",
    appType: "custom",
    server: { middlewareMode: true, hmr: false, ws: false, preTransformRequests: false },
    optimizeDeps: { noDiscovery: true, include: [] },
    html: { cspNonce: "n0nce" },
    plugins: plugins(),
  });
  try {
    for (const page of pages) {
      const expected = await server.transformIndexHtml(page.url, page.html, page.originalUrl);
      const proxy = createHtmlProxy(() => root);
      // Vite's CSS pipeline leaves these sheets as written (no PostCSS config,
      // root-absolute urls): what is held to Vite is where each style goes
      // and under which id; vite:css-post escapes an attribute's quotes.
      const styleIds = [];
      const compileStyle = async (id, code) => {
        styleIds.push(id);
        return /[?&]style-attr\b/.test(id) ? code.replace(/"/g, "&quot;") : code;
      };
      const actual = await transformDevHtml({
        url: page.url,
        html: page.html,
        originalUrl: page.originalUrl,
        server,
        config: server.config,
        plugins: server.config.plugins,
        bind: (_p, handler) => ({ fn: handler }),
        proxy,
        moduleGraph: server.environments.client.moduleGraph,
        clientTags: [{ tag: "script", attrs: { type: "module", src: "/@vite/client" }, injectTo: "head-prepend" }],
        compileStyle,
      });
      assert.equal(actual, expected, page.name);
      // Vite registered each style under the id the port compiled it as.
      for (const id of styleIds) {
        assert.ok(await server.environments.client.moduleGraph.getModuleByUrl(id), `${page.name}: ${id}`);
      }
      // Each lifted module loads the same code from both caches.
      const ids = [...actual.matchAll(/src="([^"]*html-proxy[^"]*)"/g)].map((m) => m[1]);
      assert.ok(ids.length > 0, `${page.name}: inline modules were lifted`);
      for (const url of ids) {
        const id = url.startsWith("/@id/") ? url.slice(5).replace("__x00__", "\0") : url;
        const vite = await server.environments.client.pluginContainer.load(id);
        const ours = proxy.plugin.load.handler(id);
        assert.equal(ours.code, vite.code, `${page.name}: ${url}`);
      }
    }
  } finally {
    await server.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("scanning finds elements the way parse5 does", () => {
  const html = `<!-- <script>no</script> --><SCRIPT Type="module" src=a.js src="b.js"></SCRIPT>
<style>p{}</style><script>if (a </script) {}</script ><img alt='&quot;q&amp;' src="x.png"/>`;
  const nodes = scanHtml(html);
  assert.deepEqual(
    nodes.map((n) => n.nodeName),
    ["script", "style", "script", "img"],
  );
  const [script, style, classic, img] = nodes;
  assert.deepEqual(script.attrs, [
    { name: "type", value: "module" },
    { name: "src", value: "a.js" },
  ]);
  assert.equal(
    html.slice(script.sourceCodeLocation.attrs.src.startOffset, script.sourceCodeLocation.attrs.src.endOffset),
    "src=a.js",
  );
  assert.equal(style.childNodes[0].value, "p{}");
  // A raw-text element ends only at its own end tag.
  assert.equal(classic.childNodes[0].value, "if (a </script) {}");
  assert.equal(
    html.slice(classic.sourceCodeLocation.startOffset, classic.sourceCodeLocation.endOffset),
    "<script>if (a </script) {}</script >",
  );
  assert.deepEqual(img.attrs[0], { name: "alt", value: '"q&' });
});
