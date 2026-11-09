// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { asset } from "./harness.mjs";

// live-reload.js in a page with a stub socket, document and HMR runtime;
// `log` records reloads, sends and the runtime's notified events.
// `ping` is the keepalive interval filled in (0: no watchdog); `clock.now`
// is what the page's Date.now reads, and `tick()` runs its intervals.
function loadPage({
  overlays = [],
  stamp = "null",
  hot = "null",
  ping = 0,
  hostname = "localhost",
  storage = new Map(),
} = {}) {
  const log = [];
  const sockets = [];
  const timers = [];
  const intervals = [];
  const docListeners = {};
  const listeners = {};
  const clock = { now: 0 };
  const said = [];
  class WebSocket {
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      sockets.push(this);
    }
    send(text) {
      log.push(["send", JSON.parse(text)]);
    }
    close(code, reason) {
      this.readyState = 3;
      this.closedWith = [code, reason];
      log.push(["close", sockets.indexOf(this)]);
    }
  }
  const page = {
    WebSocket,
    console: { error() {}, debug() {}, warn() {}, log: (line) => said.push(line) },
    location: { protocol: "http:", host: hostname, hostname, pathname: "/", reload: () => log.push(["reload"]) },
    sessionStorage: {
      setItem: (k, v) => storage.set(k, String(v)),
      getItem: (k) => (storage.has(k) ? storage.get(k) : null),
      removeItem: (k) => storage.delete(k),
    },
    document: {
      readyState: "loading",
      activeElement: null,
      visibilityState: "visible",
      querySelectorAll: (sel) => (sel === "vite-error-overlay" ? overlays : []),
      addEventListener: (type, fn) => (docListeners[type] ??= []).push(fn),
    },
    addEventListener: (type, fn) => (listeners[type] ??= []).push(fn),
    Date: { now: () => clock.now },
    // A timer is its function; clearTimeout takes it back out.
    setTimeout: (fn) => (timers.push(fn), fn),
    clearTimeout: (fn) => {
      const i = timers.indexOf(fn);
      if (i >= 0) timers.splice(i, 1);
    },
    setInterval: (fn, ms) => intervals.push({ fn, ms }),
    __ojStartHmr: {
      runtime: {
        clientId: "c1",
        hmr: { notify: async (event, data) => log.push(["notify", event, data && { ...data }]) },
        ojApply: (msg) => log.push(["apply", msg.change]),
      },
    },
  };
  page.window = { addEventListener() {}, scrollX: 0, scrollY: 0 };
  page.globalThis = page;
  vm.createContext(page);
  const source = readFileSync(asset("start/live-reload.js"), "utf8")
    .replaceAll("__HMR_ENABLE_OVERLAY__", "true")
    .replaceAll("__OJ_DEV_BASE__", '""')
    .replaceAll("__OJ_PAGE_STAMP__", stamp)
    .replaceAll("__OJ_HOT_CHANGE__", hot)
    .replaceAll("__OJ_PING_MS__", String(ping));
  vm.runInContext(source, page);
  const ws = () => sockets[sockets.length - 1];
  const open = () => {
    ws().readyState = 1;
    ws().onopen();
  };
  const receive = async (frame) => {
    ws().onmessage({ data: typeof frame === "string" ? frame : JSON.stringify(frame) });
    await new Promise((r) => setImmediate(r));
  };
  // The socket drops; the client's retry timer fires: a new socket.
  const reconnect = () => {
    ws().onclose();
    timers.splice(0).forEach((fn) => fn());
    return ws().url;
  };
  const tick = () => intervals.forEach(({ fn }) => fn());
  // Time passing on a running page: its watchdog ticks at its interval.
  const advanceTo = (t) => {
    const step = intervals.length ? intervals[0].ms : t - clock.now;
    while (clock.now + step < t) {
      clock.now += step;
      tick();
    }
    clock.now = t;
    tick();
  };
  const emit = (on, type, event = {}) => (on[type] || []).forEach((fn) => fn(event));
  return {
    bus: page.__ojStartHmr,
    log,
    open,
    receive,
    reconnect,
    url: sockets[0].url,
    sockets,
    timers,
    intervals,
    clock,
    tick,
    advanceTo,
    page,
    visible: () => emit(docListeners, "visibilitychange"),
    resume: () => emit(docListeners, "resume"),
    pagehide: (persisted) => emit(listeners, "pagehide", { persisted }),
    pageshow: (persisted) => emit(listeners, "pageshow", { persisted }),
    said,
    online: () => emit(listeners, "online"),
  };
}

