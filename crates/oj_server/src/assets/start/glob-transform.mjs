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

function matchPattern(absGlob, exhaustive = false, caseSensitive = true) {
  absGlob = absGlob.split(sep).join("/");
  const starIdx = absGlob.search(/[*?{]/);
  const base = starIdx === -1 ? dirname(absGlob) : absGlob.slice(0, absGlob.lastIndexOf("/", starIdx));
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

export function transformGlob(code, filePath, root = process.env.OJ_APP_ROOT ?? process.cwd()) {
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
