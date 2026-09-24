// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Build-side hook gating: the Rust gate skips plugin RPCs a filter plan rules
// out, and must never skip one a plugin would have acted on. This app covers
// every shape the gate distinguishes: function-form resolveId/load (unfiltered,
// always offered: the virtual module), a RegExp-filtered transform that matches
// (its marker must land in the output), a RegExp-filtered transform that
// matches nothing, a string-filtered transform (conservatively unfiltered in
// the plan, still filtered by the host), a RegExp-filtered load for a real
// import, and transformIndexHtml.
import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hookgate-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "hookgate-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "src", "entry.js"),
  `import { virt } from "virtual:gate-info";
import { special } from "./data.special";
console.log("gate-entry", virt, special);
`,
);
// Resolved from disk like any file; the RegExp-filtered load then supplies its
// real content, proving the load RPC crossed for a matching id.
fs.writeFileSync(path.join(app, "src", "data.special"), "placeholder\n");
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [
  {
    // Function-form hooks carry no filter, so the gate must always offer them
    // every specifier and id; the virtual module only builds if they crossed.
    name: "gate-virtual",
    resolveId(id) {
      if (id === "virtual:gate-info") return "\\0gate-info";
    },
    load(id) {
      if (id === "\\0gate-info") return 'export const virt = "virtual-crossed";';
    },
  },
  {
    name: "gate-special-load",
    load: {
      filter: { id: /\\.special$/ },
      handler(id) {
        return 'export const special = "special-load-crossed";';
      },
    },
  },
  {
    name: "gate-transform-match",
    transform: {
      filter: { id: /entry\\.js$/ },
      handler(code) {
        return { code: code + '\\nconsole.log("regex-transform-crossed");', map: null };
      },
    },
  },
  {
    name: "gate-transform-never",
    transform: {
      filter: { id: /\\.does-not-exist$/ },
      handler(code) {
        return { code: code + '\\nconsole.log("never-transform-LEAKED");', map: null };
      },
    },
  },
  {
    name: "gate-transform-string-filter",
    transform: {
      // A string pattern is a glob to rolldown, so the plan treats this plugin
      // as unfiltered; the host's own filter must still scope it to the entry.
      filter: { id: "**/entry.js" },
      handler(code, id) {
        if (!id.endsWith("entry.js")) return null;
        return { code: code + '\\nconsole.log("string-transform-crossed");', map: null };
      },
    },
  },
  {
    name: "gate-html",
    transformIndexHtml(html) {
      return html.replace("</head>", '<meta name="gate" content="html-crossed" /></head>');
    },
  },
];
`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "pipe" });

  const dist = path.join(app, "dist");
  const assets = fs
    .readdirSync(path.join(dist, "assets"))
    .filter((f) => f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(dist, "assets", f), "utf8"))
    .join("\n");
  assert.match(assets, /virtual-crossed/, "function-form resolveId+load crossed the gate");
  assert.match(assets, /special-load-crossed/, "RegExp-filtered load crossed for a matching id");
  assert.match(assets, /regex-transform-crossed/, "RegExp-filtered transform crossed for a matching id");
  assert.match(assets, /string-transform-crossed/, "string-filtered plugin stayed unfiltered and crossed");
  assert.doesNotMatch(assets, /never-transform-LEAKED/, "non-matching filtered transform did not fire");

  const html = fs.readFileSync(path.join(dist, "index.html"), "utf8");
  assert.match(html, /html-crossed/, "transformIndexHtml still runs");

  console.log("PLUGIN HOOK GATING BUILD VERIFIED");
} catch (e) {
  failed = true;
  console.error(e && e.stderr ? e.stderr.toString() : e);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
