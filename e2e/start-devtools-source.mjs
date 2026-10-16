// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start dev runs @tanstack/devtools-vite's inject-source plugin on
// both sides, as Vite does, loaded from a --config file. Editors locate
// elements by the data-tsd-source attribute it writes into JSX; the plugin
// gates itself with `apply: (config) => config.mode === "development"` and
// names itself `@tanstack/devtools:*`. oj once dropped it three ways: the
// Start bridge called `apply({})` (no mode), every `@tanstack/` name was
// treated as a framework plugin oj reimplements, and the Start client
// container ignored --config. The plugin here is that shape, verbatim.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 3117);

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start devtools source: fixture deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const get = async (route) => {
  const res = await fetch(`http://localhost:${PORT}${route}`);
  return { status: res.status, body: await res.text() };
};

// The plugin rides an alternate config passed with --config, which every
// side must load (Vite's configFile): the Start client container once read
// only the default vite.config.* and would have bundled without it.
const altConfig = path.join(app, "vite.devtools-source.config.ts");
fs.writeFileSync(
  altConfig,
  `import type { Plugin, UserConfig } from "vite";
import base from "./vite.config";

const injectSource = {
  name: "@tanstack/devtools:inject-source",
  enforce: "pre",
  apply: (config: { mode?: string }) => config.mode === "development",
  transform: {
    filter: { id: { exclude: [/node_modules/, /\\?raw/, /\\/dist\\//, /\\/build\\//] } },
    handler(code: string, id: string) {
      if (!id.endsWith(".tsx")) return;
      const file = id.split("?")[0].replace(process.cwd(), "");
      return code.replace(/<(main|h1)\\b/g, (m) => m + ' data-tsd-source="' + file + '"');
    },
  },
} as Plugin;

export default { ...(base as UserConfig), plugins: [injectSource, ...((base as UserConfig).plugins ?? [])] };
`,
);

async function clientBundle() {
  const cache = path.join(app, ".oj-cache");
  const versioned = fs.readdirSync(cache).find((d) => /^v\d+$/.test(d));
  const index = JSON.parse(fs.readFileSync(path.join(cache, versioned, "start", "client-chunks.json"), "utf8"));
  let code = "";
  for (const f of index.files) {
    if (!f.name.endsWith(".js")) continue;
    const res = await get(`/@oj-start/${f.name}`);
    must(res.status === 200, `client chunk ${f.name} returned ${res.status}`);
    code += res.body;
  }
  return code;
}

fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
// From the app root, as a sandbox launches it: the plugin strips process.cwd().
const srv = spawn(oj, ["dev", app, "--config", altConfig, "--port", String(PORT)], { cwd: app, stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const html = (await get("/")).body;
  must(
    html.includes('<main data-tsd-source="/src/routes/index.tsx">'),
    `SSR HTML carries no data-tsd-source:\n${html.match(/<main[^>]*>/)?.[0]}`,
  );
  const js = await clientBundle();
  must(js.includes('"data-tsd-source": "/src/routes/index.tsx"'), "client bundle carries no data-tsd-source");
  console.log("start-dev: @tanstack/devtools inject-source tags SSR HTML and the client bundle");
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(altConfig, { force: true });
}
