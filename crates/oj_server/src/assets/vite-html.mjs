// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite's dev HTML transform, ported: createDevHtmlTransformFn and devHtmlHook
// (server/middlewares/indexHtml.ts), the inline-proxy plugin, import-map, env
// and nonce hooks, and applyHtmlTransforms' tag injection (plugins/html.ts).
// The plugin host runs it for every dev page and for `server.transformIndexHtml`,
// so an inline `<script type="module">` becomes an html-proxy module whose
// imports resolve like any other module, exactly as under Vite.
//
// Inline `<style>` and `style="url(...)"` bodies go to `compileStyle`, the
// host's stand-in for Vite's CSS plugin container. Not ported:
// preTransformRequest (a warm-up with no effect on the response).

import { existsSync, statSync } from "node:fs";
import path from "node:path";

const FS_PREFIX = "/@fs/";
const VALID_ID_PREFIX = "/@id/";
const NULL_BYTE_PLACEHOLDER = "__x00__";

export const htmlProxyRE = /[?&]html-proxy=?(?:&inline-css)?(?:&style-attr)?&index=(\d+)\.(?:js|css)$/;
export const isHtmlProxyRE = /[?&]html-proxy\b/;
const importMapRE = /[ \t]*<script[^>]*type\s*=\s*(?:"importmap"|'importmap'|importmap)[^>]*>.*?<\/script>/is;
const importMapAppendRE = new RegExp(
  [
    /[ \t]*<script[^>]*type\s*=\s*(?:"module"|'module'|module)[^>]*>/i,
    /[ \t]*<link[^>]*rel\s*=\s*(?:"modulepreload"|'modulepreload'|modulepreload)[\s\S]*?>/i,
  ]
    .map((r) => r.source)
    .join("|"),
  "i",
);
const inlineImportRE = /(?<!(?<!\.\.)\.)\bimport\s*\(("(?:[^"]|(?<=\\)")*"|'(?:[^']|(?<=\\)')*')\)/dg;
const spaceRe = /[\t\n\f\r ]/;
const postfixRE = /[?#].*$/;
const knownJsSrcRE = /\.(?:[jt]sx?|m[jt]s|vue|marko|svelte|astro|imba|mdx)(?:$|\?)/;
const CSS_LANGS_RE = /\.(css|less|sass|scss|styl|stylus|pcss|postcss|sss)(?:$|\?)/;
const wordCharRE = /\w/;
const attrValueStartRE = /=\s*(.)/;

export const cleanUrl = (url) => url.replace(postfixRE, "");
const slash = (p) => p.replace(/\\/g, "/");
export const normalizePath = (id) => path.posix.normalize(slash(id));
const withTrailingSlash = (p) => (p[p.length - 1] === "/" ? p : `${p}/`);

function wrapId(id) {
  return id.startsWith(VALID_ID_PREFIX) ? id : VALID_ID_PREFIX + id.replace("\0", NULL_BYTE_PLACEHOLDER);
}
export function joinUrlSegments(a, b) {
  if (!a || !b) return a || b || "";
  if (a.endsWith("/")) a = a.substring(0, a.length - 1);
  if (b[0] !== "/") b = "/" + b;
  return a + b;
}
function stripBase(p, base) {
  if (p === base) return "/";
  const devBase = withTrailingSlash(base);
  return p.startsWith(devBase) ? p.slice(devBase.length - 1) : p;
}
function splitFileAndPostfix(p) {
  const file = cleanUrl(p);
  return { file, postfix: p.slice(file.length) };
}
function injectQuery(url, queryToInject) {
  const { file, postfix } = splitFileAndPostfix(url);
  return `${slash(file)}?${queryToInject}${postfix[0] === "?" ? `&${postfix.slice(1)}` : postfix}`;
}
function fsPathFromId(id) {
  const fsPath = normalizePath(id.startsWith(FS_PREFIX) ? id.slice(FS_PREFIX.length) : id);
  return fsPath[0] === "/" || /^[A-Za-z]:[/\\]/.test(fsPath) ? fsPath : `/${fsPath}`;
}
function isJSRequest(url) {
  url = cleanUrl(url);
  if (knownJsSrcRE.test(url)) return true;
  if (!path.extname(url) && url[url.length - 1] !== "/") return true;
  return false;
}
const isCSSRequest = (request) => CSS_LANGS_RE.test(request);
function checkPublicFile(url, config) {
  const { publicDir } = config;
  if (!publicDir || url[0] !== "/") return;
  const publicFile = normalizePath(path.join(publicDir, cleanUrl(url)));
  if (!publicFile.startsWith(withTrailingSlash(normalizePath(publicDir)))) return;
  try {
    return statSync(publicFile).isFile() ? publicFile : undefined;
  } catch {
    return undefined;
  }
}
const isBareRelative = (url) => wordCharRE.test(url[0]) && !url.includes(":");
const getHtmlDirnameForRelativeUrl = (htmlPath) => (htmlPath.endsWith("/") ? htmlPath : path.posix.dirname(htmlPath));
const shouldPreTransform = (url, config) => !checkPublicFile(url, config) && (isJSRequest(url) || isCSSRequest(url));

export function getHtmlFilename(url, root) {
  if (url.startsWith(FS_PREFIX)) return decodeURIComponent(fsPathFromId(url));
  return decodeURIComponent(normalizePath(path.join(root, url.slice(1))));
}

// ---- HTML scanning --------------------------------------------------------
// The slice of parse5 (scriptingEnabled: false, sourceCodeLocationInfo) the
// hooks read: elements in document order with their attributes and source
// offsets, and the raw text of raw-text elements. Attribute names are
// lowercased, the first of duplicate attributes wins and values have their
// character references decoded, as parse5 does.

const RAW_TEXT = new Set(["script", "style", "xmp", "iframe", "noembed", "noframes", "textarea", "title"]);
const NAMED_REFS = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: "\u00a0" };
function decodeRefs(value) {
  if (!value.includes("&")) return value;
  return value.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (m, ref) => {
    if (ref[0] === "#") {
      const code = ref[1] === "x" || ref[1] === "X" ? parseInt(ref.slice(2), 16) : parseInt(ref.slice(1), 10);
      return Number.isFinite(code) && code <= 0x10ffff ? String.fromCodePoint(code) : m;
    }
    return NAMED_REFS[ref.toLowerCase()] ?? m;
  });
}
const isWs = (c) => c === " " || c === "\t" || c === "\n" || c === "\f" || c === "\r";
const isAlpha = (c) => (c >= "a" && c <= "z") || (c >= "A" && c <= "Z");

