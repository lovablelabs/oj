// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Writes each case's `<name>.babel.<ext>`: what TanStack's dead-code pass
// (babel-dead-code-elimination, as @tanstack/router-utils runs it) makes of
// the case after the same edit oj's parity test makes, the marked expression
// (`/*<*/ ... /*>*/`) replaced by `undefined`:
// findReferencedIdentifiers on the module, the edit, deadCodeElimination.
//
//   npm ci --prefix e2e/fixtures/start-app
//   node crates/oj_compiler/tests/dce_parity/generate.mjs

import { createRequire } from "node:module";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const fixture = path.resolve(here, "../../../../e2e/fixtures/start-app/node_modules");
const req = createRequire(path.join(fixture, "@tanstack/router-utils/package.json"));
const { parse } = req("@babel/parser");
const traverse = req("@babel/traverse").default;
const generate = req("@babel/generator").default;
const t = req("@babel/types");
const routerUtils = await import(pathToFileURL(path.join(fixture, "@tanstack/router-utils/dist/esm/ast.js")).href);

const OPEN = "/*<*/";
const CLOSE = "/*>*/";

for (const file of fs.readdirSync(here).sort()) {
  if (!/\.(ts|tsx)$/.test(file) || file.includes(".babel.")) continue;
  const marked = fs.readFileSync(path.join(here, file), "utf8");
  const start = marked.indexOf(OPEN);
  const end = marked.indexOf(CLOSE) - OPEN.length;
  const code = marked.replace(OPEN, "").replace(CLOSE, "");
  const plugins = file.endsWith(".tsx") ? ["typescript", "jsx"] : ["typescript"];
  const ast = parse(code, { sourceType: "module", plugins });
  const refs = routerUtils.findReferencedIdentifiers(ast);
  let edited = false;
  traverse(ast, {
    Expression(p) {
      if (p.node.start === start && p.node.end === end) {
        p.replaceWith(t.identifier("undefined"));
        edited = true;
        p.stop();
      }
    },
  });
  if (!edited) throw new Error(`${file}: no expression spans the marked range`);
  routerUtils.deadCodeElimination(ast, refs);
  const out = file.replace(/\.(tsx?)$/, ".babel.$1");
  fs.writeFileSync(path.join(here, out), generate(ast).code + "\n");
  console.log(out);
}
