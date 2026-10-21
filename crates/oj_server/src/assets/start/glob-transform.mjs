// SPDX-License-Identifier: MIT

import { existsSync, statSync } from "node:fs";
import { resolve, dirname, join, relative, sep } from "node:path";
import { globFiles, toAbsoluteGlob } from "./vite-globs.mjs";

const GLOB_HEAD = /import\.meta\.glob\b/g;

// Skip whitespace, then an optional balanced TypeScript type argument
// (`<...>`, nesting- and string-aware, e.g. `<Record<string, unknown>>`), then
// require `(`. Returns the index just past `(`, or -1 if this is not a call.
function callArgsStart(code, from) {
  let i = from;
  while (i < code.length && /\s/.test(code[i])) i++;
  if (code[i] === "<") {
    let depth = 0,
      str = "";
    for (; i < code.length; i++) {
      const c = code[i];
      if (str) {
        if (c === "\\") i++;
        else if (c === str) str = "";
        continue;
      }
      if (c === '"' || c === "'" || c === "`") str = c;
      else if (c === "<") depth++;
      else if (c === ">") {
        depth--;
        if (depth === 0) {
          i++;
          break;
        }
      }
    }
    while (i < code.length && /\s/.test(code[i])) i++;
  }
  return code[i] === "(" ? i + 1 : -1;
}

const toRel = (fileDir, abs) => {
  let r = relative(fileDir, abs).split(sep).join("/");
  if (!r.startsWith(".")) r = "./" + r;
  return r;
};

// The key of a matched file, as Vite writes it: relative to the importer for
// relative patterns, relative to `base` when that option is set, and
// root-relative (`/src/...`) for `/`-prefixed and other absolute patterns.
function keyOf(file, { fileDir, root, base, isRelative }) {
  if (base) return toRel(base[0] === "/" ? join(root, base) : resolve(fileDir, base), file);
  if (isRelative) return toRel(fileDir, file);
  const r = relative(root, file).split(sep).join("/");
  return r.startsWith("./") || r.startsWith("../") ? r : "/" + r;
}

// Vite's deprecated `as` option: it is `query` under another name, and
// `as: 'raw' | 'url'` forces `import: 'default'` (Vite rejects anything else).
const FORCE_DEFAULT_AS = ["raw", "url"];
function normalizeOptions(opts) {
  const out = { ...opts };
  if (typeof out.as === "string") {
    if (typeof out.query === "string" || (out.query && typeof out.query === "object")) {
      throw new Error('Options "as" and "query" cannot be used together');
    }
    if (FORCE_DEFAULT_AS.includes(out.as)) {
      if (out.import && out.import !== "default" && out.import !== "*") {
        throw new Error(
          `Option "import" can only be "default" or "*" when "as" is "${out.as}", but got "${out.import}"`,
        );
      }
      out.import = "default";
    }
    out.query = out.as;
  }
  if (
    typeof out.base === "string" &&
    out.base[0] !== "/" &&
    !out.base.startsWith("./") &&
    !out.base.startsWith("../")
  ) {
    throw new Error(`Option "base" must start with '/', './' or '../', but got "${out.base}"`);
  }
  return out;
}