function scanStartTag(html, lt) {
  let i = lt + 1;
  const nameStart = i;
  while (i < html.length && !isWs(html[i]) && html[i] !== "/" && html[i] !== ">") i++;
  const nodeName = html.slice(nameStart, i).toLowerCase();
  const attrs = [];
  const attrLocs = {};
  for (;;) {
    while (i < html.length && (isWs(html[i]) || html[i] === "/")) i++;
    if (i >= html.length) return null;
    if (html[i] === ">") break;
    const start = i;
    i++;
    while (i < html.length && !isWs(html[i]) && html[i] !== "/" && html[i] !== ">" && html[i] !== "=") i++;
    const name = html.slice(start, i).toLowerCase();
    let j = i;
    while (j < html.length && isWs(html[j])) j++;
    let value = "";
    if (html[j] === "=") {
      j++;
      while (j < html.length && isWs(html[j])) j++;
      const q = html[j];
      if (q === '"' || q === "'") {
        const close = html.indexOf(q, j + 1);
        if (close === -1) return null;
        value = html.slice(j + 1, close);
        i = close + 1;
      } else {
        const vStart = j;
        while (j < html.length && !isWs(html[j]) && html[j] !== ">") j++;
        value = html.slice(vStart, j);
        i = j;
      }
    }
    if (!(name in attrLocs)) {
      attrs.push({ name, value: decodeRefs(value) });
      attrLocs[name] = { startOffset: start, endOffset: i };
    }
  }
  const end = i + 1;
  return { nodeName, attrs, attrLocs, startTag: { startOffset: lt, endOffset: end } };
}

export function scanHtml(html) {
  const nodes = [];
  let i = 0;
  while (i < html.length) {
    const lt = html.indexOf("<", i);
    if (lt === -1) break;
    const next = html[lt + 1];
    if (html.startsWith("<!--", lt)) {
      const close = html.indexOf("-->", lt + 4);
      i = close === -1 ? html.length : close + 3;
      continue;
    }
    if (next === "!" || next === "?" || next === "/") {
      const close = html.indexOf(">", lt + 2);
      i = close === -1 ? html.length : close + 1;
      continue;
    }
    if (!next || !isAlpha(next)) {
      i = lt + 1;
      continue;
    }
    const tag = scanStartTag(html, lt);
    if (!tag) break;
    const node = {
      nodeName: tag.nodeName,
      attrs: tag.attrs,
      childNodes: [],
      sourceCodeLocation: {
        startOffset: lt,
        endOffset: tag.startTag.endOffset,
        startTag: tag.startTag,
        attrs: tag.attrLocs,
      },
    };
    i = tag.startTag.endOffset;
    if (RAW_TEXT.has(tag.nodeName)) {
      const closeRE = new RegExp(`</${tag.nodeName}[\\t\\n\\f\\r />]`, "ig");
      closeRE.lastIndex = i;
      const m = closeRE.exec(html);
      const contentEnd = m ? m.index : html.length;
      if (contentEnd > i) {
        node.childNodes.push({
          nodeName: "#text",
          value: html.slice(i, contentEnd),
          sourceCodeLocation: { startOffset: i, endOffset: contentEnd },
        });
      }
      const gt = m ? html.indexOf(">", m.index) : -1;
      i = gt === -1 ? html.length : gt + 1;
      node.sourceCodeLocation.endOffset = i;
    }
    nodes.push(node);
  }
  return nodes;
}

