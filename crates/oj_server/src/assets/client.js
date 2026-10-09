// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const hotModules = new Map();

// Filled in by the server from `server.hmr` (Vite's clientInjections): JSON
// literals, `null` where the config is silent.
const hmrProtocol = __HMR_PROTOCOL__;
const hmrHostname = __HMR_HOSTNAME__;
const hmrPort = __HMR_PORT__;
const hmrPath = __HMR_PATH__;
const enableOverlay = __HMR_ENABLE_OVERLAY__;
const wsToken = __WS_TOKEN__;
// Vite 8's resolved `server.forwardConsole`: {enabled, unhandledErrors, logLevels}.
const forwardConsole = __FORWARD_CONSOLE__;

const customListeners = new Map();
function emit(event, data) {
  const set = customListeners.get(event);
  if (set) for (const cb of [...set]) cb(data);
}

export function createHotContext(ownerId) {
  const id = ownerId.split("?")[0];
  let mod = hotModules.get(id);
  if (mod) {
    mod.acceptCallbacks = [];
    mod.disposeCallbacks = [];
    mod.pruneCallbacks = [];
    if (mod.listeners) for (const [ev, cb] of mod.listeners) customListeners.get(ev)?.delete(cb);
    mod.listeners = [];
  } else {
    mod = {
      acceptCallbacks: [],
      disposeCallbacks: [],
      pruneCallbacks: [],
      data: {},
      listeners: [],
    };
    hotModules.set(id, mod);
  }
  const data = mod.data;
  return {
    data,
    accept(first, second) {
      // accept() / accept(cb): self-accepting. accept(dep, cb) / accept([deps], cb):
      // this module is the boundary for updates of those dependencies and the
      // callback receives the new dependency module(s), as in Vite.
      if (first === undefined || typeof first === "function") {
        mod.acceptCallbacks.push({ deps: [id], fn: first || (() => {}) });
        return;
      }
      const deps = (Array.isArray(first) ? first : [first]).map((d) => String(d).split("?")[0]);
      mod.acceptCallbacks.push({ deps, fn: second || (() => {}), single: typeof first === "string" });
    },
    acceptExports(_names, cb) {
      mod.acceptCallbacks.push({ deps: [id], fn: cb || (() => {}) });
    },
    dispose(cb) {
      mod.disposeCallbacks.push(cb);
    },
    prune(cb) {
      mod.pruneCallbacks.push(cb);
    },
    decline() {},
    invalidate(message) {
      // Vite's shape: the module that started the chain travels along so the
      // server can spot an invalidate that came back around and reload instead
      // of ping-ponging updates.
      const firstInvalidatedBy = currentFirstInvalidatedBy ?? id;
      const data = { path: id, message, firstInvalidatedBy };
      emit("vite:invalidate", data);
      console.warn(`[oj] ${id} invalidated${message ? ": " + message : ""}`);
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "custom", event: "vite:invalidate", data }));
      } else {
        location.reload();
      }
    },
    on(event, cb) {
      if (!customListeners.has(event)) customListeners.set(event, new Set());
      customListeners.get(event).add(cb);
      mod.listeners.push([event, cb]);
    },
    off(event, cb) {
      customListeners.get(event)?.delete(cb);
    },
    send(event, payload) {
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "custom", event, data: payload }));
      }
    },
  };
}

const styleTags = new Map();

export function updateStyle(id, css) {
  let tag = styleTags.get(id);
  if (!tag) {
    tag = document.createElement("style");
    tag.setAttribute("data-oj-id", id);
    document.head.appendChild(tag);
    styleTags.set(id, tag);
  }
  tag.textContent = css;
}

export function removeStyle(id) {
  const tag = styleTags.get(id);
  if (tag) {
    tag.remove();
    styleTags.delete(id);
  }
}

// Vite's HMRClient.currentFirstInvalidatedBy: set while an update's accept
// callbacks run, so an invalidate() they trigger names the original module.
let currentFirstInvalidatedBy;

async function prunePaths(paths) {
  for (const path of paths) {
    const mod = hotModules.get(path);
    if (!mod) continue;
    for (const dispose of mod.disposeCallbacks) await dispose(mod.data);
    for (const prune of mod.pruneCallbacks) await prune(mod.data);
  }
}

let updateChain = Promise.resolve();
function queueUpdate(update) {
  updateChain = updateChain.then(() => applyUpdate(update)).catch(() => {});
  return updateChain;
}