// `scopes` (a Set, optional): collects each glob call as Vite matches added
// files against it (JSON of `{ patterns, exhaustive, caseSensitive }`, the
// patterns resolved as Vite resolves them). The dev rebundle gate
// (oj_compiler::glob::GlobMatcher) consumes them: Vite re-expands a glob's
// importer when an added file matches its patterns, and nothing else about a
// new file changes a bundle whose inputs all resolved. Vite's glob libraries
// must be loaded first (vite-globs.mjs `loadViteGlobs`).
export function transformGlob(code, filePath, root = process.env.OJ_APP_ROOT ?? process.cwd(), scopes = null) {
  if (!code.includes("import.meta.glob")) return code;
  const fileDir = dirname(filePath);
  const prelude = [];
  let g = 0,
    out = "",
    last = 0,
    m;
  GLOB_HEAD.lastIndex = 0;
  while ((m = GLOB_HEAD.exec(code))) {
    const argsStart = callArgsStart(code, GLOB_HEAD.lastIndex);
    if (argsStart === -1) continue;
    let i = argsStart,
      depth = 1,
      str = "";
    for (; i < code.length && depth > 0; i++) {
      const c = code[i];
      if (str) {
        if (c === "\\") i++;
        else if (c === str) str = "";
        continue;
      }
      if (c === '"' || c === "'" || c === "`") str = c;
      else if (c === "(") depth++;
      else if (c === ")") depth--;
    }
    const argsSrc = code.slice(argsStart, i - 1);
    let args;
    try {
      args = new Function("return [" + argsSrc + "]")();
    } catch {
      continue;
    }
    GLOB_HEAD.lastIndex = i;
    const patterns = (Array.isArray(args[0]) ? args[0] : [args[0]]).filter((p) => typeof p === "string");
    let opts;
    try {
      opts = normalizeOptions(args[1] && typeof args[1] === "object" ? args[1] : {});
    } catch (e) {
      throw new Error(`${e.message} (import.meta.glob in ${filePath})`);
    }
    const exhaustive = opts.exhaustive === true;
    const caseSensitive = opts.caseSensitive !== false;
    const base = typeof opts.base === "string" ? opts.base : null;
    const isRelative = patterns.every((p) => ".!".includes(p[0]));
    const globsResolved = patterns.map((p) => toAbsoluteGlob(p, fileDir, root, base));
    scopes?.add(JSON.stringify({ patterns: globsResolved, exhaustive, caseSensitive }));
    const files = globFiles(globsResolved, { exhaustive, caseSensitive, cwd: root }).filter((f) => f !== filePath);
    let query =
      typeof opts.query === "string"
        ? opts.query
        : opts.query && typeof opts.query === "object"
          ? `?${new URLSearchParams(opts.query)}`
          : "";
    if (query && query[0] !== "?") query = `?${query}`;
    // `import: '*'` is the whole namespace, like no `import` at all.
    const importName = typeof opts.import === "string" && opts.import !== "*" ? opts.import : null;
    const wantDefault = importName === "default";
    const entries = files.map((f, idx) => {
      const rel = toRel(fileDir, f);
      const spec = rel + query;
      const key = JSON.stringify(keyOf(f, { fileDir, root, base, isRelative }));
      if (opts.eager) {
        const id = `__oj_glob${g}_${idx}`;
        prelude.push(
          wantDefault
            ? `import ${id} from ${JSON.stringify(spec)};`
            : importName
              ? `import { ${importName} as ${id} } from ${JSON.stringify(spec)};`
              : `import * as ${id} from ${JSON.stringify(spec)};`,
        );
        return `${key}: ${id}`;
      }
      const imp = wantDefault
        ? `() => import(${JSON.stringify(spec)}).then((m) => m.default)`
        : importName
          ? `() => import(${JSON.stringify(spec)}).then((m) => m[${JSON.stringify(importName)}])`
          : `() => import(${JSON.stringify(spec)})`;
      return `${key}: ${imp}`;
    });
    // Line-preserving: a call that spanned several lines keeps its line breaks
    // (inside the braces), and the eager imports share the first line, so the
    // transform's source map still points at the right lines of the original.
    const spanned = code.slice(m.index, i).split("\n").length - 1;
    out += code.slice(last, m.index) + `{${entries.join(", ")}${"\n".repeat(spanned)}}`;
    last = i;
    g++;
  }
  out += code.slice(last);
  return prelude.length ? prelude.join(" ") + " " + out : out;
}

