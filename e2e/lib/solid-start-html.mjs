// SPDX-License-Identifier: MIT
// Solid Start SSR HTML helpers: script stripping happens on the ENTIRE
// document BEFORE body/head boundaries are located, so a fake
// <body>/<title> inside a (possibly uppercase) <SCRIPT> payload in <head>
// can never satisfy a body or title assertion. Healthy routes must have
// real <body>...</body> / </head> boundaries; the render-error fallback
// (/render-boom, global default boundary) is a legitimate fragment with no
// body wrapper and must NOT go through the body helper.

const SCRIPT_RE = /<script[\s\S]*?<\/script\s*>/gi;
const BODY_OPEN_RE = /<body[^>]*>/i;
const BODY_CLOSE_RE = /<\/body\s*>/i;
const HEAD_CLOSE_RE = /<\/head\s*>/i;

// The whole document with serialized script payloads removed
// (case-insensitive <SCRIPT> included).
export function stripScripts(html) {
  return String(html).replace(SCRIPT_RE, "");
}

// The SSR body with scripts removed. Boundaries are located AFTER stripping
// (throws when missing: no falling back to the whole document).
export function bodyWithoutScripts(html) {
  const stripped = stripScripts(html);
  const open = BODY_OPEN_RE.exec(stripped);
  const close = BODY_CLOSE_RE.exec(stripped);
  if (!open || !close || close.index <= open.index) {
    throw new Error("SSR HTML is missing <body>...</body> boundaries");
  }
  return stripped.slice(open.index + open[0].length, close.index);
}

// The SSR head with scripts removed, head-scoped so a marker echoed in
// serialized body data cannot satisfy title assertions.
export function headOf(html) {
  const stripped = stripScripts(html);
  const close = HEAD_CLOSE_RE.exec(stripped);
  if (!close) throw new Error("SSR HTML is missing </head>");
  return stripped.slice(0, close.index);
}

// TanStack merges head down the route tree: the deepest route's title wins
// over the root title. Pin that ordering, head-scoped, not just presence.
export function assertDeepestTitle(html, label, route, routeTitle) {
  const head = headOf(html);
  if (!new RegExp(`<title[^>]*>${routeTitle}<\\/title>`).test(head)) {
    throw new Error(`${label}: ${route} SSR head missing the route title (want <title>${routeTitle}</title>)`);
  }
  if (head.includes("solid-start-fixture-root")) {
    throw new Error(`${label}: ${route} SSR head kept the root title; the route title must win`);
  }
}
