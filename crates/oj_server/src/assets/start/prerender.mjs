// SPDX-License-Identifier: MIT

// `build.prerender` for a Start build: each route rendered by the built
// server bundle's handler and written as `<route>/index.html` under the
// client output, where dist/server.mjs serves it before the handler.
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

export async function run(env) {
  for (const [k, v] of Object.entries(env)) process.env[k] = v;
  const handler = (await import(pathToFileURL(env.OJ_SERVER_BUNDLE).href)).default;
  for (const route of JSON.parse(env.OJ_PRERENDER_ROUTES)) {
    const res = await handler.fetch(new Request("http://localhost" + route));
    const html = await res.text();
    const rel = route === "/" ? "index.html" : `${route.replace(/^\/+/, "").replace(/\/+$/, "")}/index.html`;
    const out = join(env.OJ_CLIENT_DIR, rel);
    mkdirSync(dirname(out), { recursive: true });
    writeFileSync(out, html);
  }
}
