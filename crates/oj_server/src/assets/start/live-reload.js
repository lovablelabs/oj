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

  // `build`: the client bundle's own failure, whose overlay goes when the
  // bundle builds again (a plugin's error overlay does not).
  function showError(err, build) {
    console.error("[oj] build error\n" + ((err && err.message) || err));
    if (!enableOverlay) return;
    import(base + "/@oj-start/error-overlay.js")
      .then(() => {
        const Overlay = customElements.get("vite-error-overlay");
        document.querySelectorAll("vite-error-overlay").forEach((n) => (n.close ? n.close() : n.remove()));
        const overlay = new Overlay(err);
        if (build) overlay.ojBuildError = true;
        document.body.appendChild(overlay);
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

  // The nonce for the tags this client creates, under a policy that names
  // one: Vite's `<meta property="csp-nonce">` (its client's `cspNonce`, which
  // `updateStyle` puts on each style tag it adds), else the nonce the server
  // gave this client's own tag, the one the document's tags carry.
  function cspNonce() {
    const of = (el) => (el && (el.nonce || el.getAttribute("nonce"))) || "";
    return (
      of(document.querySelector("meta[property=csp-nonce]")) ||
      of(document.querySelector('script[src*="/@oj-start/live-reload.js"]'))
    );
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
      const nonce = cspNonce();
      if (nonce) el.setAttribute("nonce", nonce);
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
  // For the bundle's runtime, which names them on each `import()` it asks
  // the server for (hmr-runtime.js `lazyChunkUrl`).
  bus.hotHas = () => hotHas;
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
  // Whether this document may reload for being behind under `mark`: once.
  // One that comes back under the same mark was served from a cache (a
  // service worker's, say), and reloading again would only loop.
  const behindOnce = (mark) => {
    let last = null;
    try {
      last = sessionStorage.getItem(BEHIND_KEY);
    } catch {}
    if (last === mark) {
      console.warn("[oj] this page is older than the dev server's code, and reloading served it unchanged");
      return false;
    }
    try {
      sessionStorage.setItem(BEHIND_KEY, mark);
    } catch {}
    return true;
  };
  // Whether this document is reloading for being behind. The reload takes
  // as long as the server takes to answer it (a server that just restarted
  // is still building), and the document runs on until then: its socket
  // may be told it is behind, and each `import()` it makes is answered with
  // the same word. Those find the mark this document left itself, which
  // says a reload served the same document only to the one that comes
  // back: this one is still on its way out, and a second `import()` that
  // rejected here would put the app's error screen up over a page about to
  // be replaced.
  let leaving = false;
  // Sent with each request for a module behind an `import()`: which run of
  // the dev server rendered this document.
  bus.stamp = stamp;
  // The bundle's runtime may have asked for a module before this script ran,
  // naming neither stamp, and waits to ask again (hmr-runtime.js
  // `ojAskedUnnamed`).
  for (const known of bus.onStamps?.splice(0) ?? []) known();
  // For the answer to an `import()` made by a page another run of the dev
  // server rendered (the same reload its socket is told to make, under the
  // same guard); whether it reloads.
  bus.reloadBehind = () => {
    // On its way out already: the import waits with the one that reloaded.
    if (leaving) return true;
    if (!behindOnce(stamp)) return false;
    leaving = true;
    snapshot();
    location.reload();
    return true;
  };
  // For the answer to an `import()` of a module that does not compile.
  bus.showBuildError = (err) => showError(err, true);
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
  // The document's hot changes are part of the mark as well: hot updates
  // rename chunks back and forth (an edit taken back brings the old name
  // back) without a new stamp, so a tab can meet a name it reloaded for
  // under an earlier document, and that document is not this one. One
  // served from a cache comes back with the same changes, and stops.
  // Once per document and chunk: the server's frame and the import's own
  // rejection can both speak of one failed import, and the second, finding
  // the mark the first left, would say the reload served the same document
  // while this one is still on its way out. By name, not for the document
  // as a whole: a reload the user declined (a `beforeunload` prompt) leaves
  // the document where it is, and the next chunk it finds gone asks again.
  const reloadedFor = new Set();
  const reloadForGoneChunk = (what) => {
    if (stamp == null || reloadedFor.has(what)) return;
    const mark = stamp + "|" + (docHas ?? "") + "|" + what;
    let marks = [];
    try {
      const kept = JSON.parse(sessionStorage.getItem(GONE_CHUNK_KEY) || "[]");
      if (Array.isArray(kept)) marks = kept.filter((m) => typeof m === "string");
    } catch {}
    if (marks.includes(mark)) {
      console.warn(
        "[oj] a chunk of this page is gone from the dev server (" + what + "), and reloading served the same document",
      );
      return;
    }
    marks.push(mark);
    try {
      sessionStorage.setItem(GONE_CHUNK_KEY, JSON.stringify(marks.slice(-GONE_CHUNK_MARKS)));
    } catch {}
    console.warn("[oj] a chunk of this page is gone from the dev server (" + what + "); reloading");
    reloadedFor.add(what);
    snapshot();
    location.reload();
  };
  // The server says which chunk is gone (oj:chunk-gone) to every open page,
  // and the one that asked for it and did not get it is the one to reload:
  // the others, behind or not, have nothing broken. A page knows what it
  // asked for from its resource timeline, kept here by an observer since
  // the timeline's own buffer fills on a large app. A request that was
  // answered with the chunk does not count (`responseStatus`, where the
  // browser reports one): a page that has the chunk loaded keeps running
  // it, and the frame is about another page's request, made after the
  // chunk went. The import's rejection would not do: a router catches a
  // failed lazy import (TanStack Router's lazyRouteComponent), so no
  // unhandled rejection is ever raised, where Vite's `vite:preloadError` is
  // dispatched by its preload helper before the import rejects, caught or
  // not.
  const CHUNKS = base + "/@oj-start/";
  const asked = new Set();
  // The chunk a url names (null for a url that is not one of the dev
  // server's chunks), as the server names it in oj:chunk-gone.
  const chunkNamed = (raw) => {
    const url = String(raw).split(/[?#]/)[0];
    const at = url.indexOf(CHUNKS);
    if (at < 0) return null;
    const name = url.slice(at + CHUNKS.length);
    // A hot update, or the answer to an `import()`, is no chunk of the
    // bundle: one that fails rejects its import, as under Vite, and is
    // asked for again.
    if (name.startsWith("hmr/")) return null;
    try {
      return decodeURIComponent(name);
    } catch {
      return name;
    }
  };
  // Whether the request got its chunk. No status (an older browser) or 0
  // (one the browser will not tell) is not known to have.
  const answered = (entry) => {
    const status = entry.responseStatus;
    return typeof status === "number" && status > 0 && status < 400;
  };
  const noteAsked = (entries) => {
    for (const entry of entries) {
      const name = chunkNamed(entry.name);
      if (name != null && !answered(entry)) asked.add(name);
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
    // The chunk by its name, as the server's frame has it: the two can both
    // speak of one failed import, and it is reloaded for once.
    const chunk = named ? chunkNamed(named[0]) : null;
    if (named ? chunk == null : ws && ws.readyState === 1) return;
    if (typeof fetch !== "function") return;
    fetch(CHUNKS + "client-entry.js", { method: "HEAD", cache: "no-store" }).then(
      (answer) => {
        if (answer && answer.ok) reloadForGoneChunk(chunk ?? message);
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
        showError(msg.err, msg.build === true);
        return;
      }
      // The client bundle builds again. An update this page applies clears
      // the overlay itself; a fix with nothing for this page to run (the
      // broken module was one it has not loaded) sends no update, and the
      // overlay would stay up. Only a page whose bundle ran: one that
      // opened on the failed build is reloaded onto the fixed one.
      if (msg && msg.type === "oj:build-ok") {
        if (bus.runtime && enableOverlay) {
          document.querySelectorAll("vite-error-overlay").forEach((n) => {
            if (n.ojBuildError) n.close ? n.close() : n.remove();
          });
        }
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
      // A module that declares a server function was patched in place: the
      // router loads its matches again, as the route module's accept handler
      // does under Vite, so a loader that calls the function shows what the
      // edited handler answers. `_refreshRoute` is what that handler calls
      // from router-core 1.171.18; before it, `invalidate({ sync: true })`.
      // Not awaited, as there: a loader that never settles would hold every
      // later frame of this socket, the next patch and a reload among them.
      if (msg && msg.type === "oj:routes") {
        const router = self.__TSR_ROUTER__;
        const refresh =
          router && typeof router._refreshRoute === "function"
            ? () => router._refreshRoute()
            : router && typeof router.invalidate === "function"
              ? () => router.invalidate({ sync: true })
              : null;
        if (refresh) {
          try {
            Promise.resolve(refresh()).catch((err) => console.error("[oj]", err));
          } catch (err) {
            console.error("[oj]", err);
          }
        }
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
        if (leaving || !behindOnce(msg.hot ? stamp + "#h" + hotHas : stamp)) return;
        leaving = true;
      }
    }
    await notify("vite:beforeFullReload", { type: "full-reload" });
    snapshot();
    location.reload();
  };
  connect();
})();
