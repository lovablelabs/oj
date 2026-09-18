// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import * as RefreshRuntime from "/@oj/refresh-runtime.js";

RefreshRuntime.injectIntoGlobalHook(window);
window.$RefreshReg$ = () => {};
window.$RefreshSig$ = () => (type) => type;
window.process ??= { env: { NODE_ENV: "development" } };
window.global ??= window;
window.setImmediate ??= (fn, ...args) => setTimeout(fn, 0, ...args);
window.clearImmediate ??= (id) => clearTimeout(id);

const registry = new Map();
const instances = new Map();
let socket = null;

window.__oj_register = (url, kind, deps, factory) => {
  registry.set(url, { kind, deps: deps || {}, factory });
};

function instantiate(url) {
  const reg = registry.get(url);
  if (!reg) throw new Error(`[oj] module not registered: ${url}`);
  const module = { exports: {}, hot: makeHot(url) };
  const record = { module, exports: module.exports, ns: null };
  instances.set(url, record);

  const localRequire = (spec) => {
    const target = reg.deps[spec] ?? spec;
    return requireRaw(target, reg.kind);
  };

  const isAppEsm = reg.kind === "esm" && !url.startsWith("/node_modules/");
  const prevReg = window.$RefreshReg$;
  const prevSig = window.$RefreshSig$;
  if (isAppEsm) {
    window.$RefreshReg$ = (type, id) => RefreshRuntime.register(type, url + " " + id);
    window.$RefreshSig$ = RefreshRuntime.createSignatureFunctionForTransform;
  }
  try {
    if (reg.kind === "cjs") {
      reg.factory.call(module.exports, module, module.exports, localRequire);
      record.exports = module.exports;
    } else {
      reg.factory.call(undefined, module, module.exports, localRequire);
    }
  } finally {
    if (isAppEsm) {
      window.$RefreshReg$ = prevReg;
      window.$RefreshSig$ = prevSig;
    }
  }
  if (isAppEsm) {
    RefreshRuntime.registerExportsForReactRefresh(url, module.exports);
  }
  return record;
}

function requireRaw(url, importerKind) {
  const record = instances.get(url) ?? instantiate(url);
  const target = registry.get(url);
  if (importerKind === "esm" && target && target.kind === "cjs") {
    if (!record.ns) record.ns = cjsNamespace(record);
    return record.ns;
  }
  return record.exports;
}

function cjsNamespace(record) {
  const ns = { __proto__: null };
  const raw = () => record.exports;
  Object.defineProperty(ns, "default", {
    enumerable: true,
    get: () => (raw().__esModule ? raw().default : raw()),
  });
  for (const key of Object.keys(record.exports)) {
    if (key !== "default") {
      Object.defineProperty(ns, key, { enumerable: true, get: () => raw()[key] });
    }
  }
  Object.defineProperty(ns, "__cjs_exports", { enumerable: true, get: raw });
  return ns;
}

window.__oj_esm = (exports, getters) => {
  Object.defineProperty(exports, "__esModule", { value: true });
  for (const name of Object.keys(getters)) {
    Object.defineProperty(exports, name, { enumerable: true, get: getters[name] });
  }
};
window.__oj_export_star = (from, exports) => {
  for (const key of Object.keys(from)) {
    if (key !== "default" && !Object.prototype.hasOwnProperty.call(exports, key)) {
      Object.defineProperty(exports, key, { enumerable: true, get: () => from[key] });
    }
  }
};

window.__oj_import_lazy = async (url) => {
  const clean = url.split("?")[0];
  if (!registry.has(url) && !registry.has(clean)) {
    const have = [...registry.keys()].map(encodeURIComponent).join(",");
    await import(`/@oj/lazy.js?id=${encodeURIComponent(url)}&have=${have}`);
  }
  return requireRaw(registry.has(url) ? url : clean, "esm");
};

const styleTags = new Map();
window.__oj_inject_css = (id, css) => {
  let tag = styleTags.get(id);
  if (!tag) {
    tag = document.createElement("style");
    tag.setAttribute("data-oj-id", id);
    document.head.appendChild(tag);
    styleTags.set(id, tag);
  }
  tag.textContent = css;
};

window.__oj_start = (entry) => {
  try {
    requireRaw(entry, "esm");
  } catch (err) {
    showOverlay(`boot failed\n\n${err && err.stack ? err.stack : err}`);
    throw err;
  }
};

function makeHot(url) {
  return {
    data: {},
    accept() {},
    dispose() {},
    invalidate() {
      escalateInvalidate(url);
    },
  };
}

function escalateInvalidate(url) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify({ type: "invalidate", path: url }));
  } else {
    location.reload();
  }
}