async function applyUpdate(update) {
  const cleanPath = update.path.split("?")[0];
  const acceptedUrl = update.acceptedPath || update.path;
  const acceptedPath = acceptedUrl.split("?")[0];
  const mod = hotModules.get(cleanPath);
  if (!mod) {
    // The module is in the server's graph but this page never loaded it (a
    // lazy route, a component behind a condition). Nothing to swap; like Vite,
    // ignore it instead of reloading a page that does not run that code.
    console.debug(`[oj] ${cleanPath} is not loaded here, ignoring update`);
    return;
  }
  const isSelf = acceptedPath === cleanPath;
  const accepts = mod.acceptCallbacks.filter((cb) => cb.deps.includes(acceptedPath));
  if (accepts.length === 0) {
    console.log(`[oj] ${cleanPath} has no accept handler for ${acceptedPath}, reloading`);
    location.reload();
    return;
  }
  // Dispose callbacks belong to the module being replaced (the accepted one).
  const disposed = hotModules.get(acceptedPath);
  const disposes = disposed ? disposed.disposeCallbacks.slice() : [];
  emit("vite:beforeUpdate", { type: "update", updates: [update] });
  try {
    for (const dispose of disposes) await dispose(disposed.data);
    const sep = acceptedUrl.includes("?") ? "&" : "?";
    let next;
    try {
      next = await import(acceptedUrl + sep + "t=" + update.timestamp);
    } catch (err) {
      if (update.isWithinCircularImport) {
        // Vite: an accepted module inside an import loop cannot recover its
        // execution order; reload the page to reset it instead of an overlay.
        console.info(
          `[oj] ${acceptedPath} failed to apply HMR as it's within a circular import. Reloading page to reset the execution order.`,
        );
        location.reload();
        return;
      }
      throw err;
    }
    try {
      currentFirstInvalidatedBy = update.firstInvalidatedBy;
      for (const cb of accepts) {
        if (isSelf || cb.single) await cb.fn(next);
        else await cb.fn(cb.deps.map((d) => (d === acceptedPath ? next : undefined)));
      }
    } finally {
      currentFirstInvalidatedBy = undefined;
    }
    clearOverlay();
    emit("vite:afterUpdate", { type: "update", updates: [update] });
    console.log(`[oj] hot updated ${update.path}`);
  } catch (err) {
    // Like Vite's warnFailedUpdate: log, but do not raise an overlay of our own.
    // A compile error behind the failed fetch already arrived as the server's
    // error frame (with the file and code frame); replacing that overlay with
    // "failed to fetch" would hide the real cause.
    emit("vite:error", { err: { message: String(err) } });
    reportHmrFailure(update.path, err);
    if (!(err instanceof Error) || !err.message.includes("fetch")) console.error(err);
    console.error(
      `[oj] Failed to reload ${update.path}. This could be due to syntax errors or importing non-existent modules. (see errors above)`,
    );
  }
}

let overlayEl = null;
let isFirstUpdate = true;

// If this is the first update and an error overlay is already showing, the page
// opened with a server compile error and the module script never finished
// loading (one of its nested imports was a 500): the boundaries above the fixed
// file were never registered, so a hot update has nothing to swap into. A full
// reload is the only way to recover, as in Vite's clearOverlayOrReloadOnFirstUpdate.
function clearOverlayOrReloadOnFirstUpdate() {
  if (isFirstUpdate && overlayEl) {
    location.reload();
    return "reload";
  }
  clearOverlay();
  isFirstUpdate = false;
  return "continue";
}

function parseError(text) {
  const s = String(text);
  const title = (s.split("\n").find((l) => l.trim()) || "Build error").trim();
  const loc = s.match(/([^\s():]+\.[A-Za-z0-9]+):(\d+)(?::(\d+))?/);
  return { title, file: loc && loc[1], line: loc && loc[2], col: loc && loc[3], frame: s };
}