// ---- MagicString, the slice the hooks use ----------------------------------
export class Edits {
  constructor(original) {
    this.original = original;
    this.edits = [];
  }
  slice(start, end) {
    return this.original.slice(start, end);
  }
  update(start, end, content) {
    this.edits.push({ start, end, content });
    return this;
  }
  remove(start, end) {
    return this.update(start, end, "");
  }
  appendRight(index, content) {
    this.edits.push({ start: index, end: index, content });
    return this;
  }
  toString() {
    const sorted = this.edits.map((e, n) => ({ ...e, n })).sort((a, b) => a.start - b.start || a.n - b.n);
    let out = "";
    let at = 0;
    for (const e of sorted) {
      if (e.start < at) continue;
      out += this.original.slice(at, e.start) + e.content;
      at = e.end;
    }
    return out + this.original.slice(at);
  }
}

function overwriteAttrValue(s, loc, newValue) {
  const srcString = s.slice(loc.startOffset, loc.endOffset);
  const valueStart = attrValueStartRE.exec(srcString);
  if (!valueStart) throw new Error(`[vite:html] internal error, failed to overwrite attribute value`);
  const wrapOffset = valueStart[1] === '"' || valueStart[1] === "'" ? 1 : 0;
  const valueOffset = valueStart.index + valueStart[0].length - 1;
  s.update(loc.startOffset + valueOffset + wrapOffset, loc.endOffset - wrapOffset, newValue);
  return s;
}
function removeViteIgnoreAttr(s, loc) {
  const attr = loc.attrs?.["vite-ignore"];
  if (attr) s.remove(attr.startOffset, attr.endOffset);
  return s;
}
function getScriptInfo(node) {
  let src;
  let srcSourceCodeLocation;
  let isModule = false;
  let isIgnored = false;
  for (const p of node.attrs) {
    if (p.name === "src") {
      if (!src) {
        src = p;
        srcSourceCodeLocation = node.sourceCodeLocation.attrs.src;
      }
    } else if (p.name === "type" && p.value === "module") isModule = true;
    else if (p.name === "vite-ignore") isIgnored = true;
  }
  return { src, srcSourceCodeLocation, isModule, isIgnored };
}

const ALLOWED_META_NAME = [
  "msapplication-tileimage",
  "msapplication-square70x70logo",
  "msapplication-square150x150logo",
  "msapplication-wide310x150logo",
  "msapplication-square310x310logo",
  "msapplication-config",
  "twitter:image",
];
const ALLOWED_META_PROPERTY = [
  "og:image",
  "og:image:url",
  "og:image:secure_url",
  "og:audio",
  "og:audio:secure_url",
  "og:video",
  "og:video:secure_url",
];
const DEFAULT_HTML_ASSET_SOURCES = {
  audio: { srcAttributes: ["src"] },
  embed: { srcAttributes: ["src"] },
  img: { srcAttributes: ["src"], srcsetAttributes: ["srcset"] },
  image: { srcAttributes: ["href", "xlink:href"] },
  input: { srcAttributes: ["src"] },
  link: { srcAttributes: ["href"], srcsetAttributes: ["imagesrcset"] },
  object: { srcAttributes: ["data"] },
  source: { srcAttributes: ["src"], srcsetAttributes: ["srcset"] },
  track: { srcAttributes: ["src"] },
  use: { srcAttributes: ["href", "xlink:href"] },
  video: { srcAttributes: ["src", "poster"] },
  meta: {
    srcAttributes: ["content"],
    filter({ attributes }) {
      if (attributes.name && ALLOWED_META_NAME.includes(attributes.name.trim().toLowerCase())) return true;
      if (attributes.property && ALLOWED_META_PROPERTY.includes(attributes.property.trim().toLowerCase())) return true;
      return false;
    },
  },
};
function findNeedTransformStyleAttribute(node) {
  const attr = node.attrs.find(
    (prop) =>
      prop.prefix === undefined &&
      prop.name === "style" &&
      (prop.value.includes("url(") || prop.value.includes("image-set(")),
  );
  if (!attr) return undefined;
  return { attr, location: node.sourceCodeLocation?.attrs?.style };
}
function getNodeAssetAttributes(node, additionalAssetSources) {
  const defaults = DEFAULT_HTML_ASSET_SOURCES[node.nodeName];
  const additional = additionalAssetSources?.[node.nodeName];
  if (!defaults && !additional) return [];
  const attributes = {};
  for (const attr of node.attrs) attributes[attr.name] = attr.value;
  if ("vite-ignore" in attributes)
    return [
      {
        type: "remove",
        key: "vite-ignore",
        value: "",
        attributes,
        location: node.sourceCodeLocation.attrs["vite-ignore"],
      },
    ];
  const actions = [];
  function handleAttributeKey(key, type, filter) {
    const value = attributes[key];
    if (!value) return;
    if (filter && !filter({ key, value, attributes })) return;
    actions.push({ type, key, value, attributes, location: node.sourceCodeLocation.attrs[key] });
  }
  for (const sources of [defaults, additional]) {
    if (!sources) continue;
    sources.srcAttributes?.forEach((key) => handleAttributeKey(key, "src", sources.filter));
    sources.srcsetAttributes?.forEach((key) => handleAttributeKey(key, "srcset", sources.filter));
  }
  return actions;
}