let lastPatchSeq = 0;

async function applyPatch(msg) {
  if (typeof msg.seq === "number") {
    if (lastPatchSeq !== 0 && msg.seq !== lastPatchSeq + 1) {
      console.warn(`[oj] patch gap (${lastPatchSeq} -> ${msg.seq}), reloading`);
      location.reload();
      return;
    }
    lastPatchSeq = msg.seq;
  }
  // The /__ws broadcast reaches every frame; boundaries this frame's registry
  // never registered belong to another frame's graph (seq stays tracked above
  // so a later relevant patch is not mistaken for a gap).
  const boundaries = (msg.boundaries || []).filter((b) => registry.has(b));
  if (boundaries.length === 0 && (msg.boundaries || []).length > 0) return;
  const prevExports = new Map();
  for (const boundary of boundaries) {
    const record = instances.get(boundary);
    if (record) prevExports.set(boundary, record.exports);
  }
  try {
    await import(`/@oj/patch.js?m=${encodeURIComponent(msg.changed.join(","))}&t=${msg.timestamp}`);
    for (const url of msg.dirty) instances.delete(url);
    for (const boundary of boundaries) {
      const next = requireRaw(boundary, "esm");
      if (boundary.split("?")[0].endsWith(".css")) continue;
      const prev = prevExports.get(boundary);
      if (prev) {
        const invalidate = RefreshRuntime.validateRefreshBoundaryAndEnqueueUpdate(
          boundary,
          prev,
          next
        );
        if (invalidate) {
          console.warn(`[oj] ${boundary}: ${invalidate}, escalating to importers`);
          escalateInvalidate(boundary);
          return;
        }
      } else {
        RefreshRuntime.enqueueUpdate();
      }
    }
    clearOverlay();
    console.log(`[oj] patched ${msg.changed.join(", ")}`);
  } catch (err) {
    showOverlay(`patch failed\n\n${err && err.stack ? err.stack : err}`);
  }
}

function swapCss(update) {
  const links = [...document.querySelectorAll("link[rel=stylesheet]")];
  const link = links.find((l) => new URL(l.href).pathname === update.path);
  if (!link) {
    location.reload();
    return;
  }
  const next = link.cloneNode();
  next.href = update.path + "?t=" + update.timestamp;
  next.addEventListener("load", () => link.remove());
  link.after(next);
}

let overlayEl = null;
function showOverlay(text) {
  clearOverlay();
  overlayEl = document.createElement("div");
  overlayEl.style.cssText =
    "position:fixed;inset:0;z-index:99999;background:rgba(12,12,14,0.92);" +
    "color:#ff8484;font:13px/1.5 ui-monospace,Menlo,monospace;padding:2rem;overflow:auto";
  const pre = document.createElement("pre");
  pre.style.cssText = "white-space:pre-wrap;margin:0";
  pre.textContent = "[oj] " + text;
  overlayEl.appendChild(pre);
  overlayEl.addEventListener("click", clearOverlay);
  document.body.appendChild(overlayEl);
}
function clearOverlay() {
  if (overlayEl) {
    overlayEl.remove();
    overlayEl = null;
  }
}

(function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  // The socket path and per-process token are filled in by the server (see
  // client.js): a browser upgrade without the token is refused.
  const ws = new WebSocket(proto + "://" + location.host + __HMR_PATH__ + "?token=" + __WS_TOKEN__);
  socket = ws;
  ws.addEventListener("message", (event) => {
    let msg;
    try {
      msg = JSON.parse(event.data);
    } catch {
      return;
    }
    if (msg.type === "patch") applyPatch(msg);
    else if (msg.type === "css-update") swapCss(msg);
    else if (msg.type === "update") for (const u of msg.updates || []) { if (u.type === "css-update") swapCss(u); }
    else if (msg.type === "full-reload") location.reload();
    else if (msg.type === "error") showOverlay((msg.err && msg.err.message) || msg.message || "unknown error");
  });
  let opened = false;
  ws.addEventListener("open", () => {
    opened = true;
    console.log("[oj] dev server connected (bundle mode)");
  });
  ws.addEventListener("close", async () => {
    if (!opened) return setTimeout(connect, 1000);
    // The server restarted: its token and caches are new, so poll until it
    // answers and reload (as client.js and Vite do).
    const url = (location.protocol === "https:" ? "https" : "http") + "://" + location.host + __HMR_PATH__;
    for (;;) {
      try {
        await fetch(url, { mode: "no-cors", headers: { Accept: "text/x-vite-ping" } });
        break;
      } catch {
        await new Promise((r) => setTimeout(r, 1000));
      }
    }
    location.reload();
  });
})();