// Vite's `vite-error-overlay` custom element: framework runtimes look it up
// with customElements.get() and construct it with an ErrorPayload `err`
// ({message, stack, id, loc, frame, plugin}) to show their own runtime errors.
export class ErrorOverlay extends HTMLElement {
  constructor(err, _links = true) {
    super();
    const root = this.attachShadow({ mode: "open" });
    const e = err && typeof err === "object" ? err : { message: String(err ?? "unknown error") };
    const parsed = parseError(e.message || "unknown error");
    const file = e.loc?.file || e.id || parsed.file;
    const line = e.loc?.line ?? parsed.line;
    const col = e.loc?.column ?? parsed.col;
    const frame = e.frame && String(e.frame).trim() ? e.frame : parsed.frame;
    const backdrop = document.createElement("div");
    backdrop.setAttribute("role", "dialog");
    backdrop.setAttribute("aria-label", "Build error");
    backdrop.style.cssText =
      "position:fixed;inset:0;z-index:99999;background:rgba(10,10,14,.86);" +
      "display:flex;align-items:flex-start;justify-content:center;padding:6vh 4vw;overflow:auto;" +
      "font:13px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace";
    const card = document.createElement("div");
    card.style.cssText =
      "max-width:920px;width:100%;background:#16161c;border:1px solid #33333f;border-radius:12px;" +
      "box-shadow:0 20px 60px rgba(0,0,0,.5);overflow:hidden;color:#e6e6ea";
    const bar = document.createElement("div");
    bar.style.cssText =
      "display:flex;align-items:center;gap:10px;padding:12px 18px;background:#2a1417;border-bottom:1px solid #4a2028";
    const brand = document.createElement("span");
    brand.style.cssText = "font-weight:700;letter-spacing:.08em;color:#ff6b6b";
    brand.textContent = "oj";
    const label = document.createElement("span");
    label.style.cssText = "color:#ff9a9a;font-weight:600";
    label.textContent = e.plugin && e.plugin !== "oj" ? `Build error (plugin: ${e.plugin})` : "Build error";
    bar.append(brand, label);
    const body = document.createElement("div");
    body.style.cssText = "padding:18px";
    const title = document.createElement("div");
    title.style.cssText = "color:#ff8a8a;font-weight:600;font-size:14px;white-space:pre-wrap;margin-bottom:10px";
    title.textContent = parsed.title;
    body.appendChild(title);
    if (file) {
      const loc = document.createElement("div");
      loc.style.cssText = "color:#8ab4ff;margin-bottom:12px;font-size:12px";
      loc.textContent = file + (line ? ":" + line + (col ? ":" + col : "") : "");
      body.appendChild(loc);
    }
    const pre = document.createElement("pre");
    pre.style.cssText =
      "margin:0;white-space:pre-wrap;background:#0d0d12;border:1px solid #26262f;border-radius:8px;" +
      "padding:12px 14px;color:#c9c9d4;max-height:52vh;overflow:auto";
    pre.textContent = frame;
    body.appendChild(pre);
    if (e.stack && String(e.stack).trim() && !String(frame).includes(String(e.stack).trim())) {
      const stack = document.createElement("pre");
      stack.style.cssText = "margin:12px 0 0;white-space:pre-wrap;color:#8d8d9c;font-size:12px";
      stack.textContent = e.stack;
      body.appendChild(stack);
    }
    const hint = document.createElement("div");
    hint.style.cssText = "margin-top:12px;color:#6f6f80;font-size:12px";
    hint.textContent = "Fix the file and save to retry · click outside or press Esc to dismiss";
    body.appendChild(hint);
    card.append(bar, body);
    backdrop.appendChild(card);
    // Dismiss on backdrop click only, so text inside the card stays selectable.
    backdrop.addEventListener("click", (ev) => {
      if (ev.target === backdrop) this.close();
    });
    this._onKey = (ev) => {
      if (ev.key === "Escape") this.close();
    };
    window.addEventListener("keydown", this._onKey);
    root.appendChild(backdrop);
  }
  close() {
    window.removeEventListener("keydown", this._onKey);
    this.remove();
    if (overlayEl === this) overlayEl = null;
  }
}

const overlayId = "vite-error-overlay";
if (typeof customElements !== "undefined" && !customElements.get(overlayId)) {
  customElements.define(overlayId, ErrorOverlay);
}

function showOverlay(err) {
  clearOverlay();
  if (!enableOverlay) return;
  overlayEl = new ErrorOverlay(err);
  document.body.appendChild(overlayEl);
}

function clearOverlay() {
  document.querySelectorAll(overlayId).forEach((n) => n.close());
  overlayEl = null;
}

