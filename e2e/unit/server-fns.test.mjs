// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  findServerFns,
  rewriteClientServerFns,
  rewriteServerServerFns,
  serverFnId,
} from "../../crates/oj_server/src/assets/start/server-fns.mjs";

const CLIENT_IMPORT = 'import { createClientRpc } from "@tanstack/react-start/client-rpc"; ';

// The client output, minus its import and exports, must still be JavaScript.
const parses = (out) =>
  new Function(
    out
      .slice(CLIENT_IMPORT.length)
      .replace(/\bexport\s+/g, "")
      .replace(/^import[^;]*;/gm, ""),
  );

// The shape that blanked a preview: a comment inside the handler with an
// unbalanced `)`. The paren count ended the handler there and left the rest
// of the comment and the body in the bundle (rolldown PARSE_ERROR).
test("a ) inside a handler comment does not end the handler", () => {
  const code = [
    'import { createServerFn } from "@tanstack/react-start";',
    'export const saveKey = createServerFn({ method: "POST" })',
    "  .handler(async ({ data }) => {",
    "    // 1) chave já gravada localmente para este ambiente",
    "    /* 2) senão, grava :) */",
    "    return data;",
    "  });",
    "export const after = 1;",
    "",
  ].join("\n");
  const out = rewriteClientServerFns(code, "src/qitech.functions.ts");
  const id = JSON.stringify(serverFnId("src/qitech.functions.ts", "saveKey"));
  assert.ok(out.startsWith(CLIENT_IMPORT), out);
  assert.ok(out.includes(`.handler(createClientRpc(${id})\n\n\n\n);`), out);
  assert.doesNotMatch(out, /chave|grava|return data/);
  assert.ok(out.includes("export const after = 1;"));
  assert.equal(out.split("\n").length, code.split("\n").length);
  parses(out);
});

test("strings, templates and regex literals with brackets stay inside the handler", () => {
  const code = [
    "export const run = createServerFn()",
    "  .handler(async () => {",
    '    const a = ")((";',
    "    const b = `x ${f(')')} )`;",
    "    if (/^[)]+\\)/.test(a)) return b;",
    "    return a;",
    "  });",
    "",
  ].join("\n");
  const out = rewriteClientServerFns(code, "src/run.ts");
  assert.ok(out.includes(`.handler(createClientRpc(${JSON.stringify(serverFnId("src/run.ts", "run"))})`), out);
  assert.doesNotMatch(out, /return a|\)\(\(/);
  parses(out);
});

// handleCreateServerFn replaces only handlerFnPath, the first argument.
test("only the handler's first argument becomes the RPC stub", () => {
  const out = rewriteClientServerFns("export const x = createServerFn().handler(async () => 1, extra);", "src/x.ts");
  assert.ok(out.endsWith(`.handler(createClientRpc(${JSON.stringify(serverFnId("src/x.ts", "x"))}), extra);`), out);
});

test("validator and middleware chains across lines and comments find their handler", () => {
  const code = [
    "export const getUser = createServerFn({",
    '  method: "GET",',
    "})",
    "  .middleware([auth]) // trailing ) comment",
    "  .validator((s) => {",
    '    if (!/^[(]/.test(s.name) && s.label !== ")((") return s;',
    "    throw new Error(`bad ${s.name}; (unbalanced`);",
    "  })",
    "  .handler(async ({ data }) => data);",
    "",
  ].join("\n");
  assert.deepEqual(
    findServerFns(code).map((f) => f.name),
    ["getUser"],
  );
  const out = rewriteClientServerFns(code, "src/user.ts");
  assert.ok(out.includes(`.handler(createClientRpc(${JSON.stringify(serverFnId("src/user.ts", "getUser"))}));`), out);
  parses(out);
});

// As the dev SSR loader (start_host.rs split_statement_factories_are_left_untouched):
// the client and the server must agree on the set, so neither guesses.
test("a split-statement factory is left alone, a chained neighbour still rewrites", () => {
  const split =
    'const factory = createServerFn({ method: "POST" });\nexport const doIt = factory.handler(async () => 1);\n';
  assert.equal(rewriteClientServerFns(split, "src/split.ts"), null);
  assert.equal(rewriteServerServerFns(split, "src/split.ts"), null);
  const mixed =
    'const factory = createServerFn({ method: "POST" });\nexport const real = createServerFn({ method: "GET" }).handler(async () => 2);\nexport const viaFactory = factory.handler(async () => 3);\n';
  assert.deepEqual(
    findServerFns(mixed).map((f) => f.name),
    ["real"],
  );
  assert.ok(rewriteClientServerFns(mixed, "src/mixed.ts").includes(".handler(async () => 3)"));
});

test("a commented-out server fn is not one", () => {
  const code = "// export const old = createServerFn().handler(async () => 1);\nexport const y = 2;\n";
  assert.deepEqual(findServerFns(code), []);
  assert.equal(rewriteClientServerFns(code, "src/y.ts"), null);
});

// Byte-for-byte what start_host.rs server_fn_rewrite_matches_the_node_loader
// expects, so a build serves the ids the dev SSR loader registers.
test("the server rewrite matches the dev SSR loader's", () => {
  const code =
    'import { createServerFn } from "@tanstack/react-start";\nexport const getGreeting = createServerFn({ method: "GET" }).handler(async () => "hi");\n';
  const out = rewriteServerServerFns(code, "src/server/data.ts");
  const id = Buffer.from("src/server/data.ts#getGreeting").toString("base64url");
  assert.ok(out.startsWith('import { createServerRpc } from "@tanstack/react-start/server-rpc"; '), out);
  assert.ok(
    out.includes(
      `export const getGreeting_createServerFn_handler = createServerRpc({ id: "${id}", name: "getGreeting", filename: "src/server/data.ts" }, (opts) => getGreeting.__executeServer(opts)); `,
    ),
    out,
  );
  assert.ok(out.includes('.handler(getGreeting_createServerFn_handler, async () => "hi")'), out);
  assert.equal(out.split("\n").length, code.split("\n").length);
});

test("the server rewrite keeps a commented handler body intact", () => {
  const code = "export const k = createServerFn().handler(async () => {\n  // 1) chave\n  return 1;\n});\n";
  const out = rewriteServerServerFns(code, "src/k.ts");
  assert.ok(out.includes(".handler(k_createServerFn_handler, async () => {\n  // 1) chave\n  return 1;\n});"), out);
});

test("modules without a server fn pass through", () => {
  assert.equal(rewriteClientServerFns("export const x = 1;", "src/x.ts"), null);
  assert.equal(rewriteServerServerFns("export const x = 1;", "src/x.ts"), null);
});
