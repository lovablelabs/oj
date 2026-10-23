// SPDX-License-Identifier: MIT

(() => {
  const KEY = "oj:start:preserve:" + location.pathname;
  const FIELDS = "input, textarea, select";

  function domPath(el) {
    const parts = [];
    while (el && el.nodeType === 1 && el !== document.body) {
      let i = 1;
      for (let s = el.previousElementSibling; s; s = s.previousElementSibling) {
        if (s.tagName === el.tagName) i++;
      }
      parts.unshift(el.tagName + ":" + i);
      el = el.parentElement;
    }
    return parts.join(">");
  }
  const key = (el) => (el.id ? "#" + el.id : "p:" + domPath(el));

  function snapshot() {
    const fields = {};
    for (const el of document.querySelectorAll(FIELDS)) {
      if (el.type === "password" || el.type === "file") continue;
      const rec = { v: el.value };
      if (el.type === "checkbox" || el.type === "radio") rec.c = el.checked;
      if (el.selectionStart != null) {
        rec.s = el.selectionStart;
        rec.e = el.selectionEnd;
      }
      fields[key(el)] = rec;
    }
    const active = document.activeElement;
    const scroll = { x: window.scrollX, y: window.scrollY };
    for (const el of document.querySelectorAll("[id]")) {
      if (el.scrollTop || el.scrollLeft) (scroll.el ??= {})["#" + el.id] = [el.scrollTop, el.scrollLeft];
    }
    const data = { fields, scroll, active: active && active.matches(FIELDS) ? key(active) : null };
    try {
      sessionStorage.setItem(KEY, JSON.stringify(data));
    } catch {}
  }

  function setValue(el, v) {
    const proto =
      el instanceof HTMLTextAreaElement
        ? HTMLTextAreaElement.prototype
        : el instanceof HTMLSelectElement
          ? HTMLSelectElement.prototype
          : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
    setter ? setter.call(el, v) : (el.value = v);
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
  }

  function apply(data) {
    const byKey = new Map();
    for (const el of document.querySelectorAll(FIELDS)) byKey.set(key(el), el);
    for (const [k, rec] of Object.entries(data.fields)) {
      const el = byKey.get(k);
      if (!el) continue;
      if (rec.c != null) {
        el.checked = rec.c;
        el.dispatchEvent(new Event("change", { bubbles: true }));
      } else if (el.value !== rec.v) setValue(el, rec.v);
    }
    window.scrollTo(data.scroll.x, data.scroll.y);
    for (const [sel, [t, l]] of Object.entries(data.scroll.el || {})) {
      const el = document.querySelector(sel);
      if (el) {
        el.scrollTop = t;
        el.scrollLeft = l;
      }
    }
    if (data.active) {
      const el = byKey.get(data.active);
      if (el) {
        el.focus();
        const rec = data.fields[data.active];
        if (rec && rec.s != null && el.setSelectionRange) {
          try {
            el.setSelectionRange(rec.s, rec.e);
          } catch {}
        }
      }
    }
  }

  function restore() {
    let data;
    try {
      data = JSON.parse(sessionStorage.getItem(KEY));
    } catch {}
    if (!data) return;
    sessionStorage.removeItem(KEY);
    let n = 0;
    const tick = () => {
      apply(data);
      if (++n < 4) setTimeout(tick, 60);
    };
    requestAnimationFrame(tick);
  }

  if (document.readyState === "complete") restore();
  else window.addEventListener("load", restore);

  // Vite's `server.hmr.overlay` (its client's __HMR_ENABLE_OVERLAY__), filled
  // in when the dev server serves this file: off, a build error is only logged.
  const enableOverlay = __HMR_ENABLE_OVERLAY__;
  // The prefix the dev server's urls take under `base`, filled in when the
  // dev server serves this file ("" at the root).
  const base = __OJ_DEV_BASE__;

  function showError(err) {
    console.error("[oj] build error\n" + ((err && err.message) || err));
    if (!enableOverlay) return;
    import(base + "/@oj-start/error-overlay.js")
      .then(() => {
        const Overlay = customElements.get("vite-error-overlay");
        document.querySelectorAll("vite-error-overlay").forEach((n) => (n.close ? n.close() : n.remove()));
        document.body.appendChild(new Overlay(err));
      })
      .catch(() => {});
  }

  // A Tailwind sheet scans the sources it styles, so an edit applied in place
  // can need new rules: re-link each same-origin stylesheet (Vite's css-update
  // swap: the old sheet stays until the new one loads, no unstyled flash).
  function refreshStyles() {
    for (const el of document.querySelectorAll('link[rel="stylesheet"]')) {
      const url = new URL(el.href, location.href);
      if (url.origin !== location.origin) continue;
      url.searchParams.set("t", Date.now());
      const next = el.cloneNode();
      next.href = url.href;
      const drop = () => el.remove();
      next.addEventListener("load", drop);
      next.addEventListener("error", drop);
      el.after(next);
    }
  }

  // Sheets an update imported for the first time (a CSS import is a link in
  // Start dev, never a module that injects its own sheet).
  function linkStyles(hrefs) {
    const have = new Set(
      [...document.querySelectorAll('link[rel="stylesheet"]')].map((el) => new URL(el.href, location.href).pathname),
    );
    for (const href of hrefs) {
      if (have.has(new URL(href, location.href).pathname)) continue;
      const el = document.createElement("link");
      el.rel = "stylesheet";
      el.href = href;
      document.head.appendChild(el);
    }
  }

  // The bundle's HMR runtime (hmr-runtime.js) and this script share the bus;
  // either may run first, so whichever comes second makes the introduction.
  const bus = (globalThis.__ojStartHmr ??= {});
  let ws;
  const send = (msg) => {
    if (!ws || ws.readyState !== 1) return false;
    ws.send(JSON.stringify(msg));
    return true;
  };
  bus.send = send;
  // `import.meta.hot.send`: one sent before the socket opens waits for it.
  const outbox = (bus.outbox ??= []);
  bus.post = (msg) => send(msg) || outbox.push(msg);
  bus.onRuntime = (runtime) => send({ type: "oj:client", id: runtime.clientId });
  const notify = (event, data) => bus.runtime?.hmr?.notify(event, data);

  // Vite's clearOverlayOrReloadOnFirstUpdate: an overlay up before the first
  // update means the page opened on a failed build and its bundle never ran,
  // so an update cannot fix it; otherwise the update clears the overlay.
  let firstUpdate = true;
  bus.beforeApply = () => {
    const overlays = document.querySelectorAll("vite-error-overlay");
    if (firstUpdate && overlays.length) {
      snapshot();
      location.reload();
      return "reload";
    }
    if (enableOverlay) overlays.forEach((n) => (n.close ? n.close() : n.remove()));
    firstUpdate = false;
    return "continue";
  };

  const connect = () => {
    ws = new WebSocket(
      (location.protocol === "https:" ? "wss" : "ws") + "://" + location.host + base + "/@oj-start/hmr",
    );
    ws.onopen = () => {
      if (bus.runtime) send({ type: "oj:client", id: bus.runtime.clientId });
      while (outbox.length && send(outbox[0])) outbox.shift();
    };
    // One message at a time, as Vite's createHMRHandler queues them: a
    // listener awaited for one finishes before the next message runs.
    ws.onmessage = (ev) => {
      inbox = inbox.then(() => handle(ev.data)).catch((err) => console.error("[oj]", err));
    };
    ws.onclose = () => setTimeout(connect, 1000);
  };
  let inbox = Promise.resolve();
  const handle = async (data) => {
    if (typeof data === "string" && data.startsWith("{")) {
      let msg = null;
      try {
        msg = JSON.parse(data);
      } catch {}
      // Vite's bundled dev error frame ({type:"error", err}): the client
      // bundle failed, so the page keeps what it has and shows the overlay;
      // the fixed rebundle's update or reload clears it.
      if (msg && msg.type === "error") {
        await notify("vite:error", msg);
        showError(msg.err);
        return;
      }
      // A plugin's `server.ws.send`: to the page's `import.meta.hot.on`.
      if (msg && msg.type === "custom") {
        if (typeof msg.event === "string") await notify(msg.event, msg.data);
        return;
      }
      if (msg && msg.type === "oj:hmr" && bus.runtime && bus.runtime.ojApply) {
        bus.runtime.ojApply(msg);
        return;
      }
      if (msg && msg.type === "oj:css") {
        if (msg.relink !== false) refreshStyles();
        linkStyles(msg.add || []);
        return;
      }
    }
    await notify("vite:beforeFullReload", { type: "full-reload" });
    snapshot();
    location.reload();
  };
  connect();
})();