test("a plugin's custom frame reaches the page's hot.on listeners without a reload", async () => {
  const { log, open, receive } = loadPage();
  open();
  log.length = 0;
  await receive({ type: "custom", event: "app:pong", data: { n: 2 } });
  assert.deepEqual(JSON.parse(JSON.stringify(log)), [["notify", "app:pong", { n: 2 }]]);
});

test("a reload frame notifies vite:beforeFullReload, then reloads", async () => {
  const { log, open, receive } = loadPage();
  open();
  log.length = 0;
  await receive("reload");
  assert.deepEqual(JSON.parse(JSON.stringify(log)), [
    ["notify", "vite:beforeFullReload", { type: "full-reload" }],
    ["reload"],
  ]);
});

test("hot.send before the socket opens waits for it", () => {
  const { bus, log, open } = loadPage();
  bus.post({ type: "custom", event: "app:ping", data: 1 });
  assert.deepEqual(log, []);
  open();
  assert.deepEqual(JSON.parse(JSON.stringify(log)), [
    ["send", { type: "oj:client", id: "c1" }],
    ["send", { type: "custom", event: "app:ping", data: 1 }],
  ]);
});

test("an overlay up before the first update reloads, as Vite's clearOverlayOrReloadOnFirstUpdate", () => {
  const closed = [];
  const { bus, log } = loadPage({ overlays: [{ close: () => closed.push(1) }] });
  assert.equal(bus.beforeApply(), "reload");
  assert.deepEqual(log, [["reload"]]);
  assert.deepEqual(closed, []);
});

test("after the first update, an update closes the overlay instead", () => {
  const overlays = [];
  const { bus, log } = loadPage({ overlays });
  assert.equal(bus.beforeApply(), "continue");
  const closed = [];
  overlays.push({ close: () => closed.push(1) });
  assert.equal(bus.beforeApply(), "continue");
  assert.deepEqual(closed, [1]);
  assert.deepEqual(log, []);
});

test("the socket names the document's stamp, so a change it missed while loading reloads it", () => {
  assert.equal(loadPage({ stamp: '"1f-4"' }).url, "ws://localhost/@oj-start/hmr?g=1f-4");
  assert.equal(loadPage().url, "ws://localhost/@oj-start/hmr");
});

test("a page found behind on connect reloads once per stamp, so a cached document cannot loop", async () => {
  const storage = new Map();
  const reloadsFor = async (stamp, frame) => {
    const { log, open, receive } = loadPage({ stamp, storage });
    open();
    log.length = 0;
    await receive(frame);
    return log.filter(([kind]) => kind === "reload").length;
  };
  assert.equal(await reloadsFor('"1f-4"', { type: "oj:behind" }), 1);
  // The reload served the same document again (a cache): no second reload.
  assert.equal(await reloadsFor('"1f-4"', { type: "oj:behind" }), 0);
  // A newer document that is behind again reloads.
  assert.equal(await reloadsFor('"1f-5"', { type: "oj:behind" }), 1);
  // A change that goes out while the page is open always reloads it.
  assert.equal(await reloadsFor('"1f-5"', "reload"), 1);
});

test("the socket names the hot changes the document has, beside its stamp", () => {
  assert.equal(loadPage({ stamp: '"1f-4"', hot: "3" }).url, "ws://localhost/@oj-start/hmr?g=1f-4&h=3");
  assert.equal(loadPage({ hot: "0" }).url, "ws://localhost/@oj-start/hmr?h=0");
  // Without hot updates the document names none, and neither does the socket.
  assert.equal(loadPage({ stamp: '"1f-4"' }).url, "ws://localhost/@oj-start/hmr?g=1f-4");
});

