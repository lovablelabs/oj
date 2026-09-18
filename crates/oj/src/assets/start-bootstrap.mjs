// SPDX-License-Identifier: MIT

// The in-process Start runner's JS half: imported by the embedded engine,
// which calls `handle` once per request. The server entry arrives as a
// version-stamped specifier; `import()` goes through the engine's module
// host, so the loader semantics (transforms, aliases, plugin modules,
// invalidation) all live in Rust. Unchanged specifiers hit the isolate's
// module cache, so concurrent requests share one evaluation.

// The TSS defaults the node runner exported for the Start runtime.
process.env.TSS_SERVER_FN_BASE ??= "/_serverFn/";
process.env.TSS_DEV_SERVER ??= "true";
process.env.TSS_DEV_SSR_STYLES_ENABLED ??= "false";

/// One-time environment priming: the runner process carried these as spawn
/// env; in-process they are applied here before the entry is ever imported.
export function init(env) {
  for (const [k, v] of Object.entries(env ?? {})) {
    if (process.env[k] === undefined) process.env[k] = v;
  }
  // The node runner ran with cwd = the app root; keep that for cwd-relative
  // plugin filters and app code reading relative paths.
  if (env && env.OJ_APP_ROOT) {
    try { process.chdir(env.OJ_APP_ROOT); } catch {}
  }
  return true;
}

const b64ToBytes = (b64) => Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));

function bytesToB64(bytes) {
  let bin = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK));
  }
  return btoa(bin);
}

/// One request through the app's fetch handler. `req` is
/// `{ method, url, host, headers: [[k, v]...], bodyBase64 | null }`; the reply
/// is `{ status, headers: [[k, v]...], setCookies: [...], bodyBase64 | null }`.
/// The response is read to completion (a TanStack stream's deferred content is
/// present), so the transport is buffered where the loopback runner streamed.
export async function handle(entry, req) {
  const handler = (await import(entry)).default;
  const headers = new Headers();
  for (const [k, v] of req.headers) {
    if (k.toLowerCase() === "host") continue;
    headers.append(k, v);
  }
  headers.set("host", req.host);
  const init = { method: req.method, headers };
  if (req.bodyBase64 != null && req.method !== "GET" && req.method !== "HEAD") {
    init.body = b64ToBytes(req.bodyBase64);
  }
  const out = await handler.fetch(new Request("http://" + req.host + req.url, init));
  const outHeaders = [];
  out.headers.forEach((v, k) => {
    if (k !== "set-cookie" && k !== "content-length") outHeaders.push([k, v]);
  });
  const setCookies = out.headers.getSetCookie?.() ?? [];
  const body = out.body ? new Uint8Array(await out.arrayBuffer()) : null;
  return {
    status: out.status,
    headers: outHeaders,
    setCookies,
    bodyBase64: body && body.length ? bytesToB64(body) : null,
  };
}
