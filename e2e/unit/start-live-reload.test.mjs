// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { asset } from "./harness.mjs";

// live-reload.js in a page with a stub socket, document and HMR runtime;
// `log` records reloads, sends and the runtime's notified events.
function loadPage({ overlays = [], stamp = "null", hot = "null", storage = new Map() } = {}) {
  const log = [];
  const sockets = [];
  const timers = [];
  class WebSocket {
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      sockets.push(this);
    }
    send(text) {
      log.push(["send", JSON.parse(text)]);
    }
  }
  const page = {
    WebSocket,
    console: { error() {}, debug() {}, warn() {} },
    location: { protocol: "http:", host: "localhost", pathname: "/", reload: () => log.push(["reload"]) },
    sessionStorage: {
      setItem: (k, v) => storage.set(k, String(v)),
      getItem: (k) => (storage.has(k) ? storage.get(k) : null),
      removeItem: (k) => storage.delete(k),
    },
    document: {
      readyState: "loading",
      activeElement: null,
      querySelectorAll: (sel) => (sel === "vite-error-overlay" ? overlays : []),
    },
    setTimeout: (fn) => timers.push(fn),
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
    .replaceAll("__OJ_HOT_CHANGE__", hot);
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
  return { bus: page.__ojStartHmr, log, open, receive, reconnect, url: sockets[0].url };
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
