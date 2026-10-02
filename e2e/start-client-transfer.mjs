// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start dev: the client bundle chunks carry a content-hash ETag (a
// reload revalidates to a 304), gzip when the browser accepts it, and a new
// ETag once an edit rebundles, so a reload never keeps a stale entry.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import zlib from "node:zlib";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 3099);
const ENTRY = `http://localhost:${PORT}/@oj-start/client-entry.js`;

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start client transfer: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const about = path.join(app, "src", "routes", "about.tsx");
const original = fs.readFileSync(about, "utf8");
fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);

  const plain = await fetch(ENTRY, { headers: { "accept-encoding": "identity" } });
  must(plain.status === 200, `entry returned ${plain.status}`);
  const etag = plain.headers.get("etag");
  must(/^W\/"[^"]+"$/.test(etag ?? ""), `entry has no ETag: ${etag}`);
  must(plain.headers.get("cache-control") === "no-cache", "the entry must revalidate, it keeps a fixed name");
  must(!plain.headers.get("content-encoding"), "gzip served to a client that did not ask for it");
  const body = await plain.text();

  const revalidated = await fetch(ENTRY, { headers: { "if-none-match": etag } });
  must(revalidated.status === 304, `matching If-None-Match returned ${revalidated.status}`);

  // fetch() decodes transparently; read the raw frame through node's http.
  const raw = await new Promise((resolve, reject) => {
    import("node:http").then(({ get }) =>
      get(ENTRY, { headers: { "accept-encoding": "gzip" } }, (res) => {
        const parts = [];
        res.on("data", (c) => parts.push(c));
        res.on("end", () => resolve({ headers: res.headers, bytes: Buffer.concat(parts) }));
      }).on("error", reject),
    );
  });
  must(raw.headers["content-encoding"] === "gzip", "entry not gzipped for a gzip client");
  must(zlib.gunzipSync(raw.bytes).toString() === body, "gzip body does not round-trip to the plain entry");
  must(raw.bytes.length < body.length / 2, "gzip body is not meaningfully smaller");

  const edit = () => fs.writeFileSync(about, original.replace("about-page-marker", "about-page-edited"));
  edit();
  // An error response carries no ETag: only a served entry with a different
  // one counts as rebundled, so a transient failure can't pass this vacuously.
  const rotated = await settles(
    async () => {
      const r = await fetch(ENTRY, { headers: { "if-none-match": etag } });
      if (r.status !== 200 && r.status !== 304) return false;
      const next = r.headers.get("etag");
      return next !== null && next !== etag;
    },
    { pollMs: 200, touch: edit },
  );
  must(rotated, "the entry ETag did not change after an edit rebundled it");
  console.log("start-dev: client chunks revalidate by ETag, gzip, and change ETag on edit");
} finally {
  fs.writeFileSync(about, original);
  srv.kill("SIGKILL");
  await sleep(300);
}