test("frames the page applies raise the hot changes its next socket names", async () => {
  const { log, open, receive, reconnect } = loadPage({ stamp: '"1f-4"', hot: "3" });
  open();
  await receive({ type: "oj:hmr", url: "/@oj-start/hmr/hmr_patch_0.js", seq: 1, change: 4 });
  assert.deepEqual(
    log.filter(([k]) => k === "apply"),
    [["apply", 4]],
  );
  assert.equal(reconnect(), "ws://localhost/@oj-start/hmr?g=1f-4&h=4");
  open();
  // A style refresh is a change too; an older frame never lowers the count.
  await receive({ type: "oj:css", relink: false, add: [], change: 6 });
  await receive({ type: "oj:hmr", url: "/@oj-start/hmr/hmr_patch_1.js", seq: 2, change: 5 });
  assert.equal(reconnect(), "ws://localhost/@oj-start/hmr?g=1f-4&h=6");
  assert.equal(log.filter(([k]) => k === "reload").length, 0, "applying frames never reloads");
});

test("a change's end frame: named, the page has it; left out, it reloads unless its document is newer", async () => {
  const reloads = async (hot, frame) => {
    const { log, open, receive, reconnect } = loadPage({ stamp: '"1f-4"', hot });
    open();
    log.length = 0;
    await receive(frame);
    return { reloads: log.filter(([k]) => k === "reload").length, next: reconnect() };
  };
  // Named among the clients the change covered: it has the change.
  assert.deepEqual(await reloads("3", { type: "oj:change", change: 4, clients: ["c1", "c2"] }), {
    reloads: 0,
    next: "ws://localhost/@oj-start/hmr?g=1f-4&h=4",
  });
  // Left out (its client registered while the change was built): reload.
  assert.equal((await reloads("3", { type: "oj:change", change: 4, clients: ["c2"] })).reloads, 1);
  // Left out, but the document was rendered after the change: nothing missing.
  assert.equal((await reloads("4", { type: "oj:change", change: 4, clients: ["c2"] })).reloads, 0);
  assert.equal((await reloads("5", { type: "oj:change", change: 4, clients: ["c2"] })).reloads, 0);
});

test("a style refresh that lands first cannot hide a change the page was left out of", async () => {
  const { log, open, receive } = loadPage({ stamp: '"1f-4"', hot: "3" });
  open();
  log.length = 0;
  // The refresh raises the page's count past change 4 before 4's end frame.
  await receive({ type: "oj:css", relink: false, add: [], change: 5 });
  await receive({ type: "oj:change", change: 4, clients: ["c2"] });
  assert.equal(log.filter(([k]) => k === "reload").length, 1);
});

test("a page behind a hot change reloads once per document, so a cached document cannot loop", async () => {
  const storage = new Map();
  const reloadsFor = async (hot) => {
    const { log, open, receive } = loadPage({ stamp: '"1f-4"', hot, storage });
    open();
    log.length = 0;
    await receive({ type: "oj:behind", hot: true });
    return log.filter(([kind]) => kind === "reload").length;
  };
  assert.equal(await reloadsFor("3"), 1);
  // The reload served the same document again: no second reload.
  assert.equal(await reloadsFor("3"), 0);
  // A document that names newer changes, and is behind again, reloads.
  assert.equal(await reloadsFor("4"), 1);
});

// The keepalive: the server sends a frame at least every `ping` ms, and a
// socket silent for two intervals and a grace of one more (at most 10s) is
// presumed dead: 3000ms at a 1000ms ping. A connection dropped on the way
// without a close never fires onclose. The watchdog ticks every 500ms here.

test("the server's ping neither reloads the page, nor reaches its listeners, nor is answered", async () => {
  const { log, open, receive } = loadPage({ ping: 1000 });
  open();
  log.length = 0;
  // The server's own deadline runs on the protocol's ping, which the
  // browser answers without the page's scripts.
  await receive({ type: "oj:ping" });
  assert.deepEqual(log, []);
});

test("a ping counts at once, even behind a listener still busy with an update", async () => {
  const { open, receive, page, sockets, advanceTo } = loadPage({ ping: 1000 });
  open();
  // A plugin listener that never finishes holds the frame queue.
  page.__ojStartHmr.runtime.hmr.notify = () => new Promise(() => {});
  await receive({ type: "custom", event: "app:slow" });
  for (let t = 1000; t <= 6000; t += 1000) {
    advanceTo(t);
    await receive({ type: "oj:ping" });
  }
  assert.equal(sockets.length, 1, "heard before the queue, so the socket stays");
});