function swapCss(update) {
  const links = [...document.querySelectorAll("link[rel=stylesheet]")];
  const link = links.find((l) => new URL(l.href).pathname === update.path);
  if (link) {
    const next = link.cloneNode();
    next.href = update.path + "?t=" + update.timestamp;
    next.addEventListener("load", () => link.remove());
    link.after(next);
    console.log(`[oj] css updated ${update.path}`);
    return;
  }
  // A stylesheet imported from JS (`import "./index.css"`) lives in a <style> tag
  // that its module wrapper wrote via updateStyle. Re-import the wrapper with a
  // fresh timestamp so it re-runs against the recompiled css and swaps the tag in
  // place, as Vite's client does for a css-update; reloading here would drop all
  // component state on every edit in a Tailwind app.
  if (styleTags.has(update.path)) {
    import(update.path + "?import&t=" + update.timestamp)
      .then(() => console.log(`[oj] css updated ${update.path}`))
      .catch((err) => {
        console.log(`[oj] css re-import failed for ${update.path}, reloading`, err);
        reportHmrFailure(update.path, err);
        location.reload();
      });
    return;
  }
  console.log(`[oj] no <link> or <style> for ${update.path}, reloading`);
  location.reload();
}

let socket = null;
let hadConnection = false;

// Vite 8's forwardConsole channel (src/shared/forwardConsole.ts): unhandled
// errors and the configured console levels travel to the dev server as the
// custom event `vite:forward-console`, so the server (and whatever supervises
// it) sees what only the browser console knew. Sends queue until the socket
// opens — a page-boot error predates it — and are budgeted so a render loop
// cannot flood the socket.
const forwardQueue = [];
let forwardSpent = 0;
let forwardWindow = Date.now();
function forwardSend(data) {
  const now = Date.now();
  if (now - forwardWindow >= 60000) {
    forwardSpent = 0;
    forwardWindow = now;
  }
  if (forwardSpent >= 100) return;
  forwardSpent++;
  const frame = JSON.stringify({ type: "custom", event: "vite:forward-console", data });
  if (socket && socket.readyState === WebSocket.OPEN) socket.send(frame);
  else if (forwardQueue.length < 50) forwardQueue.push(frame);
}

function forwardError(type, error) {
  forwardSend({
    type,
    data: {
      name: (error && error.name) || "Unknown Error",
      message: (error && error.message) || String(error),
      stack: error && error.stack,
    },
  });
}

function stringifyConsoleArg(arg) {
  if (typeof arg === "string") return arg;
  if (typeof arg === "bigint") return arg + "n";
  if (arg instanceof Error) return arg.stack || String(arg);
  try {
    return JSON.stringify(arg) ?? String(arg);
  } catch {
    return String(arg);
  }
}

// A compact take on Vite's formatConsoleArgs: the leading format string's
// %-specifiers consume arguments, the rest are appended stringified.
function formatConsoleArgs(args) {
  if (args.length === 0) return "";
  if (typeof args[0] !== "string") return args.map(stringifyConsoleArg).join(" ");
  let i = 1;
  const message = args[0].replace(/%[sdjifoOc%]/g, (spec) => {
    if (spec === "%%") return "%";
    if (i >= args.length) return spec;
    const arg = args[i++];
    switch (spec) {
      case "%s":
        return typeof arg === "object" && arg != null ? stringifyConsoleArg(arg) : String(arg);
      case "%d":
      case "%i":
        return typeof arg === "bigint" ? arg + "n" : String(parseInt(arg, 10));
      case "%f":
        return String(parseFloat(arg));
      case "%j":
      case "%o":
      case "%O":
        return stringifyConsoleArg(arg);
      case "%c":
        return "";
      default:
        return spec;
    }
  });
  return [message, ...args.slice(i).map(stringifyConsoleArg)].join(" ");
}

if (forwardConsole && forwardConsole.enabled) {
  for (const level of forwardConsole.logLevels || []) {
    const original = console[level];
    if (typeof original !== "function") continue;
    console[level] = (...args) => {
      original.apply(console, args);
      forwardSend({ type: "log", data: { level, message: formatConsoleArgs(args).slice(0, 4096) } });
    };
  }
  if (forwardConsole.unhandledErrors && typeof window !== "undefined") {
    window.addEventListener("error", (event) => {
      forwardError("error", event.error ?? (event.message ? new Error(event.message) : event));
    });
    window.addEventListener("unhandledrejection", (event) => {
      forwardError("unhandled-rejection", event.reason);
    });
  }
}

