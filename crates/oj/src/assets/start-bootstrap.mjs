// SPDX-License-Identifier: MIT

// The in-process Start runner's JS half: imported by the embedded engine,
// which calls `handle` once per request. The server entry arrives as a
// version-stamped specifier; `import()` goes through the engine's module
// host, so the loader semantics (transforms, aliases, plugin modules,
// invalidation) all live in Rust. Unchanged specifiers hit the isolate's
// module cache, so concurrent requests share one evaluation.

// A private process.env copy, the plugin host's and the script engines'
// isolation: Deno.env writes are process-real, so `init`'s priming (NODE_ENV,
// the .env values, OJ_*) and the TSS defaults below would otherwise land in
// oj's own environment and ride a restart's exec into the next boot.
Object.defineProperty(process, "env", {
  value: { ...process.env },
  configurable: true,
  writable: true,
});

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
    try {
      process.chdir(env.OJ_APP_ROOT);
    } catch {}
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

// The response bodies in flight, by the id `handle` gave them. The ids are
// random, so a pull meant for an engine replaced since can never land on a
// newer body of the replacement.
const bodies = new Map();

/// One request through the app's fetch handler. `req` is
/// `{ method, url, host, headers: [[k, v]...], bodyBase64 | null }`; the reply
/// is `{ status, headers: [[k, v]...], setCookies: [...], body: id | null }`.
/// The body is not read here: `body` names a reader `pull` drains chunk by
/// chunk, so each chunk reaches the client as the handler writes it (a
/// server-sent event, a render's deferred content), as Node's response does
/// under Vite's middleware for Start (srvx's `streamBody`: one `res.write`
/// per chunk).
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
  let body = null;
  if (out.body) {
    body = crypto.randomUUID();
    bodies.set(body, out.body.getReader());
  }
  return { status: out.status, headers: outHeaders, setCookies, body };
}

/// The next chunk of a body `handle` returned: `{ chunkBase64 }`, or
/// `{ done: true }` once the stream ended (the reader is forgotten then, as
/// it is when the read fails).
export async function pull(id) {
  const reader = bodies.get(id);
  if (!reader) throw new Error(`oj start: no response body ${id} in flight`);
  for (;;) {
    let next;
    try {
      next = await reader.read();
    } catch (e) {
      bodies.delete(id);
      throw e;
    }
    if (next.done) {
      bodies.delete(id);
      return { done: true };
    }
    // An empty chunk is not worth a round trip.
    if (next.value && next.value.length) return { chunkBase64: bytesToB64(next.value) };
  }
}

/// Gives a body up before its end (the client went away): the stream is
/// cancelled, so the handler's producer stops writing for nobody.
export async function cancel(id) {
  const reader = bodies.get(id);
  bodies.delete(id);
  if (reader) await reader.cancel().catch(() => {});
  return true;
}
