// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start's load-env plugin assigns every var of the mode's `.env`
// files to `process.env`, so server functions read unprefixed secrets
// (`process.env.DATABASE_URL`). `envPrefix` only gates `import.meta.env`, so
// the client must still never see them. A copy of the Start fixture whose
// server function returns `process.env.UNPREFIXED_SECRET`, checked under
// `oj dev` (SSR page shows it, no client chunk carries it, import.meta.env on
// the server lacks it) and in a prerendered `oj build`.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 3104;
const SECRET = "must-not-leak";

if (!fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start-dotenv-process-env: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-dotenv-"));
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/[/\\](node_modules|\.oj-cache|dist)$/.test(src),
});
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"));
const edit = (rel, from, to) => {
  const file = path.join(app, rel);
  const code = fs.readFileSync(file, "utf8");
  if (!code.includes(from)) throw new Error(`fixture drifted: ${rel} lacks ${from}`);
  fs.writeFileSync(file, code.replace(from, to));
};
edit(
  "src/server/data.ts",
  "edition: String(edition) }",
  `edition: String(edition), secret: process.env.UNPREFIXED_SECRET ?? "no-secret", meta: String(import.meta.env.UNPREFIXED_SECRET) }`,
);
edit(
  "src/routes/index.tsx",
  "edition={data.edition}",
  "edition={data.edition} / secret={data.secret} / meta={data.meta}",
);

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
// React separates adjacent text nodes with `<!-- -->`.
const text = (html) => html.replaceAll("<!-- -->", "");
const get = async (route) => (await fetch(`http://localhost:${PORT}${route}`)).text();

let failed = false;
try {
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  try {
    await waitUp(`http://localhost:${PORT}/`);
    const html = text(await get("/"));
    must(html.includes(`secret=${SECRET}`), `dev: process.env lacks the .env var:\n${html.match(/secret=[^<]*/)?.[0]}`);
    must(
      html.includes("meta=undefined"),
      `dev: an unprefixed var reached import.meta.env:\n${html.match(/meta=[^<]*/)?.[0]}`,
    );
    const versioned = fs.readdirSync(path.join(app, ".oj-cache")).find((d) => /^v\d+$/.test(d));
    const index = JSON.parse(
      fs.readFileSync(path.join(app, ".oj-cache", versioned, "start", "client-chunks.json"), "utf8"),
    );
    for (const f of index.files.filter((f) => f.name.endsWith(".js"))) {
      must(!(await get(`/@oj-start/${f.name}`)).includes(SECRET), `dev: client chunk ${f.name} carries the secret`);
    }
  } finally {
    srv.kill("SIGKILL");
    await new Promise((r) => srv.once("exit", r));
  }

  fs.writeFileSync(path.join(app, ".env.production"), `UNPREFIXED_SECRET=${SECRET}\n`);
  fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ build: { prerender: ["/"] } }));
  execSync(`"${oj}" build "${app}"`, { stdio: "inherit" });
  const page = text(fs.readFileSync(path.join(app, "dist", "client", "index.html"), "utf8"));
  must(
    page.includes(`secret=${SECRET}`),
    `prerender: process.env lacks the .env var:\n${page.match(/secret=[^<]*/)?.[0]}`,
  );
  for (const f of fs.readdirSync(path.join(app, "dist", "client", "assets"))) {
    if (f.endsWith(".js")) {
      must(
        !fs.readFileSync(path.join(app, "dist", "client", "assets", f), "utf8").includes(SECRET),
        `build: ${f} carries the secret`,
      );
    }
  }
  console.log("START-DOTENV-PROCESS-ENV E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-DOTENV-PROCESS-ENV E2E FAILED:", err && err.stack ? err.stack : err);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