const escapedSpaceCharacters = /(?: |\\t|\\n|\\f|\\r)+/g;
const imageCandidateRegex =
  /(?:^|\s|(?<=,))(?<url>[\w-]+\([^)]*\)|"[^"]*"|'[^']*'|[^,]\S*[^,])\s*(?:\s(?<descriptor>\w[^,]+))?(?:,|$)/g;
function parseSrcset(string) {
  const matches = string
    .trim()
    .replace(escapedSpaceCharacters, " ")
    .replace(/\r?\n/, "")
    .replace(/,\s+/, ", ")
    .replaceAll(/\s+/g, " ")
    .matchAll(imageCandidateRegex);
  return Array.from(matches, ({ groups }) => ({
    url: groups?.url?.trim() ?? "",
    descriptor: groups?.descriptor?.trim() ?? "",
  })).filter(({ url }) => !!url);
}
const joinSrcset = (ret) => ret.map(({ url, descriptor }) => url + (descriptor ? ` ${descriptor}` : "")).join(", ");
const processSrcSetSync = (srcs, replacer) =>
  joinSrcset(parseSrcset(srcs).map(({ url, descriptor }) => ({ url: replacer({ url, descriptor }), descriptor })));

// strip-literal's contract for the import() scan: comments and string or
// template contents blanked to spaces at the same offsets, quotes kept.
function stripLiteral(code) {
  let out = "";
  let i = 0;
  const blank = (s) => s.replace(/[^\n]/g, " ");
  while (i < code.length) {
    const c = code[i];
    if (c === "/" && code[i + 1] === "/") {
      const end = code.indexOf("\n", i);
      const stop = end === -1 ? code.length : end;
      out += blank(code.slice(i, stop));
      i = stop;
    } else if (c === "/" && code[i + 1] === "*") {
      const end = code.indexOf("*/", i + 2);
      const stop = end === -1 ? code.length : end + 2;
      out += blank(code.slice(i, stop));
      i = stop;
    } else if (c === '"' || c === "'" || c === "`") {
      let j = i + 1;
      while (j < code.length && code[j] !== c) j += code[j] === "\\" ? 2 : 1;
      out += c + blank(code.slice(i + 1, Math.min(j, code.length))) + (j < code.length ? c : "");
      i = j + 1;
    } else {
      out += c;
      i++;
    }
  }
  return out;
}
function extractImportExpressionFromClassicScript(scriptTextNode) {
  const startOffset = scriptTextNode.sourceCodeLocation.startOffset;
  const cleanCode = stripLiteral(scriptTextNode.value);
  const scriptUrls = [];
  let match;
  inlineImportRE.lastIndex = 0;
  while ((match = inlineImportRE.exec(cleanCode))) {
    const [, [urlStart, urlEnd]] = match.indices;
    const start = urlStart + 1;
    const end = urlEnd - 1;
    scriptUrls.push({
      start: start + startOffset,
      end: end + startOffset,
      url: scriptTextNode.value.slice(start, end),
    });
  }
  return scriptUrls;
}

// ---- Tag injection (applyHtmlTransforms) ------------------------------------
const headInjectRE = /([ \t]*)<\/head>/i;
const headPrependInjectRE = /([ \t]*)<head[^>]*>/i;
const htmlInjectRE = /<\/html>/i;
const htmlPrependInjectRE = /([ \t]*)<html[^>]*>/i;
const bodyInjectRE = /([ \t]*)<\/body>/i;
const bodyPrependInjectRE = /([ \t]*)<body[^>]*>/i;
const doctypePrependInjectRE = /<!doctype html>/i;
const unaryTags = new Set(["link", "meta", "base"]);
const elementsAllowedInHead = new Set(["title", "base", "link", "style", "meta", "script", "noscript", "template"]);

