// SPDX-License-Identifier: MIT

// The `createServerFn` rewrites outside the dev SSR loader: the client RPC
// stub (dev and build bundles), the build's server export, and the build
// manifest. Which calls are server functions, and their ids, come from a
// port of start_host.rs `rewrite_server_fns`, which the dev SSR loader runs,
// so every side agrees: it follows the `createServerFn(...)...handler(` chain
// textually with strings, template literals, comments and regex literals
// skipped, so a `)` in a comment (`// 1) read the key`) cannot end it early.
// The client rewrite then edits the AST, as TanStack's compiler does.

const DECL_RE = /(?:^|[\n;])[ \t]*((?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn)\b/g;
const REGEX_AFTER = new Set("([{,;:!&|?=+-*%<>~^");

export const serverFnId = (rel, name) => Buffer.from(`${rel}#${name}`).toString("base64url");

// Every `const NAME = createServerFn(...)...handler(` in `code`: the name, the
// declaration's start, and the offset just past the handler's `(`. A chain
// that ends before a `.handler(` of the same expression is skipped, as in
// the SSR loader (TanStack resolves split shapes through bindings).
export function findServerFns(code) {
  if (!code.includes("createServerFn")) return [];
  const out = [];
  DECL_RE.lastIndex = 0;
  let m;
  while ((m = DECL_RE.exec(code))) {
    const declStart = m.index + m[0].length - m[1].length;
    const argsAt = chainToHandler(code, m.index + m[0].length);
    if (argsAt != null) out.push({ name: m[2], declStart, argsAt });
  }
  return out;
}

// The client caller, edited on the module's AST as TanStack's
// handleCreateServerFn edits its Babel AST: the handler's first argument
// becomes `createClientRpc(id)` (handlerFnPath.replaceWith) and a
// `.validator()` / `.inputValidator()` call is dropped (stripMethodCall), so
// neither the handler nor the validator ships to the browser. A text scan
// cannot delimit a handler body (`return /\)/`, a `(` in JSX text), and a
// handler it gave up on would have shipped whole. `parse(code)` is
// rolldown's `parseAst` for `rel`: an ESTree program with UTF-16 offsets.
// Line counts are kept so stack traces and source maps stay put. Null when
// the module has no server function; throws when one cannot be rewritten.
export function rewriteClientServerFns(code, rel, parse) {
  const fns = findServerFns(code);
  if (!fns.length) return null;
  const ast = parse(code);
  const handlers = [];
  walk(ast, (node) => {
    if (isMethodCall(node, "handler") && createServerFnChain(node)) handlers.push(node);
  });
  const edits = [];
  for (const fn of fns) {
    const call = handlers.find((c) => c.callee.property.end < fn.argsAt && fn.argsAt <= firstArgStart(c));
    const arg = call?.arguments[0];
    if (!arg) throw new Error(`oj: could not rewrite server function ${fn.name} in ${rel} for the client`);
    edits.push([arg.start, arg.end, `createClientRpc(${JSON.stringify(serverFnId(rel, fn.name))})`]);
    for (let link = call.callee.object; isMethodCall(link); link = link.callee.object) {
      const method = link.callee.property.name;
      if (method === "validator" || method === "inputValidator") edits.push([link.callee.object.end, link.end, ""]);
    }
  }
  edits.sort((a, b) => b[0] - a[0]);
  let out = code;
  for (const [s, e, text] of edits) {
    const lines = code.slice(s, e).split("\n").length - 1;
    out = out.slice(0, s) + text + "\n".repeat(lines) + out.slice(e);
  }
  // After the directive prologue, on its line: the rewrite must not shift
  // source lines, nor turn a `"use client"` into a plain expression.
  const at =
    ast.body.filter((s) => s.directive != null).at(-1)?.end ?? (code.startsWith("#!") ? code.indexOf("\n") + 1 : 0);
  const sep = at > 0 && code[at - 1] !== "\n" ? " " : "";
  return `${out.slice(0, at)}${sep}import { createClientRpc } from "@tanstack/react-start/client-rpc"; ${out.slice(at)}`;
}

const isMethodCall = (node, name) =>
  node?.type === "CallExpression" &&
  node.callee?.type === "MemberExpression" &&
  !node.callee.computed &&
  node.callee.property?.type === "Identifier" &&
  (name == null || node.callee.property.name === name);

// Whether the method chain under `call` starts at a `createServerFn(...)` call.
function createServerFnChain(call) {
  let node = call.callee.object;
  while (isMethodCall(node)) node = node.callee.object;
  return node?.type === "CallExpression" && node.callee?.type === "Identifier" && node.callee.name === "createServerFn";
}

const firstArgStart = (call) => call.arguments[0]?.start ?? call.end;

function walk(node, visit) {
  if (Array.isArray(node)) {
    for (const child of node) walk(child, visit);
    return;
  }
  if (node == null || typeof node !== "object" || typeof node.type !== "string") return;
  visit(node);
  for (const key in node) {
    const value = node[key];
    if (value != null && typeof value === "object") walk(value, visit);
  }
}

// The server provider, exactly as the dev SSR loader writes it: a sibling
// `NAME_createServerFn_handler = createServerRpc(meta, ...)` export and that
// binding as the handler's first argument. Null when nothing matched.
export function rewriteServerServerFns(code, rel) {
  const edits = [];
  for (const { name, declStart, argsAt } of findServerFns(code)) {
    const meta = `{ id: ${JSON.stringify(serverFnId(rel, name))}, name: ${JSON.stringify(name)}, filename: ${JSON.stringify(rel)} }`;
    edits.push([
      declStart,
      `export const ${name}_createServerFn_handler = createServerRpc(${meta}, (opts) => ${name}.__executeServer(opts)); `,
    ]);
    edits.push([argsAt, `${name}_createServerFn_handler, `]);
  }
  if (!edits.length) return null;
  edits.sort((a, b) => a[0] - b[0]);
  let out = 'import { createServerRpc } from "@tanstack/react-start/server-rpc"; ',
    last = 0;
  for (const [at, text] of edits) {
    out += code.slice(last, at) + text;
    last = at;
  }
  return out + code.slice(last);
}

// From just past the identifier, the `.handler(` of the same chain: balanced
// calls joined by `.name`, anything else ends it (start_host.rs
// scan_chain_to_handler).
function chainToHandler(code, from) {
  let i = from;
  for (;;) {
    i = skipWsAndComments(code, i);
    if (i == null || code[i] !== "(") return null;
    i = skipBalanced(code, i);
    if (i == null) return null;
    i = skipWsAndComments(code, i);
    if (i == null || code[i] !== ".") return null;
    i = skipWsAndComments(code, i + 1);
    if (i == null) return null;
    const start = i;
    while (i < code.length && /[\w$]/.test(code[i])) i++;
    if (i === start) return null;
    const method = code.slice(start, i);
    i = skipWsAndComments(code, i);
    if (i == null || code[i] !== "(") return null;
    if (method === "handler") return i + 1;
  }
}

function skipWsAndComments(code, i) {
  for (;;) {
    while (i < code.length && /\s/.test(code[i])) i++;
    if (code[i] === "/" && code[i + 1] === "/") {
      while (i < code.length && code[i] !== "\n") i++;
      continue;
    }
    if (code[i] === "/" && code[i + 1] === "*") {
      const end = code.indexOf("*/", i + 2);
      if (end === -1) return null;
      i = end + 2;
      continue;
    }
    return i < code.length ? i : null;
  }
}

// From `(` at `open`, the offset just past its matching `)`.
function skipBalanced(code, open) {
  const close = scan(code, open + 1, ["("]);
  return close == null ? null : close + 1;
}

// Bracket-depth scan from `i` with `stack` open (start_host.rs
// skip_balanced_parens): strings, template literals with their `${}` code,
// comments and regex literals (the `/`-after-operator call) skipped. Returns
// the offset of the `)` that empties the stack; null on EOF or a mismatched
// bracket.
function scan(code, i, stack) {
  stack = [...stack];
  let prev = "(";
  while (i < code.length) {
    if (stack.at(-1) === "`") {
      const c = code[i];
      if (c === "\\") i += 2;
      else if (c === "`") {
        stack.pop();
        prev = '"';
        i++;
      } else if (c === "$" && code[i + 1] === "{") {
        stack.push("T");
        prev = "{";
        i += 2;
      } else i++;
      continue;
    }
    const c = code[i];
    if (c === "'" || c === '"') {
      i = skipString(code, i);
      if (i == null) return null;
      prev = '"';
    } else if (c === "`") {
      stack.push("`");
      i++;
    } else if (c === "/" && code[i + 1] === "/") {
      while (i < code.length && code[i] !== "\n") i++;
    } else if (c === "/" && code[i + 1] === "*") {
      const end = code.indexOf("*/", i + 2);
      if (end === -1) return null;
      i = end + 2;
    } else if (c === "/" && REGEX_AFTER.has(prev)) {
      i = skipRegex(code, i);
      if (i == null) return null;
      prev = "/";
    } else if (c === "(" || c === "[" || c === "{") {
      stack.push(c);
      prev = c;
      i++;
    } else if (c === ")" || c === "]") {
      if (stack.pop() !== (c === ")" ? "(" : "[")) return null;
      if (!stack.length) return i;
      prev = c;
      i++;
    } else if (c === "}") {
      const top = stack.pop();
      if (top !== "{" && top !== "T") return null;
      prev = c;
      i++;
    } else {
      if (!/\s/.test(c)) prev = c;
      i++;
    }
  }
  return null;
}

function skipString(code, start) {
  const quote = code[start];
  let i = start + 1;
  while (i < code.length) {
    const c = code[i];
    if (c === "\\") i += 2;
    else if (c === quote) return i + 1;
    // Unterminated: fail soft at the line end.
    else if (c === "\n") return i + 1;
    else i++;
  }
  return null;
}

function skipRegex(code, start) {
  let i = start + 1,
    cls = false;
  while (i < code.length) {
    const c = code[i];
    if (c === "\\") i += 2;
    else if (c === "[") {
      cls = true;
      i++;
    } else if (c === "]" && cls) {
      cls = false;
      i++;
    } else if (c === "/" && !cls) return i + 1;
    // Not a regex after all.
    else if (c === "\n") return null;
    else i++;
  }
  return null;
}
