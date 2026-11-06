// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The golden pages oj's native dev HTML transform (crates/oj_server/src/dev_html.rs)
// is tested against: every page in tests/fixtures/dev-html/pages.json goes
// through the fixture's real Vite (`server.transformIndexHtml`, no plugins)
// under each config below, and the output and its html-proxy modules must be
// the stored ones. UPDATE_DEV_HTML_GOLDENS=1 rewrites them from Vite. Skips
// without the fixture's Vite.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { asset, repo } from "./harness.mjs";

const { htmlEnvOf } = await import(pathToFileURL(asset("vite-html.mjs")).href);

const dir = path.join(repo, "crates/oj_server/tests/fixtures/dev-html");
const viteDir = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");
const hasVite = fs.existsSync(viteDir);
const viteTest = hasVite ? test : (name) => test(name, { skip: "fixture vite not installed" }, () => {});

export const CONFIGS = {
  plain: {},
  nonce: { html: { cspNonce: "n0nce" } },
  base: { base: "/app/" },
};

viteTest("the dev HTML goldens are Vite's output", async () => {
  const { createServer } = await import(pathToFileURL(path.join(viteDir, "dist/node/index.js")).href);
  const pages = JSON.parse(fs.readFileSync(path.join(dir, "pages.json"), "utf8"));
  const goldens = {};
  for (const [key, extra] of Object.entries(CONFIGS)) {
    const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oj-dev-html-golden-")));
    fs.writeFileSync(path.join(root, ".env"), "VITE_TITLE=From env\n");
    for (const page of pages) if (page.file) fs.writeFileSync(path.join(root, page.file), "<html></html>");
    const server = await createServer({
      root,
      configFile: false,
      logLevel: "silent",
      appType: "custom",
      server: { middlewareMode: true, hmr: false, ws: false, preTransformRequests: false },
      optimizeDeps: { noDiscovery: true, include: [] },
      ...extra,
    });
    try {
      const out = {
        base: server.config.base,
        nonce: server.config.html?.cspNonce ?? null,
        envPrefix: [server.config.envPrefix ?? "VITE_"].flat(),
        env: htmlEnvOf(server.config),
        pages: {},
      };
      for (const page of pages) {
        const html = await server.transformIndexHtml(page.url, page.html, page.originalUrl);
        const proxies = {};
        for (const [, url] of html.matchAll(/src="([^"]*html-proxy[^"]*)"/g)) {
          const base = server.config.base;
          const u = base !== "/" && url.startsWith(base) ? url.slice(base.length - 1) : url;
          const id = u.startsWith("/@id/") ? u.slice(5).replace("__x00__", "\0") : u;
          proxies[url] = (await server.environments.client.pluginContainer.load(id)).code;
        }
        out.pages[page.name] = { html, proxies };
      }
      goldens[key] = out;
    } finally {
      await server.close();
      fs.rmSync(root, { recursive: true, force: true });
    }
  }
  const file = path.join(dir, "goldens.json");
  const text = JSON.stringify(goldens, null, 2) + "\n";
  if (process.env.UPDATE_DEV_HTML_GOLDENS === "1" || !fs.existsSync(file)) fs.writeFileSync(file, text);
  assert.equal(
    fs.readFileSync(file, "utf8"),
    text,
    "goldens.json is Vite's output (UPDATE_DEV_HTML_GOLDENS=1 rewrites it)",
  );
});