function escapeHtml(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}
function serializeAttrs(attrs) {
  let res = "";
  for (const key in attrs) {
    if (typeof attrs[key] === "boolean") res += attrs[key] ? ` ${key}` : ``;
    else res += ` ${key}="${escapeHtml(attrs[key])}"`;
  }
  return res;
}
const incrementIndent = (indent = "") => `${indent}${indent[0] === "\t" ? "\t" : "  "}`;
function serializeTag({ tag, attrs, children }, indent = "") {
  if (unaryTags.has(tag)) return `<${tag}${serializeAttrs(attrs)}>`;
  return `<${tag}${serializeAttrs(attrs)}>${serializeTags(children, incrementIndent(indent))}</${tag}>`;
}
export function serializeTags(tags, indent = "") {
  if (typeof tags === "string") return tags;
  if (tags && tags.length) return tags.map((tag) => `${indent}${serializeTag(tag, indent)}\n`).join("");
  return "";
}
function prependInjectFallback(html, tags) {
  if (htmlPrependInjectRE.test(html)) return html.replace(htmlPrependInjectRE, `$&\n${serializeTags(tags)}`);
  if (doctypePrependInjectRE.test(html)) return html.replace(doctypePrependInjectRE, `$&\n${serializeTags(tags)}`);
  return serializeTags(tags) + html;
}
function injectToHead(html, tags, prepend = false) {
  if (tags.length === 0) return html;
  if (prepend) {
    if (headPrependInjectRE.test(html))
      return html.replace(headPrependInjectRE, (match, p1) => `${match}\n${serializeTags(tags, incrementIndent(p1))}`);
  } else {
    if (headInjectRE.test(html))
      return html.replace(headInjectRE, (match, p1) => `${serializeTags(tags, incrementIndent(p1))}${match}`);
    if (bodyPrependInjectRE.test(html))
      return html.replace(bodyPrependInjectRE, (match, p1) => `${serializeTags(tags, p1)}\n${match}`);
  }
  return prependInjectFallback(html, tags);
}
function injectToBody(html, tags, prepend = false) {
  if (tags.length === 0) return html;
  if (prepend) {
    if (bodyPrependInjectRE.test(html))
      return html.replace(bodyPrependInjectRE, (match, p1) => `${match}\n${serializeTags(tags, incrementIndent(p1))}`);
    if (headInjectRE.test(html))
      return html.replace(headInjectRE, (match, p1) => `${match}\n${serializeTags(tags, p1)}`);
    return prependInjectFallback(html, tags);
  }
  if (bodyInjectRE.test(html))
    return html.replace(bodyInjectRE, (match, p1) => `${serializeTags(tags, incrementIndent(p1))}${match}`);
  if (htmlInjectRE.test(html)) return html.replace(htmlInjectRE, `${serializeTags(tags)}\n$&`);
  return html + `\n` + serializeTags(tags);
}

/** `hooks`: `{ fn, thisArg }`, run in order; each result's tags inject before the next hook runs. */
export async function applyHtmlTransforms(html, hooks, ctx, warn = () => {}) {
  for (const { fn, thisArg } of hooks) {
    const res = await fn.call(thisArg, html, ctx);
    if (!res) continue;
    if (typeof res === "string") {
      html = res;
      continue;
    }
    let tags;
    if (Array.isArray(res)) tags = res;
    else {
      html = res.html || html;
      tags = res.tags;
    }
    let headTags;
    let headPrependTags;
    let bodyTags;
    let bodyPrependTags;
    for (const tag of tags ?? []) {
      switch (tag.injectTo) {
        case "body":
          (bodyTags ??= []).push(tag);
          break;
        case "body-prepend":
          (bodyPrependTags ??= []).push(tag);
          break;
        case "head":
          (headTags ??= []).push(tag);
          break;
        default:
          (headPrependTags ??= []).push(tag);
      }
    }
    const inHead = [...(headTags || []), ...(headPrependTags || [])];
    const disallowed = [...new Set(inHead.filter((t) => !elementsAllowedInHead.has(t.tag)).map((t) => `<${t.tag}>`))];
    if (disallowed.length)
      warn(`[${disallowed.join(",")}] can not be used inside the <head> Element, please check the 'injectTo' value`);
    if (headPrependTags) html = injectToHead(html, headPrependTags, true);
    if (headTags) html = injectToHead(html, headTags);
    if (bodyPrependTags) html = injectToBody(html, bodyPrependTags, true);
    if (bodyTags) html = injectToBody(html, bodyTags);
  }
  return html;
}