// Tell the server an HMR update failed to apply in this page (Vite has no
// equivalent signal): the server records it as an hmr_apply_failed diagnostic
// instead of the failure living and dying in this console.
function reportHmrFailure(path, error) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(
      JSON.stringify({
        type: "custom",
        event: "oj:hmr-result",
        data: { ok: false, path, error: String(error).slice(0, 4096) },
      }),
    );
  }
}

// The dev `base` this client is served under (Vite's __BASE__).
const base = new URL(import.meta.url).pathname.replace(/@oj\/client\.js$/, "");

// Where the socket lives (Vite's client): `server.hmr.{protocol,host,clientPort,
// path}` override the page's own origin, and the per-process token lets the
// server tell this page apart from another origin's.
function socketUrl() {
  const proto = hmrProtocol || (location.protocol === "https:" ? "wss" : "ws");
  const hostname = hmrHostname || location.hostname;
  const port = hmrPort || location.port;
  const host = port ? `${hostname}:${port}` : hostname;
  return `${proto}://${host}${hmrPath}?token=${wsToken}`;
}

(function connect() {
  const ws = new WebSocket(socketUrl());
  socket = ws;
  ws.addEventListener("message", (event) => {
    let msg;
    try {
      msg = JSON.parse(event.data);
    } catch {
      return;
    }
    if (msg.type === "connected") {
      // Vite-protocol greeting; nothing to do.
    } else if (msg.type === "update") {
      // Vite's UpdatePayload: css-update entries swap stylesheets, js-update
      // entries re-import boundaries.
      if (clearOverlayOrReloadOnFirstUpdate() === "reload") return;
      for (const u of msg.updates || []) {
        if (u.type === "css-update") swapCss(u);
        else queueUpdate(u);
      }
    } else if (msg.type === "css-update") {
      swapCss(msg);
    } else if (msg.type === "full-reload") {
      emit("vite:beforeFullReload", msg);
      if (msg.path && msg.path.endsWith(".html")) {
        // An edited page reloads only the tabs showing it (Vite's client);
        // index.html is the SPA shell behind every route, so it always does.
        const pagePath = decodeURI(location.pathname);
        const payloadPath = base + msg.path.slice(1);
        if (
          pagePath !== payloadPath &&
          msg.path !== "/index.html" &&
          !(pagePath.endsWith("/") && pagePath + "index.html" === payloadPath)
        ) {
          console.log(`[oj] ${msg.path} changed, not this page`);
          return;
        }
      }
      console.log("[oj] full reload:", msg.reason || msg.path || "");
      location.reload();
    } else if (msg.type === "prune") {
      emit("vite:beforePrune", msg);
      prunePaths(msg.paths || []);
    } else if (msg.type === "error") {
      // Vite's ErrorPayload carries `err`; older oj frames carried `message`.
      const err = msg.err || { message: msg.message };
      emit("vite:error", { err });
      if (enableOverlay) showOverlay(err);
      else console.error(`[oj] Internal Server Error\n${err.message}\n${err.stack || ""}`);
    } else if (msg.type === "custom") {
      emit(msg.event, msg.data);
    }
  });
  ws.addEventListener("open", () => {
    hadConnection = true;
    emit("vite:ws:connect", { webSocket: ws });
    for (const frame of forwardQueue.splice(0)) ws.send(frame);
    console.log("[oj] dev server connected");
  });
  ws.addEventListener("close", async () => {
    emit("vite:ws:disconnect", { webSocket: ws });
    if (hadConnection) {
      // The server went away (a config or .env change restarts it): its module
      // graph, defines, caches and socket token are new, so the page's modules
      // are stale. Like Vite's client, poll until it answers again, then reload
      // (the fresh page loads a client carrying the new token).
      console.log("[oj] dev server connection lost, polling for restart...");
      await waitForSuccessfulPing();
      console.log("[oj] dev server restarted, reloading");
      location.reload();
      return;
    }
    console.log("[oj] dev server disconnected, retrying in 1s…");
    setTimeout(connect, 1000);
  });
})();

// Vite's waitForSuccessfulPing: any HTTP answer from the socket's host means
// the server is back (the ws route itself replies to a plain GET).
async function waitForSuccessfulPing(ms = 1000) {
  const url = socketUrl().replace(/^ws/, "http").split("?")[0];
  for (;;) {
    try {
      await fetch(url, { mode: "no-cors", headers: { Accept: "text/x-vite-ping" } });
      return;
    } catch {
      await new Promise((r) => setTimeout(r, ms));
    }
  }
}