test("a socket silent past the deadline is replaced at once, naming the page's stamps", () => {
  const { log, open, sockets, clock, advanceTo, tick, timers } = loadPage({
    ping: 1000,
    stamp: '"1f-3"',
    hot: "4",
  });
  open();
  advanceTo(3000);
  assert.equal(sockets.length, 1, "not yet: 3000ms is the deadline itself");
  clock.now = 3001;
  tick();
  assert.equal(sockets.length, 2, "the silent socket was replaced");
  assert.ok(
    log.some((e) => e[0] === "close" && e[1] === 0),
    "the old socket was closed",
  );
  assert.equal(sockets[1].url, "ws://localhost/@oj-start/hmr?g=1f-3&h=4");
  assert.equal(timers.length, 0, "replaced now, not after the retry delay");
  assert.ok(!log.some((e) => e[0] === "reload"), "replacing the socket does not reload the page");
});

test("any frame keeps the socket: a busy socket outlives the deadline", async () => {
  const { open, receive, sockets, advanceTo } = loadPage({ ping: 1000 });
  open();
  for (let t = 1000; t <= 10000; t += 1000) {
    advanceTo(t);
    await receive(t % 2000 ? { type: "oj:ping" } : { type: "custom", event: "app:x" });
  }
  assert.equal(sockets.length, 1);
});

test("the replaced socket's late close starts no second reconnect", () => {
  const { open, sockets, advanceTo, timers } = loadPage({ ping: 1000 });
  open();
  const dead = sockets[0];
  advanceTo(3500);
  assert.equal(dead.onclose, null, "its handlers were detached first");
  assert.equal(dead.onmessage, null);
  assert.equal(timers.length, 0);
  assert.equal(sockets.length, 2);
});

test("a socket stuck connecting past the deadline is replaced too", () => {
  const { sockets, advanceTo } = loadPage({ ping: 1000 });
  advanceTo(3500);
  assert.equal(sockets.length, 2);
});

test("a replaced socket says why, in the console and in its close", () => {
  const { open, sockets, advanceTo, said } = loadPage({ ping: 1000 });
  open();
  advanceTo(3500);
  assert.deepEqual(sockets[0].closedWith, [1000, "oj: no word from the dev server in 3000ms"]);
  assert.deepEqual(said, ["[oj] no word from the dev server in 3000ms; reconnecting"]);
});

test("a page that was itself stopped gives its socket one more ping before replacing it", async () => {
  const { open, receive, sockets, clock, tick } = loadPage({ ping: 1000 });
  open();
  // A breakpoint, an alert() or a frozen tab: nothing ran for 50s, and the
  // frames the server sent meanwhile wait to be delivered.
  clock.now = 50000;
  tick();
  assert.equal(sockets.length, 1, "not replaced on the first look after the stop");
  // They arrive: the socket was fine all along.
  await receive({ type: "oj:hmr", change: 1 });
  clock.now = 50500;
  tick();
  clock.now = 52500;
  tick();
  assert.equal(sockets.length, 1, "kept");
});

test("after a stop, a socket still silent once its grace ran out is replaced", () => {
  const { open, sockets, clock, tick, advanceTo } = loadPage({ ping: 1000 });
  open();
  clock.now = 50000;
  tick();
  assert.equal(sockets.length, 1);
  advanceTo(51900);
  assert.equal(sockets.length, 1, "one ping and a second of grace");
  advanceTo(52100);
  assert.equal(sockets.length, 2, "dead after all");
});

test("the grace comes once: a throttled tab, every tick late, still replaces a dead socket", () => {
  const { open, sockets, clock, tick } = loadPage({ ping: 1000 });
  open();
  // A hidden tab's timers run once a minute.
  clock.now = 60000;
  tick();
  assert.equal(sockets.length, 1, "the first late look grants the grace");
  clock.now = 120000;
  tick();
  assert.equal(sockets.length, 2, "the next one, still silent, does not grant it again");
});

test("the network coming back replaces a remote dev server's socket at once, silent or not", () => {
  const { open, sockets, advanceTo, online, said } = loadPage({ ping: 1000, hostname: "preview.example.dev" });
  open();
  advanceTo(500);
  online();
  assert.equal(sockets.length, 2, "a socket from before the network went is dead");
  assert.deepEqual(said, ["[oj] the network came back; reconnecting"]);
});

test("a dev server on this machine keeps its loopback socket when the network comes back", () => {
  const { open, sockets, advanceTo, online } = loadPage({ ping: 1000 });
  open();
  advanceTo(500);
  online();
  assert.equal(sockets.length, 1, "loopback survives a network change");
  advanceTo(2900);
  online();
  assert.equal(sockets.length, 1, "within the deadline still");
  advanceTo(3100);
  assert.equal(sockets.length, 2, "but a silent one is still replaced");
});