/** Vite's resolveHtmlTransforms: `[pre, normal, post]` handlers by `order`. */
export function resolveHtmlTransforms(plugins, bind) {
  const preHooks = [];
  const normalHooks = [];
  const postHooks = [];
  for (const plugin of plugins) {
    const hook = plugin && plugin.transformIndexHtml;
    if (!hook) continue;
    if (typeof hook === "function") normalHooks.push(bind(plugin, hook));
    else {
      // Vite <6 also read `enforce` and `transform`; apps on older Vite still ship them.
      const handler = hook.handler ?? hook.transform;
      if (typeof handler !== "function") continue;
      const order = hook.order ?? hook.enforce;
      if (order === "pre") preHooks.push(bind(plugin, handler));
      else if (order === "post") postHooks.push(bind(plugin, handler));
      else normalHooks.push(bind(plugin, handler));
    }
  }
  return [preHooks, normalHooks, postHooks];
}

// ---- The built-in hooks -----------------------------------------------------
export function preImportMapHook(root, warnOnce) {
  return (html, ctx) => {
    const importMapIndex = html.search(importMapRE);
    if (importMapIndex < 0) return;
    const importMapAppendIndex = html.search(importMapAppendRE);
    if (importMapAppendIndex < 0) return;
    if (importMapAppendIndex < importMapIndex) {
      const relativeHtml = normalizePath(path.relative(root, ctx.filename));
      warnOnce(
        `(!) <script type="importmap"> should come before <script type="module"> and <link rel="modulepreload"> in /${relativeHtml}`,
      );
    }
  };
}
export function postImportMapHook() {
  return (html) => {
    if (importMapAppendRE.test(html)) {
      let importMap;
      html = html.replace(importMapRE, (match) => {
        importMap = match;
        return "";
      });
      if (importMap) html = html.replace(importMapAppendRE, (match) => `${importMap}\n${match}`);
    }
    return html;
  };
}
export function injectCspNonceMetaTagHook(nonce) {
  return () => {
    if (!nonce) return;
    return [{ tag: "meta", injectTo: "head", attrs: { property: "csp-nonce", nonce } }];
  };
}
export function injectNonceAttributeTagHook(nonce) {
  const processRelType = new Set(["stylesheet", "modulepreload", "preload"]);
  return (html) => {
    if (!nonce) return;
    const s = new Edits(html);
    for (const node of scanHtml(html)) {
      const { nodeName, attrs, sourceCodeLocation } = node;
      if (
        nodeName === "script" ||
        nodeName === "style" ||
        (nodeName === "link" &&
          attrs.some(
            (attr) =>
              attr.name === "rel" &&
              attr.value
                .split(spaceRe)
                .map((v) => v.toLowerCase())
                .some((a) => processRelType.has(a)),
          ))
      ) {
        if (attrs.some(({ name }) => name === "nonce")) continue;
        const startTagEndOffset = sourceCodeLocation.startTag.endOffset;
        const appendOffset = html[startTagEndOffset - 2] === "/" ? 2 : 1;
        s.appendRight(startTagEndOffset - appendOffset, ` nonce="${nonce}"`);
      }
    }
    return s.toString();
  };
}
/** `env`: Vite's `{ ...config.env, ...import.meta.env.* defines }`. */
export function htmlEnvHook(env, envPrefix, root, warn) {
  const pattern = /%(\S+?)%/g;
  const prefixes = Array.isArray(envPrefix) ? envPrefix : [envPrefix ?? "VITE_"];
  return (html, ctx) =>
    html.replace(pattern, (text, key) => {
      if (key in env) return env[key];
      if (prefixes.some((prefix) => key.startsWith(prefix))) {
        const relativeHtml = normalizePath(path.relative(root, ctx.filename));
        warn(`(!) ${text} is not defined in env variables found in /${relativeHtml}. Is the variable mistyped?`);
      }
      return text;
    });
}
/** Vite's htmlEnvHook env: config.env plus every `import.meta.env.*` define. */
export function htmlEnvOf(config) {
  const env = { ...config.env };
  for (const key in config.define ?? {}) {
    if (!key.startsWith(`import.meta.env.`)) continue;
    const val = config.define[key];
    if (typeof val === "string") {
      try {
        const parsed = JSON.parse(val);
        env[key.slice(16)] = typeof parsed === "string" ? parsed : val;
      } catch {
        env[key.slice(16)] = val;
      }
    } else env[key.slice(16)] = JSON.stringify(val);
  }
  return env;
}

