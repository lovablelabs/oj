// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The names a CommonJS dep's module.exports carries at runtime, for the
// named-export facade oj writes next to a CJS React-family pre-bundle.
// Requiring is the only reliable read: React's entry is
// `module.exports = require(env ? a : b)`, which no static pass sees through.
// Runs on a short-lived engine; each dep is required once and dropped.

import { createRequire } from "node:module";
import path from "node:path";

const IDENT = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

function namesOf(req, dep) {
  try {
    const m = req(dep);
    const mod = m && m.__esModule && m.default && typeof m.default === "object" ? m.default : m;
    if (!mod || (typeof mod !== "object" && typeof mod !== "function")) return [];
    return [...new Set(Object.keys(mod))].filter((k) => k !== "default" && k !== "__esModule" && IDENT.test(k));
  } catch {
    return [];
  }
}

export function namedExports({ root, deps }) {
  const req = createRequire(path.join(root, "package.json"));
  return Object.fromEntries(deps.map((dep) => [dep, namesOf(req, dep)]));
}
