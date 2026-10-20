// SPDX-License-Identifier: MIT

import { readdirSync, statSync } from "node:fs";
import { resolve, dirname, join, relative, sep } from "node:path";

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

function globToRegExp(absGlob, flags = "") {
  let re = "";
  for (let i = 0; i < absGlob.length; i++) {
    const c = absGlob[i];
    if (c === "*") {
      if (absGlob[i + 1] === "*") {
        re += "(?:.*)";
        i++;
        if (absGlob[i + 1] === "/") i++;
      } else {
        re += "[^/]*";
      }
    } else if (c === "?") {
      re += "[^/]";
    } else if (c === "{") {
      const end = absGlob.indexOf("}", i);
      if (end === -1) {
        re += "\\{";
        continue;
      }
      re +=
        "(?:" +
        absGlob
          .slice(i + 1, end)
          .split(",")
          .map((s) => s.replace(/[.+^${}()|[\]\\]/g, "\\$&"))
          .join("|") +
        ")";
      i = end;
    } else if ("+^$.()|[]\\".includes(c)) {
      re += "\\" + c;
    } else {
      re += c;
    }
  }
  return new RegExp("^" + re + "$", flags);
}

function walk(dir, out) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const e of entries) {
    const p = dir + sep + e.name;
    if (e.isDirectory()) walk(p, out);
    else if (e.isFile()) out.push(p);
  }
}

// Vite's toAbsoluteGlob: `/x` is project-root relative, `./x` and `../x`
// resolve against the importer's directory or, with the `base` option,
// against that base (itself root-relative when it starts with `/`); anything
// else (an alias, a bare id) keeps resolving against the importer's directory.
function absGlobOf(pattern, fileDir, root, base) {
  const dir = base ? (base[0] === "/" ? join(root, base) : resolve(fileDir, base)) : fileDir;
  if (pattern.startsWith("/")) return join(root, pattern.slice(1));
  if (pattern.startsWith("./")) return join(dir, pattern.slice(2));
  if (pattern.startsWith("../")) return join(dir, pattern);
  return resolve(fileDir, pattern);
}