// ---- html-proxy modules -----------------------------------------------------
/** The proxy cache and the `vite:html-inline-proxy` plugin that loads from it. */
export function createHtmlProxy(getRoot) {
  const cache = new Map();
  return {
    add(filePath, index, result) {
      if (!cache.has(filePath)) cache.set(filePath, []);
      cache.get(filePath)[index] = result;
    },
    plugin: {
      name: "vite:html-inline-proxy",
      resolveId: {
        filter: { id: isHtmlProxyRE },
        handler(id) {
          return id;
        },
      },
      load: {
        filter: { id: isHtmlProxyRE },
        handler(id) {
          const proxyMatch = htmlProxyRE.exec(id);
          if (!proxyMatch) return;
          const index = Number(proxyMatch[1]);
          const url = cleanUrl(id).replace(normalizePath(getRoot()), "");
          const result = cache.get(url)?.[index];
          if (result) return { ...result, moduleSideEffects: true };
          throw new Error(`No matching HTML proxy module found from ${id}`);
        },
      },
    },
  };
}

function processNodeUrl(url, useSrcSetReplacer, config, htmlPath, originalUrl, moduleGraph, isClassicScriptLink) {
  const decodedBase = config.decodedBase || "/";
  const replacer = (url) => {
    if (
      (url[0] === "/" && url[1] !== "/") ||
      ((url[0] === "." || isBareRelative(url)) && originalUrl && originalUrl !== "/" && htmlPath === "/index.html")
    )
      url = path.posix.join(config.base || "/", url);
    let preTransformUrl;
    if (!isClassicScriptLink && shouldPreTransform(url, config)) {
      if (url[0] === "/" && url[1] !== "/") preTransformUrl = url;
      else if (url[0] === "." || isBareRelative(url))
        preTransformUrl = path.posix.join(config.base || "/", getHtmlDirnameForRelativeUrl(htmlPath), url);
    }
    const mod = moduleGraph?.urlToModuleMap?.get(stripBase(preTransformUrl || url, decodedBase));
    if (mod && mod.lastHMRTimestamp > 0) url = injectQuery(url, `t=${mod.lastHMRTimestamp}`);
    return url;
  };
  return useSrcSetReplacer ? processSrcSetSync(url, ({ url }) => replacer(url)) : replacer(url);
}

/**
 * Vite's devHtmlHook. `config`: root, base, decodedBase, publicDir, html;
 * `clientTags`: the dev client's tags (Vite's `/@vite/client`), injected
 * head-prepend; `compileStyle(id, code)`: the compiled text of an inline
 * style's html-proxy id (Vite's client pluginContainer.transform, for a
 * `style-attr` id the attribute value vite:css-post records), absent to leave
 * styles as written.
 */
