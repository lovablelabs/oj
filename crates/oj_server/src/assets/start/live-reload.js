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
    // Never into a socket already past its deadline, as engine.io checks
    // its ping timeout before a write (a throttled timer may not have
    // fired yet): replaced first, the message waits for the new one (post).
    watchdog.check();
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

  // The changes this document had when the server rendered it, filled in
  // from this script's url (null without one). The socket names them, so a
  // change that went out while the page still loaded (no socket open to take
  // it) reloads the page on connect.
  const stamp = __OJ_PAGE_STAMP__;
  // The hot changes this page has: the document's, then each one it applies
  // (every oj:hmr / oj:css frame names its change). The socket names them on
  // every connect, so a change sent while this page loaded or while its
  // socket was down reloads it.
  const docHas = __OJ_HOT_CHANGE__;
  let hotHas = docHas;
  const applied = (msg) => {
    if (typeof msg.change === "number" && (hotHas == null || msg.change > hotHas)) hotHas = msg.change;
  };
  // A change's end frame (oj:change): does this page still need it? Named
  // among the clients it covered, it has it (patched, or nothing to patch).
  // Left out, it needs it, unless its document was rendered after the
  // change. Order-independent: the changes the page has applied (raised by
  // style refreshes too) play no part.
  // <oj:missed-change>
  const missedChange = (msg, id, docHas) =>
    typeof msg.change === "number" &&
    !(id != null && Array.isArray(msg.clients) && msg.clients.includes(id)) &&
    !(docHas != null && msg.change <= docHas);
  // </oj:missed-change>
  const BEHIND_KEY = "oj:start:behind:" + location.pathname;
  // A chunk this document names and the dev server no longer has: a rebundle
  // this page missed renamed it, and the old name has fallen out of the
  // bundles the server keeps serving (or its file was swept). The page's
  // entry is too old to run on, so it reloads, once per document and chunk:
  // a document that comes back asking for the same gone chunk was served
  // from a cache, and reloading again would only loop. Not once per stamp
  // alone, as a behind connect is: the stamp counts the reloads the pages
  // were sent (start_reloads), and hot updates rebundle (and rename chunks)
  // without one, so a tab that reloaded for one gone chunk would be left
  // blank at the next, its document still under the same stamp. The marks
  // are kept as a list, not the last one: a document that asks for two gone
  // chunks as it loads would otherwise reload for each in turn, forever.
  const GONE_CHUNK_KEY = "oj:start:stale-import:" + location.pathname;
  const GONE_CHUNK_MARKS = 64;
  const reloadForGoneChunk = (what) => {
    if (stamp == null) return;
    const mark = stamp + "|" + what;
    let marks = [];
    try {
      const kept = JSON.parse(sessionStorage.getItem(GONE_CHUNK_KEY) || "[]");
      if (Array.isArray(kept)) marks = kept.filter((m) => typeof m === "string");
    } catch {}
    if (marks.includes(mark)) {
      console.warn("[oj] a chunk of this page is gone from the dev server (" + what + "), and reloading served the same document");
      return;
    }
    marks.push(mark);
    try {
      sessionStorage.setItem(GONE_CHUNK_KEY, JSON.stringify(marks.slice(-GONE_CHUNK_MARKS)));
    } catch {}
    console.warn("[oj] a chunk of this page is gone from the dev server (" + what + "); reloading");
    snapshot();
    location.reload();
  };
  // The server says which chunk is gone (oj:chunk-gone) to every open page,
  // and the one that asked for it is the one to reload: the others, behind
  // or not, have nothing broken. A page knows what it asked for from its
  // resource timeline, kept here by an observer since the timeline's own
  // buffer fills on a large app. The import's rejection would not do: a
  // router catches a failed lazy import (TanStack Router's
  // lazyRouteComponent), so no unhandled rejection is ever raised, where
  // Vite's `vite:preloadError` is dispatched by its preload helper before
  // the import rejects, caught or not.
  const CHUNKS = base + "/@oj-start/";
  const asked = new Set();
  const noteAsked = (entries) => {
    for (const entry of entries) {
      const url = String(entry.name).split(/[?#]/)[0];
      const at = url.indexOf(CHUNKS);
      if (at < 0) continue;
      let name = url.slice(at + CHUNKS.length);
      try {
        name = decodeURIComponent(name);
      } catch {}
      asked.add(name);
    }
  };
  try {
    new PerformanceObserver((list) => noteAsked(list.getEntries())).observe({ type: "resource", buffered: true });
  } catch {}
  const askedFor = (name) => {
    if (!asked.has(name)) {
      try {
        noteAsked(performance.getEntriesByType("resource"));
      } catch {}
    }
    return asked.has(name);
  };
  // The frame can outrun the 404 it is about: look again shortly.
  const goneChunk = (name, waits = [50, 250, 1000]) => {
    if (askedFor(name)) return reloadForGoneChunk(name);
    if (waits.length) setTimeout(() => goneChunk(name, waits.slice(1)), waits[0]);
  };
  // With no socket to hear the server on, an import nothing caught still
  // says so ("Failed to fetch dynamically imported module", Firefox's "error
  // loading ...", Safari's "Importing a module script failed"). Only for the
  // dev server's own chunks, where the message names the url, and only if
  // the server answers: an import that failed because the server is
  // restarting is not a chunk gone, and reloading would trade the page for
  // a connection error, with no client left to bring it back. Safari's
  // message names no url, so it could be any import of the app's (a script
  // from a CDN): it counts only while the socket is down, when the server's
  // own word for it (oj:chunk-gone) cannot arrive.
  addEventListener("unhandledrejection", (e) => {
    const reason = e && e.reason;
    const message = reason && typeof reason.message === "string" ? reason.message : "";
    if (stamp == null || !/dynamically imported module|Importing a module script failed/.test(message)) return;
    const named = /https?:\/\/\S+/.exec(message);
    if (named ? !named[0].includes(CHUNKS) : ws && ws.readyState === 1) return;
    if (typeof fetch !== "function") return;
    fetch(CHUNKS + "client-entry.js", { method: "HEAD", cache: "no-store" }).then(
      (answer) => {
        if (answer && answer.ok) reloadForGoneChunk(message);
      },
      () => {},
    );
  });
  // Liveness (socket-watchdog.js, served ahead of this file): the server
  // sends every open socket a frame at least every `pingMs` (oj:ping when
  // it has nothing else), and a socket silent past the deadline is replaced
  // at once. The new one names the page's stamps, and the server reloads it
  // onto any change it missed meanwhile, so a blip with nothing changed
  // keeps the page as it is. 0: no keepalive, no watchdog.
  const pingMs = __OJ_PING_MS__;
  const PING = '{"type":"oj:ping"}';
  const watchdog = ojSocketWatchdog({
    pingMs,
    socket: () => ws,
    recover: (dead) => {
      dead.onopen = dead.onmessage = dead.onclose = null;
      connect();
    },
  });
  const heard = watchdog.heard;
  // The retry a close scheduled. Whatever opens a socket first cancels it,
  // as Phoenix resets its reconnect timer: one scheduled before a page went
  // into the back/forward cache fires on the way back, after pageshow
  // already connected, and two sockets would carry every frame twice.
  let retry = null;
  const connect = () => {
    clearTimeout(retry);
    retry = null;
    const query = [];
    if (stamp != null) query.push("g=" + encodeURIComponent(stamp));
    if (hotHas != null) query.push("h=" + hotHas);
    ws = new WebSocket(
      (location.protocol === "https:" ? "wss" : "ws") +
        "://" +
        location.host +
        base +
        "/@oj-start/hmr" +
        (query.length ? "?" + query.join("&") : ""),
    );
    heard();
    ws.onopen = () => {
      heard();
      if (bus.runtime) send({ type: "oj:client", id: bus.runtime.clientId });
      while (outbox.length && send(outbox[0])) outbox.shift();
    };
    // One message at a time, as Vite's createHMRHandler queues them: a
    // listener awaited for one finishes before the next message runs.
    ws.onmessage = (ev) => {
      heard();
      if (ev.data === PING) return;
      inbox = inbox.then(() => handle(ev.data)).catch((err) => console.error("[oj]", err));
    };
    ws.onclose = () => (retry = setTimeout(connect, 1000));
  };
  // The back/forward cache, as Phoenix does: a page going in closes its
  // socket (an open one can keep it out, and would be dead on the way back
  // anyway), and one restored from it connects again; its stamps bring it
  // up to whatever changed meanwhile.
  addEventListener("pagehide", (e) => {
    if (!e.persisted || !ws) return;
    const old = ws;
    old.onopen = old.onmessage = old.onclose = null;
    ws = null;
    try {
      old.close(1000, "oj: page cached");
    } catch {}
  });
  addEventListener("pageshow", (e) => {
    if (e.persisted && !ws) connect();
  });
  let inbox = Promise.resolve();
  const handle = async (data) => {
    if (typeof data === "string" && data.startsWith("{")) {
      let msg = null;
      try {
        msg = JSON.parse(data);
      } catch {}
      // The server's keepalive: hearing it was the point (socket-watchdog.js).
      if (msg && msg.type === "oj:ping") return;
      // Vite's bundled dev error frame ({type:"error", err}): the client
      // bundle failed, so the page keeps what it has and shows the overlay;
      // the fixed rebundle's update or reload clears it.
      if (msg && msg.type === "error") {
        await notify("vite:error", msg);
        showError(msg.err);
        return;
      }
      // A chunk some page asked for is gone from the server: this page
      // reloads if it was the one asking, and is otherwise left as it is.
      if (msg && msg.type === "oj:chunk-gone") {
        if (typeof msg.name === "string") goneChunk(msg.name);
        return;
      }
      // A plugin's `server.ws.send`: to the page's `import.meta.hot.on`.
      if (msg && msg.type === "custom") {
        if (typeof msg.event === "string") await notify(msg.event, msg.data);
        return;
      }
      if (msg && msg.type === "oj:hmr" && bus.runtime && bus.runtime.ojApply) {
        bus.runtime.ojApply(msg);
        applied(msg);
        return;
      }
      if (msg && msg.type === "oj:css") {
        if (msg.relink !== false) refreshStyles();
        linkStyles(msg.add || []);
        applied(msg);
        return;
      }
      // A change went out to the pages it names. Named, this page has it
      // (its patch came first, or it had nothing to patch). Left out, its
      // client was not there for it (registered while it was being built):
      // reload onto it.
      if (msg && msg.type === "oj:change") {
        if (!missedChange(msg, bus.runtime ? bus.runtime.clientId : null, docHas)) {
          applied(msg);
          return;
        }
      }
      // The page connected behind the server's changes: reload, once per
      // stamp. A document that comes back with the same stamp was served
      // from a cache (a service worker's, say), and reloading again would
      // only loop.
      if (msg && msg.type === "oj:behind") {
        // A missed hot change keys on the changes the page has too: the
        // reload brings a document stamped with newer ones.
        const mark = msg.hot ? stamp + "#h" + hotHas : stamp;
        let last = null;
        try {
          last = sessionStorage.getItem(BEHIND_KEY);
        } catch {}
        if (last === mark) {
          console.warn("[oj] this page is older than the dev server's code, and reloading served it unchanged");
          return;
        }
        try {
          sessionStorage.setItem(BEHIND_KEY, mark);
        } catch {}
      }
    }
    await notify("vite:beforeFullReload", { type: "full-reload" });
    snapshot();
    location.reload();
  };
  connect();
})();