// The directory a glob's scan walks: up to the last separator before the
// first wildcard, or the pattern's own directory when it has none.
export function globBase(absGlob) {
  absGlob = absGlob.split(sep).join("/");
  const starIdx = absGlob.search(/[*?{]/);
  return starIdx === -1 ? dirname(absGlob) : absGlob.slice(0, absGlob.lastIndexOf("/", starIdx));
}

function matchPattern(absGlob, exhaustive = false, caseSensitive = true) {
  absGlob = absGlob.split(sep).join("/");
  const base = globBase(absGlob);
  // Vite: `caseSensitive: false` matches with nocase (an `Icon.svg` pattern
  // finds `icon.svg`).
  const re = globToRegExp(absGlob, caseSensitive ? "" : "i");
  const explicitHidden = absGlob
    .slice(base.length + 1)
    .split("/")
    .filter((part) => part.startsWith("."))
    .map((part) => globToRegExp(part));
  const all = [];
  walk(base.split("/").join(sep), all);
  return all.filter((f) => {
    const normalized = f.split(sep).join("/");
    if (!re.test(normalized)) return false;
    // Vite globs with `dot: !!exhaustive` and ignores node_modules unless
    // `exhaustive: true`; a literal dot segment in the pattern still matches.
    if (exhaustive) return true;
    const parts = normalized.slice(base.length + 1).split("/");
    if (parts.includes("node_modules")) return false;
    return parts.every((part) => !part.startsWith(".") || explicitHidden.some((pattern) => pattern.test(part)));
  });
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

// `scopes` (a Set, optional): collects every include pattern's scan base, the
// directories where a CREATED file can change a glob's expansion. The dev
// rebundle gate consumes them: Vite re-expands glob importers when a file is
// added under a globbed scope, and nothing else about a new file changes a
// bundle whose inputs all resolved.
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
    const includes = patterns.filter((p) => !p.startsWith("!"));
    const excludes = patterns.filter((p) => p.startsWith("!")).map((p) => p.slice(1));
    const exhaustive = opts.exhaustive === true;
    const caseSensitive = opts.caseSensitive !== false;
    const base = typeof opts.base === "string" ? opts.base : null;
    const isRelative = patterns.every((p) => ".!".includes(p[0]));
    if (scopes) for (const p of includes) scopes.add(globBase(absGlobOf(p, fileDir, root, base)));
    const match = (p) => matchPattern(absGlobOf(p, fileDir, root, base), exhaustive, caseSensitive);
    const exclude = new Set(excludes.flatMap(match));
    const files = [...new Set(includes.flatMap(match))].filter((f) => !exclude.has(f) && f !== filePath).sort();
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

// The code with every comment and every string or template literal's contents
// replaced by spaces, offsets and line breaks kept (strip-literal's contract).
// A template's `${...}` is blanked along with its text.
export function stripLiterals(code) {
  const out = code.split("");
  const blank = (from, to) => {
    for (let k = from; k < to; k++) if (out[k] !== "\n") out[k] = " ";
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
      blank(i + 1, Math.min(j, code.length));
      i = j + 1;
    } else if (c === "`") {
      const j = templateEnd(code, i);
      blank(i + 1, j);
      i = j + 1;
    } else i++;
  }
  return out.join("");
}

// The index of the backtick closing the template literal opened at `start`,
// stepping over nested `${...}` expressions and the templates inside them.
function templateEnd(code, start) {
  let i = start + 1;
  while (i < code.length) {
    const c = code[i];
    if (c === "\\") i += 2;
    else if (c === "`") return i;
    else if (c === "$" && code[i + 1] === "{") {
      let depth = 1;
      i += 2;
      while (i < code.length && depth > 0) {
        const d = code[i];
        if (d === "`") i = templateEnd(code, i) + 1;
        else if (d === '"' || d === "'") {
          let j = i + 1;
          while (j < code.length && code[j] !== d) j += code[j] === "\\" ? 2 : 1;
          i = j + 1;
        } else {
          if (d === "{") depth++;
          else if (d === "}") depth--;
          i++;
        }
      }
    } else i++;
  }
  return code.length;
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
// resolver (an absolute path, or null); an asset it cannot resolve stays as
// written, resolved at runtime, as in Vite.
export async function transformImportMetaUrl(code, id, resolve) {
  if (!code.includes("import.meta.url")) return code;
  const clean = stripLiterals(code);
  const edits = [];
  const hoisted = [];
  const hoist = (prefix, spec) => {
    const ident = `__oj_${prefix}${hoisted.length}`;
    hoisted.push(`import ${ident} from ${JSON.stringify(spec)};`);
    return ident;
  };
  const resolveFile = async (file) => {
    const r = await resolve(file, id);
    return r && !r.startsWith("\0") ? r.replace(/[?#].*$/, "") : null;
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
    if (!abs) continue;
    const query = postfix[0] === "?" ? `${postfix}&${kind}&url` : `?${kind}&url`;
    const ident = hoist("worker_url", abs + query);
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
    // A root-absolute URL is a public file, already served at that path.
    if (file[0] === "/") continue;
    const abs = await resolveFile(file);
    if (!abs) continue;
    const ident = hoist("asset_url", abs + "?url");
    const href = postfix ? `${ident} + ${JSON.stringify(postfix)}` : ident;
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

// The rewrite for dependency modules, which the app-code transform skips:
// Vite runs both plugins on every client module, and a bundled dependency's
// relative URL breaks the same way. A template URL's glob is expanded here,
// as the app-code transform does after it.
export function depImportMetaUrlPlugin() {
  return {
    name: "oj:dep-import-meta-url",
    transform: {
      filter: { id: { include: /\/node_modules\/.*\.[cm]?[jt]sx?$/ }, code: { include: /import\.meta\.url/ } },
      async handler(code, id) {
        const out = await transformImportMetaUrl(code, id, bundlerResolve(this));
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
