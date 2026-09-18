// SPDX-License-Identifier: MIT

import { readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const APP = process.env.OJ_APP_ROOT ?? process.cwd();
// Spelled without `new URL(..., import.meta.url)`: the compile pipeline
// hoists that form into an asset import, and ./client only exists in a
// production dist (this file is copied there), never beside the dev copy.
const CLIENT_DIR = join(dirname(fileURLToPath(import.meta.url)), "client");

function stripJsonc(s) {
  let out = "", i = 0, inStr = false, q = "";
  while (i < s.length) {
    const c = s[i], n = s[i + 1];
    if (inStr) {
      out += c;
      if (c === "\\") { out += n ?? ""; i += 2; continue; }
      if (c === q) inStr = false;
      i++; continue;
    }
    if (c === '"' || c === "'") { inStr = true; q = c; out += c; i++; continue; }
    if (c === "/" && n === "/") { while (i < s.length && s[i] !== "\n") i++; continue; }
    if (c === "/" && n === "*") { i += 2; while (i < s.length && !(s[i] === "*" && s[i + 1] === "/")) i++; i += 2; continue; }
    out += c; i++;
  }
  return out;
}

function parseWranglerJsonVars(text) {
  try {
    const cfg = JSON.parse(stripJsonc(text).replace(/,(\s*[}\]])/g, "$1"));
    return cfg.vars && typeof cfg.vars === "object" ? cfg.vars : {};
  } catch {
    return {};
  }
}

function parseWranglerTomlVars(text) {
  const vars = {};
  let inVars = false;
  for (const line of text.split("\n")) {
    const t = line.trim();
    if (t.startsWith("[")) { inVars = t === "[vars]"; continue; }
    const m = inVars && t.match(/^([A-Za-z_][\w]*)\s*=\s*"([^"]*)"/);
    if (m) vars[m[1]] = m[2];
  }
  return vars;
}

function parseDevVars(text) {
  const vars = {};
  for (const line of text.split("\n")) {
    const t = line.trim();
    if (!t || t.startsWith("#")) continue;
    const eq = t.indexOf("=");
    if (eq === -1) continue;
    let v = t.slice(eq + 1).trim();
    if ((v.startsWith('"') && v.endsWith('"')) || (v.startsWith("'") && v.endsWith("'"))) v = v.slice(1, -1);
    vars[t.slice(0, eq).trim()] = v;
  }
  return vars;
}

function wranglerVars() {
  for (const f of ["wrangler.jsonc", "wrangler.json"]) {
    const p = join(APP, f);
    if (existsSync(p)) return parseWranglerJsonVars(readFileSync(p, "utf8"));
  }
  const toml = join(APP, "wrangler.toml");
  if (existsSync(toml)) return parseWranglerTomlVars(readFileSync(toml, "utf8"));
  return {};
}

function devVars() {
  const p = join(APP, ".dev.vars");
  if (!existsSync(p)) return {};
  return parseDevVars(readFileSync(p, "utf8"));
}

const ASSETS = {
  async fetch(url) {
    const pathname = new URL(url).pathname.replace(/^\/+/, "");
    const file = join(CLIENT_DIR, pathname);
    if (!file.startsWith(CLIENT_DIR)) return new Response("", { status: 403 });
    try {
      return new Response(readFileSync(file), { status: 200 });
    } catch {
      return new Response("", { status: 404 });
    }
  },
};

// The dev `env`: the wrangler vars (and `.dev.vars`) over the process env, plus
// the ASSETS binding. Shared with the `cloudflare:workers` scheme stub
// (cf-workers.mjs) so a server function reading `env` sees the same values
// whether it imports the plugin's server helper or the runtime module.
export function cloudflareEnv() {
  return { ASSETS, ...process.env, ...wranglerVars(), ...devVars() };
}

let cached;
export async function getCloudflareContext() {
  if (!cached) {
    cached = {
      env: cloudflareEnv(),
      cf: {},
      ctx: { waitUntil() {}, passThroughOnException() {} },
    };
  }
  return cached;
}

export default { getCloudflareContext };

export const __test = { stripJsonc, parseWranglerJsonVars, parseWranglerTomlVars, parseDevVars };