export function createDevHtmlHook({ config, proxy, moduleGraph, clientTags, compileStyle }) {
  return async (html, { path: htmlPath, filename, originalUrl }) => {
    const root = config.root;
    const decodedBase = config.decodedBase || "/";
    let proxyModulePath;
    let proxyModuleUrl;
    const trailingSlash = htmlPath.endsWith("/");
    if (!trailingSlash && existsSync(filename)) {
      proxyModulePath = htmlPath.startsWith(FS_PREFIX) ? filename : htmlPath;
      proxyModuleUrl = htmlPath;
    } else {
      proxyModulePath = `\0${`${htmlPath}${trailingSlash ? "index.html" : ""}`}`;
      proxyModuleUrl = wrapId(proxyModulePath);
    }
    proxyModuleUrl = joinUrlSegments(decodedBase, proxyModuleUrl);
    const s = new Edits(html);
    let inlineModuleIndex = -1;
    const proxyCacheUrl = decodeURI(cleanUrl(proxyModulePath).replace(normalizePath(root), ""));
    const styleUrl = [];
    const inlineStyles = [];
    const addInlineModule = (node, ext) => {
      inlineModuleIndex++;
      const contentNode = node.childNodes[0];
      proxy.add(proxyCacheUrl, inlineModuleIndex, { code: contentNode.value });
      const modulePath = `${proxyModuleUrl}?html-proxy&index=${inlineModuleIndex}.${ext}`;
      s.update(
        node.sourceCodeLocation.startOffset,
        node.sourceCodeLocation.endOffset,
        `<script type="module" src="${modulePath}"></script>`,
      );
    };
    for (const node of scanHtml(html)) {
      if (node.nodeName === "script") {
        const { src, srcSourceCodeLocation, isModule, isIgnored } = getScriptInfo(node);
        if (isIgnored) removeViteIgnoreAttr(s, node.sourceCodeLocation);
        else if (src) {
          const processedUrl = processNodeUrl(src.value, false, config, htmlPath, originalUrl, moduleGraph, !isModule);
          if (processedUrl !== src.value) overwriteAttrValue(s, srcSourceCodeLocation, processedUrl);
        } else if (isModule && node.childNodes.length) addInlineModule(node, "js");
        else if (node.childNodes.length) {
          const scriptNode = node.childNodes[node.childNodes.length - 1];
          for (const { url, start, end } of extractImportExpressionFromClassicScript(scriptNode)) {
            const processedUrl = processNodeUrl(url, false, config, htmlPath, originalUrl);
            if (processedUrl !== url) s.update(start, end, processedUrl);
          }
        }
      }
      const inlineStyle = findNeedTransformStyleAttribute(node);
      if (inlineStyle) {
        inlineModuleIndex++;
        inlineStyles.push({ index: inlineModuleIndex, location: inlineStyle.location, code: inlineStyle.attr.value });
      }
      if (node.nodeName === "style" && node.childNodes.length) {
        const children = node.childNodes[0];
        styleUrl.push({
          start: children.sourceCodeLocation.startOffset,
          end: children.sourceCodeLocation.endOffset,
          code: children.value,
        });
      }
      for (const attr of getNodeAssetAttributes(node, config.html?.additionalAssetSources)) {
        if (attr.type === "remove") s.remove(attr.location.startOffset, attr.location.endOffset);
        else {
          const processedUrl = processNodeUrl(attr.value, attr.type === "srcset", config, htmlPath, originalUrl);
          if (processedUrl !== attr.value) overwriteAttrValue(s, attr.location, processedUrl);
        }
      }
    }
    // A re-served page re-registers its inline modules: drop stale compiles.
    for (let i = 0; i <= inlineModuleIndex; i++) {
      const url = `${proxyModuleUrl}?html-proxy&index=${i}.js`;
      const mod = moduleGraph?.urlToModuleMap?.get(url);
      if (mod && typeof moduleGraph.invalidateModule === "function") moduleGraph.invalidateModule(mod);
    }
    if (compileStyle) {
      await Promise.all([
        ...styleUrl.map(async ({ start, end, code }, index) => {
          const url = `${proxyModulePath}?html-proxy&direct&index=${index}.css`;
          s.update(start, end, (await compileStyle(url, code)) ?? code);
        }),
        ...inlineStyles.map(async ({ index, location, code }) => {
          const url = `${proxyModulePath}?html-proxy&inline-css&style-attr&index=${index}.css`;
          overwriteAttrValue(s, location, (await compileStyle(url, code)) ?? "");
        }),
      ]);
    }
    return { html: s.toString(), tags: clientTags };
  };
}

/** Vite's createDevHtmlTransformFn hook order. */
export function devHtmlHooks({
  preHooks,
  normalHooks,
  postHooks,
  root,
  env,
  envPrefix,
  nonce,
  devHtmlHook,
  warn,
  warnOnce,
}) {
  const own = (fn) => ({ fn, thisArg: undefined });
  return [
    own(preImportMapHook(root, warnOnce)),
    own(injectCspNonceMetaTagHook(nonce)),
    ...preHooks,
    own(htmlEnvHook(env, envPrefix, root, warn)),
    own(devHtmlHook),
    ...normalHooks,
    ...postHooks,
    own(injectNonceAttributeTagHook(nonce)),
    own(postImportMapHook()),
  ];
}

/**
 * One dev page through the whole chain. `config`: the resolved config (root,
 * base, decodedBase, publicDir, html, env, define, envPrefix); `bind(plugin,
 * handler)` gives a plugin hook its context; `env`/`nonce` override what the
 * config would give.
 */
export async function transformDevHtml({
  url,
  html,
  originalUrl,
  filename,
  server,
  config,
  plugins,
  bind,
  proxy,
  moduleGraph,
  clientTags,
  compileStyle,
  env,
  nonce,
  warn = () => {},
  warnOnce = warn,
}) {
  const root = config.root;
  const base = config.base || "/";
  let decodedBase = config.decodedBase;
  if (decodedBase == null) {
    try {
      decodedBase = decodeURI(base);
    } catch {
      decodedBase = base;
    }
  }
  const ctx = { path: url, filename: filename ?? getHtmlFilename(url, root), server, originalUrl };
  const devHtmlHook = createDevHtmlHook({
    config: { root, base, decodedBase, publicDir: config.publicDir, html: config.html },
    proxy,
    moduleGraph,
    clientTags,
    compileStyle,
  });
  const [preHooks, normalHooks, postHooks] = resolveHtmlTransforms(plugins, bind);
  const hooks = devHtmlHooks({
    preHooks,
    normalHooks,
    postHooks,
    root,
    env: env ?? htmlEnvOf(config),
    envPrefix: config.envPrefix,
    nonce: nonce ?? config.html?.cspNonce,
    devHtmlHook,
    warn,
    warnOnce,
  });
  return applyHtmlTransforms(html, hooks, ctx, warn);
}