// Vite's workerImportMetaUrlRE and assetImportMetaUrlRE, matched on the code
// with comments and string contents blanked (stripLiteral), so a commented-out
// call or one spelled inside a string never matches.
const WORKER_META_URL_RE =
  /\bnew\s+(?:Worker|SharedWorker)\s*\(\s*(new\s+URL\s*\(\s*('[^']+'|"[^"]+"|`[^`]+`)\s*,\s*import\.meta\.url\s*(?:,\s*)?\))/dg;
const ASSET_META_URL_RE = /\bnew\s+URL\s*\(\s*('[^']+'|"[^"]+"|`[^`]+`)\s*,\s*import\.meta\.url\s*(?:,\s*)?\)/dg;
const VITE_IGNORE_RE = /\/\*\s*@vite-ignore\s*\*\//;

// strip-literal's contract on the code (Vite's stripLiteral): every comment,
// string body, template text and regex literal body replaced by spaces,
// offsets and line breaks kept. A template's `${...}` code stays, as does
// everything else that is code.
const REGEX_AFTER_WORD = new Set([
  "return",
  "typeof",
  "instanceof",
  "in",
  "of",
  "new",
  "delete",
  "void",
  "throw",
  "case",
  "do",
  "else",
  "yield",
  "await",
]);
export function stripLiterals(code) {
  const out = code.split("");
  const blank = (from, to) => {
    for (let k = from; k < to; k++) if (out[k] !== "\n") out[k] = " ";
  };
  // Open `${` expressions, each with the `{` depth inside it.
  const templates = [];
  // A template's text from `from` to its closing backtick or next `${`.
  const quasi = (from) => {
    let j = from;
    while (j < code.length) {
      if (code[j] === "\\") j += 2;
      else if (code[j] === "`") {
        blank(from, j);
        return j + 1;
      } else if (code[j] === "$" && code[j + 1] === "{") {
        blank(from, j);
        templates.push(0);
        return j + 2;
      } else j++;
    }
    blank(from, code.length);
    return code.length;
  };
  // js-tokens' regex-or-division call, from the last significant token.
  const regexAllowed = (at) => {
    let k = at - 1;
    while (k >= 0 && /\s/.test(out[k])) k--;
    if (k < 0) return true;
    const ch = out[k];
    if (/[\w$]/.test(ch)) {
      let w = k;
      while (w > 0 && /[\w$]/.test(out[w - 1])) w--;
      return REGEX_AFTER_WORD.has(out.slice(w, k + 1).join(""));
    }
    return !")]}".includes(ch) && !/["'`]/.test(ch);
  };
  let i = 0;
  while (i < code.length) {
    const c = code[i];
    if (c === "/" && code[i + 1] === "/") {
      const end = code.indexOf("\n", i);
      const stop = end === -1 ? code.length : end;
      blank(i, stop);
      i = stop;
    } else if (c === "/" && code[i + 1] === "*") {
      const end = code.indexOf("*/", i + 2);
      const stop = end === -1 ? code.length : end + 2;
      blank(i, stop);
      i = stop;
    } else if (c === '"' || c === "'") {
      let j = i + 1;
      while (j < code.length && code[j] !== c && code[j] !== "\n") j += code[j] === "\\" ? 2 : 1;
      // strip-literal keeps an unclosed string as written.
      if (code[j] === c) blank(i + 1, j);
      i = j + 1;
    } else if (c === "`") {
      i = quasi(i + 1);
    } else if (c === "}" && templates.length && templates[templates.length - 1] === 0) {
      templates.pop();
      i = quasi(i + 1);
    } else if (c === "/" && regexAllowed(i)) {
      let j = i + 1,
        cls = false;
      while (j < code.length && code[j] !== "\n" && (cls || code[j] !== "/")) {
        if (code[j] === "\\") j++;
        else if (code[j] === "[") cls = true;
        else if (code[j] === "]") cls = false;
        j++;
      }
      if (code[j] === "/") blank(i + 1, j);
      i = j + 1;
    } else {
      if (templates.length && c === "{") templates[templates.length - 1]++;
      else if (templates.length && c === "}") templates[templates.length - 1]--;
      i++;
    }
  }
  return out.join("");
}

// Vite's buildGlobPattern: a template's text with each `${...}` run as one `*`.
function templateGlob(raw) {
  const body = raw.slice(1, -1);
  let pattern = "",
    i = 0;
  while (i < body.length) {
    if (body[i] === "$" && body[i + 1] === "{") {
      let depth = 1;
      i += 2;
      for (; i < body.length && depth > 0; i++) {
        if (body[i] === "{") depth++;
        else if (body[i] === "}") depth--;
      }
      if (!pattern.endsWith("*")) pattern += "*";
    } else pattern += body[i++];
  }
  return pattern;
}

// Vite's getQueryDelimiterIndex: the first `?` outside a `${...}`.
function queryDelimiter(raw) {
  let depth = 0;
  for (let i = 0; i < raw.length; i++) {
    if (raw[i] === "{") depth++;
    else if (raw[i] === "}") depth--;
    else if (raw[i] === "?" && depth === 0) return i;
  }
  return -1;
}

// Vite's injectQuery: the new query first, then the URL's own.
const injectQuery = (file, postfix, query) =>
  `${file}?${query}${postfix[0] === "?" ? `&${postfix.slice(1)}` : postfix}`;

const splitPostfix = (url) => {
  const i = url.search(/[?#]/);
  return i === -1 ? { file: url, postfix: "" } : { file: url.slice(0, i), postfix: url.slice(i) };
};

// Vite's vite:worker-import-meta-url then vite:asset-import-meta-url, for a
// client bundle. `new Worker(new URL("./w.ts", import.meta.url))` (or
// SharedWorker) points at the worker's `?worker&url` import, the URL oj's
// asset plugin gives a worker (the dev pipeline's in dev, the bundled worker
// entry in a build); `new URL("./a.png", import.meta.url)` at the asset's
// `?url` import. Without this the URL stays relative to the bundled chunk
// (`/@oj-start/w.ts` in dev), which serves nothing. A template URL with
// substitutions becomes the eager `import.meta.glob` lookup Vite writes, which
// transformGlob expands after this. `resolve(spec, importer)` is the bundler's
// resolver (an absolute path, or null); a file it cannot resolve stays as
// written, resolved at runtime, with Vite's warning. Inside a worker bundle,
// `self` is the worker entry: a worker starting itself gets
// `self.location.href`, as in Vite.
export async function transformImportMetaUrl(
  code,
  id,
  resolve,
  root = process.env.OJ_APP_ROOT ?? process.cwd(),
  self = null,
) {
  if (!code.includes("import.meta.url")) return code;
  const clean = stripLiterals(code);
  const edits = [];
  const hoisted = [];
  const hoist = (prefix, spec) => {
    const ident = `__oj_${prefix}${hoisted.length}`;
    hoisted.push(`import ${ident} from ${JSON.stringify(spec)};`);
    return ident;
  };
  const resolved = async (spec) => {
    const r = await resolve(spec, id);
    return r && !r.startsWith("\0") ? r.replace(/[?#].*$/, "") : null;
  };
  // Vite's resolution for these URLs: a relative one from the importer; a
  // `/` one from the project root, else a public file served at that path
  // (left as written); any other with preferRelative, the importer's
  // directory before a package.
  const resolveFile = async (file) => {
    // A URL with a scheme (`https:`, `blob:`) is no file.
    if (SCHEME_RE.test(file)) return null;
    if (file[0] === ".") return resolved(file);
    if (file[0] === "/") {
      const abs = join(root, file);
      return existsSync(abs) && statSync(abs).isFile() ? abs : null;
    }
    return (await resolved("./" + file)) ?? (await resolved(file));
  };
  // Vite's warning for a file that is not there; a `/` URL left as written is
  // a public file, and a scheme URL no file at all.
  const missed = (file, s, e) => {
    if (file[0] === "/" || SCHEME_RE.test(file)) return;
    warnOnce(
      `${code.slice(s, e)} doesn't exist at build time, it will remain unchanged to be resolved at runtime. ` +
        `If this is intended, you can use the /* @vite-ignore */ comment to suppress this warning.`,
    );
  };

  const workers = new RegExp(WORKER_META_URL_RE);
  let m;
  while ((m = workers.exec(clean))) {
    const [[expStart, expEnd], [urlStart, urlEnd]] = m.indices.slice(1);
    const rawUrl = code.slice(urlStart, urlEnd);
    if (rawUrl[0] === "`" && rawUrl.includes("${")) {
      throw new Error(`\`new URL(url, import.meta.url)\` is not supported in dynamic template string. (${id})`);
    }
    const { file, postfix } = splitPostfix(rawUrl.slice(1, -1));
    const kind = /\bnew\s+SharedWorker/.test(clean.slice(m.index, expStart)) ? "sharedworker" : "worker";
    const abs = await resolveFile(file);
    if (!abs) {
      missed(file, expStart, expEnd);
      continue;
    }
    if (abs === self) {
      edits.push([expStart, expEnd, "self.location.href"]);
      continue;
    }
    const ident = hoist("worker_url", injectQuery(abs, postfix, `${kind}&url`));
    edits.push([expStart, expEnd, `new URL(/* @vite-ignore */ ${ident}, '' + import.meta.url)`]);
  }

  const assets = new RegExp(ASSET_META_URL_RE);
  while ((m = assets.exec(clean))) {
    const [[start, end], [urlStart, urlEnd]] = m.indices;
    // A worker's URL, rewritten above.
    if (edits.some(([s, e]) => start >= s && end <= e)) continue;
    if (VITE_IGNORE_RE.test(code.slice(start, urlStart))) continue;
    const rawUrl = code.slice(urlStart, urlEnd);
    if (rawUrl[0] === "`" && rawUrl.includes("${")) {
      const q = queryDelimiter(rawUrl);
      const pureUrl = q === -1 ? rawUrl : rawUrl.slice(0, q) + "`";
      const queryString = q === -1 ? "" : rawUrl.slice(q, -1);
      const pattern = templateGlob(pureUrl);
      // Vite leaves a pattern that starts with a wildcard alone.
      if (pattern[0] === "*") continue;
      const query = queryString ? `?url&${queryString.slice(1)}` : "?url";
      const opts = JSON.stringify({ eager: true, import: "default", query });
      edits.push([
        start,
        end,
        `new URL((import.meta.glob(${JSON.stringify(pattern)}, ${opts}))[${pureUrl}], import.meta.url)`,
      ]);
      continue;
    }
    const url = rawUrl.slice(1, -1);
    if (/^\s*data:/i.test(url)) continue;
    const { file, postfix } = splitPostfix(url);
    const abs = await resolveFile(file);
    if (!abs) {
      missed(file, start, end);
      continue;
    }
    // The query reaches oj's asset plugin as part of the id, as Vite's
    // fileToUrl sees it (`?inline` inlines); a hash stays on the URL.
    const hash = postfix[0] === "#" ? postfix : "";
    const ident = hoist("asset_url", injectQuery(abs, hash ? "" : postfix, "url"));
    const href = hash ? `${ident} + ${JSON.stringify(hash)}` : ident;
    edits.push([start, end, `new URL(${href}, '' + import.meta.url)`]);
  }

  if (!edits.length) return code;
  edits.sort((a, b) => a[0] - b[0]);
  let out = "",
    last = 0;
  for (const [s, e, text] of edits) {
    // Line-preserving, like transformGlob: the hoisted imports share line one.
    const spanned = code.slice(s, e).split("\n").length - 1;
    out += code.slice(last, s) + text + "\n".repeat(spanned);
    last = e;
  }
  out += code.slice(last);
  return hoisted.length ? hoisted.join(" ") + " " + out : out;
}

const SCHEME_RE = /^[a-z][a-z\d+.-]*:/i;

// Vite's logger.warnOnce.
const warned = new Set();
function warnOnce(message) {
  if (warned.has(message)) return;
  warned.add(message);
  console.warn(message);
}

// The rewrite for dependency modules, which the app-code transform skips:
// Vite runs both plugins on every client module, and a bundled dependency's
// relative URL breaks the same way. A template URL's glob is expanded here,
// as the app-code transform does after it. A worker bundle takes it for every
// module (`include`), with `self` its entry: Vite runs both plugins there too,
// and the emitted worker sits under `assets/`, away from its sources.
export function depImportMetaUrlPlugin({ include = /\/node_modules\/.*\.[cm]?[jt]sx?$/, self = null } = {}) {
  return {
    name: "oj:dep-import-meta-url",
    transform: {
      filter: { id: { include, exclude: /^\0/ }, code: { include: /import\.meta\.url/ } },
      async handler(code, id) {
        const out = await transformImportMetaUrl(code, id, bundlerResolve(this), undefined, self);
        return out === code ? null : transformGlob(out, id);
      },
    },
  };
}

// A plugin context's resolver as transformImportMetaUrl takes it.
export const bundlerResolve = (ctx) => async (spec, importer) => {
  const r = await ctx.resolve(spec, importer);
  return r && !r.external ? r.id : null;
};
