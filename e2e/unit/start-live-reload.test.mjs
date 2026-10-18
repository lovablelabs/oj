// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { asset } from "./harness.mjs";

// live-reload.js in a page with a stub socket, document and HMR runtime;
// `log` records reloads, sends and the runtime's notified events.
function loadPage({ overlays = [] } = {}) {
  const log = [];
  const sockets = [];
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
    console: { error() {}, debug() {} },
    location: { protocol: "http:", host: "localhost", pathname: "/", reload: () => log.push(["reload"]) },
    sessionStorage: { setItem() {}, getItem: () => null, removeItem() {} },
    document: {
      readyState: "loading",
      activeElement: null,
      querySelectorAll: (sel) => (sel === "vite-error-overlay" ? overlays : []),
    },
    setTimeout() {},
    __ojStartHmr: {
      runtime: {
        clientId: "c1",
        hmr: { notify: async (event, data) => log.push(["notify", event, data && { ...data }]) },
      },
    },
  };
  page.window = { addEventListener() {}, scrollX: 0, scrollY: 0 };
  page.globalThis = page;
  vm.createContext(page);
  const source = readFileSync(asset("start/live-reload.js"), "utf8").replaceAll("__HMR_ENABLE_OVERLAY__", "true");
  vm.runInContext(source, page);
  const ws = sockets[0];
  const open = () => {
    ws.readyState = 1;
    ws.onopen();
  };
  const receive = async (frame) => {
    ws.onmessage({ data: typeof frame === "string" ? frame : JSON.stringify(frame) });
    await new Promise((r) => setImmediate(r));
  };
  return { bus: page.__ojStartHmr, log, open, receive };
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