test("a tab coming back, or a frozen page resuming, checks at once instead of at the next tick", () => {
  const { open, sockets, clock, advanceTo, visible, resume } = loadPage({ ping: 1000 });
  open();
  advanceTo(1000);
  visible();
  assert.equal(sockets.length, 1, "a socket heard from in time stays");
  // A hidden, running tab: its watchdog kept ticking, the socket went silent.
  advanceTo(3400);
  clock.now = 3600;
  visible();
  assert.equal(sockets.length, 2, "becoming visible replaces it at once");
  open();
  // A frozen page: nothing ran for a minute, so its socket gets its grace,
  // and the resume after it, still silent, replaces it.
  clock.now = 63600;
  resume();
  assert.equal(sockets.length, 2, "the first look after the freeze grants the grace");
  clock.now = 65700;
  resume();
  assert.equal(sockets.length, 3, "a resumed page whose socket stays silent replaces it");
});

test("a replacement that hears from the server is kept", async () => {
  const { open, receive, sockets, advanceTo } = loadPage({ ping: 1000 });
  open();
  advanceTo(3500);
  assert.equal(sockets.length, 2);
  open();
  for (let t = 4000; t <= 8000; t += 1000) {
    advanceTo(t);
    await receive({ type: "oj:ping" });
  }
  assert.equal(sockets.length, 2);
});

test("a message is never written into a socket past its deadline: it waits for the new one", () => {
  const { bus, log, open, sockets, advanceTo, clock } = loadPage({ ping: 1000 });
  open();
  log.length = 0;
  advanceTo(3000);
  // Past the deadline before the watchdog's next tick.
  clock.now = 3200;
  bus.post({ type: "custom", event: "app:late", data: 1 });
  assert.equal(sockets.length, 2, "checked before the write, and replaced");
  assert.ok(!log.some((e) => e[0] === "send"), "nothing went into the dead socket");
  open();
  assert.deepEqual(JSON.parse(JSON.stringify(log.filter((e) => e[0] === "send"))), [
    ["send", { type: "oj:client", id: "c1" }],
    ["send", { type: "custom", event: "app:late", data: 1 }],
  ]);
});

test("a page going into the back/forward cache closes its socket; restored, it connects again", () => {
  const { open, sockets, pagehide, pageshow, timers, clock, tick } = loadPage({ ping: 1000, stamp: '"1f-3"' });
  open();
  pagehide(false);
  assert.equal(sockets[0].closedWith, undefined, "an unload that is not cached leaves the socket");
  pagehide(true);
  assert.deepEqual(sockets[0].closedWith, [1000, "oj: page cached"]);
  assert.equal(sockets[0].onclose, null, "its close starts no reconnect");
  assert.equal(timers.length, 0);
  clock.now = 60000;
  tick();
  clock.now = 120000;
  tick();
  assert.equal(sockets.length, 1, "the watchdog leaves a cached page alone");
  pageshow(true);
  assert.equal(sockets.length, 2, "restored, it connects again");
  assert.equal(sockets[1].url, "ws://localhost/@oj-start/hmr?g=1f-3");
  pageshow(true);
  assert.equal(sockets.length, 2, "once");
});

test("a retry scheduled before the back/forward cache does not open a second socket after it", () => {
  const { open, sockets, pagehide, pageshow, timers } = loadPage({ ping: 1000 });
  open();
  // The socket closes just before the page is cached: a retry is pending,
  // and timers stop while the page is in the cache.
  sockets[0].onclose();
  assert.equal(timers.length, 1);
  pagehide(true);
  pageshow(true);
  assert.equal(sockets.length, 2, "restored, it connects");
  assert.equal(timers.length, 0, "and the stale retry is gone");
  timers.splice(0).forEach((fn) => fn());
  assert.equal(sockets.length, 2, "one socket, not two");
});

test("with no keepalive interval there is no watchdog", () => {
  const { open, sockets, clock, tick, intervals, visible } = loadPage({ ping: 0 });
  open();
  assert.equal(intervals.length, 0);
  clock.now = 10 * 60 * 1000;
  tick();
  visible();
  assert.equal(sockets.length, 1);
});
