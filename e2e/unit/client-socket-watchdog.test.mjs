// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { asset } from "./harness.mjs";

// The SPA client (/@oj/client.js) as oj serves it, the shared socket watchdog
// ahead of it, in a page with stub sockets and timers. The watchdog's own
// rules (the grace, resume, the network coming back) are covered through the
// Start client in start-live-reload.test.mjs; these cover what this client
// does with a socket the watchdog drops. `clock.now` is what Date.now reads.
function loadPage({ ping = 0, hostname = "app.test", hmrHost = null, worker = false } = {}) {
  const log = [];
  const docListeners = {};
  const listeners = {};
  const sockets = [];
  const timers = [];
  const intervals = [];
  const clock = { now: 0 };
  const said = [];
  class WebSocket {
    static OPEN = 1;
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      this.listeners = {};
      sockets.push(this);
    }
    addEventListener(type, fn) {
      (this.listeners[type] ??= []).push(fn);
    }
    emit(type, event = {}) {
      if (type === "open") this.readyState = 1;
      if (type === "close") this.readyState = 3;
      for (const fn of this.listeners[type] ?? []) fn(event);
    }
    send(text) {
      log.push(["send", text]);
    }
    close(code, reason) {
      this.readyState = 2;
      this.closedWith = [code, reason];
      log.push(["close", sockets.indexOf(this)]);
    }
  }
  const page = {
    WebSocket,
    URL,
    console: { error() {}, debug() {}, warn() {}, log: (line) => said.push(line) },
    location: { protocol: "http:", host: hostname, hostname, port: "", reload: () => log.push(["reload"]) },
    document: {
      visibilityState: "visible",
      querySelectorAll: () => [],
      addEventListener: (type, fn) => (docListeners[type] ??= []).push(fn),
    },
    addEventListener: (type, fn) => (listeners[type] ??= []).push(fn),
    fetch: async () => (log.push(["poll"]), {}),
    Date: { now: () => clock.now },
    setTimeout: (fn) => (timers.push(fn), fn),
    clearTimeout() {},
    setInterval: (fn, ms) => intervals.push({ fn, ms }),
  };
  if (worker) delete page.document;
  page.globalThis = page;
  vm.createContext(page);
  const client = readFileSync(asset("client.js"), "utf8")
    .replace(/^export /gm, "")
    .replace("import.meta.url", '"http://app.test/@oj/client.js"')
    .replaceAll("__HMR_PROTOCOL__", "null")
    .replaceAll("__HMR_HOSTNAME__", JSON.stringify(hmrHost))
    .replaceAll("__HMR_PORT__", "null")
    .replaceAll("__HMR_PATH__", '"/__ws"')
    .replaceAll("__HMR_ENABLE_OVERLAY__", "true")
    .replaceAll("__WS_TOKEN__", '"tok"')
    .replaceAll("__FORWARD_CONSOLE__", '{"enabled":false}')
    .replaceAll("__OJ_PING_MS__", String(ping));
  vm.runInContext(readFileSync(asset("socket-watchdog.js"), "utf8") + client, page);
  const ws = () => sockets[sockets.length - 1];
  // Time passes as on a running page: each interval fires on its schedule.
  const advanceTo = (to) => {
    const due = intervals.map((iv) => ({ ...iv, next: iv.ms }));
    for (;;) {
      const next = due.filter((d) => d.next <= to).sort((a, b) => a.next - b.next)[0];
      if (!next) break;
      clock.now = Math.max(clock.now, next.next);
      next.next += next.ms;
      next.fn();
    }
    clock.now = to;
  };
  const settle = () => new Promise((r) => setImmediate(r));
  const fire = (type) => (listeners[type] ?? []).forEach((fn) => fn({}));
  const fireDoc = (type) => (docListeners[type] ?? []).forEach((fn) => fn({}));
  return { page, log, sockets, ws, timers, intervals, clock, said, advanceTo, settle, fire, fireDoc };
}

const PING = '{"type":"ping"}';

test("a socket silent past the deadline is dropped, and the page reloads once the server answers", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  // 1000ms pings: 2 * 1000 + 1000 = 3000ms deadline.
  page.advanceTo(3000);
  assert.deepEqual(page.log, [], "not before the deadline");
  page.advanceTo(3500);
  await page.settle();
  assert.deepEqual(page.sockets[0].closedWith, [1000, "oj: no word from the dev server in 3000ms"]);
  assert.ok(page.said.includes("[oj] no word from the dev server in 3000ms; reconnecting"));
  assert.deepEqual(page.log, [["poll"], ["close", 0], ["reload"]], "it missed whatever changed: it reloads, as Vite does");
});

test("the server's pings keep an idle socket", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  for (let t = 1000; t <= 20000; t += 1000) {
    page.advanceTo(t);
    page.ws().emit("message", { data: PING });
  }
  await page.settle();
  assert.equal(page.sockets.length, 1);
  assert.deepEqual(page.log, []);
});

test("the dropped socket's late close and frames are no longer the page's", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  page.advanceTo(3500);
  await page.settle();
  page.sockets[0].emit("message", { data: JSON.stringify({ type: "full-reload" }) });
  page.sockets[0].emit("close");
  await page.settle();
  assert.deepEqual(page.log, [["poll"], ["close", 0], ["reload"]], "one recovery, no second reload");
});

test("a socket stuck connecting past the deadline is retried", async () => {
  const page = loadPage({ ping: 1000 });
  page.advanceTo(3500);
  await page.settle();
  assert.deepEqual(page.log, [["close", 0]], "never connected: nothing to reload onto");
  assert.equal(page.timers.length, 1);
  page.timers.shift()();
  assert.equal(page.sockets.length, 2, "a new socket after the usual 1s retry");
});

test("a socket that closes still reloads the page once the server answers", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  page.ws().emit("close");
  await page.settle();
  assert.deepEqual(page.log, [["poll"], ["reload"]]);
});

test("with no keepalive interval there is no watchdog", async () => {
  const page = loadPage({ ping: 0 });
  page.ws().emit("open");
  assert.equal(page.intervals.length, 0);
  page.advanceTo(10 * 60 * 1000);
  await page.settle();
  assert.deepEqual(page.log, []);
});

test("each interval's ping keeps the socket: replaced only past two pings and one more", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  page.advanceTo(2000);
  page.ws().emit("message", { data: PING });
  // Heard at 2000: the deadline runs to 5000.
  page.advanceTo(5000);
  await page.settle();
  assert.deepEqual(page.log, []);
  page.advanceTo(5500);
  await page.settle();
  assert.deepEqual(page.log, [["poll"], ["close", 0], ["reload"]]);
});

test("any frame counts, not only pings", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  for (let t = 2500; t <= 20000; t += 2500) {
    page.advanceTo(t);
    page.ws().emit("message", { data: JSON.stringify({ type: "connected" }) });
  }
  await page.settle();
  assert.deepEqual(page.log, []);
});

test("a frame that is not JSON still counts", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  for (let t = 2500; t <= 20000; t += 2500) {
    page.advanceTo(t);
    page.ws().emit("message", { data: "not json" });
  }
  await page.settle();
  assert.deepEqual(page.log, []);
});

test("a page that was itself stopped gives its socket one more ping, and keeps one that is heard", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  // Stopped for a minute (a breakpoint, a sleeping laptop): no tick ran.
  page.clock.now = 60000;
  page.fireDoc("resume");
  await page.settle();
  assert.deepEqual(page.log, [], "the stop is the page's, not the socket's");
  page.ws().emit("message", { data: PING });
  // Heard at 60000: kept to its deadline, 63000.
  page.advanceTo(62900);
  await page.settle();
  assert.deepEqual(page.log, []);
  assert.equal(page.sockets.length, 1);
});

test("after a stop, a socket still silent once its grace runs out is dropped", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  page.clock.now = 60000;
  page.fireDoc("resume");
  // The grace: one more ping and a second.
  page.clock.now = 61900;
  page.fireDoc("resume");
  await page.settle();
  assert.deepEqual(page.log, []);
  page.clock.now = 62100;
  page.fireDoc("resume");
  await page.settle();
  assert.deepEqual(page.log, [["poll"], ["close", 0], ["reload"]]);
});

test("a tab coming back checks at once instead of at the next tick", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  page.advanceTo(3000);
  // Silent past the deadline between two ticks.
  page.clock.now = 3200;
  page.fireDoc("visibilitychange");
  await page.settle();
  assert.deepEqual(page.log, [["poll"], ["close", 0], ["reload"]]);
});

test("the network coming back drops a remote dev server's socket at once, silent or not", async () => {
  const page = loadPage({ ping: 1000, hostname: "box.dev.example" });
  page.ws().emit("open");
  page.fire("online");
  await page.settle();
  assert.deepEqual(page.sockets[0].closedWith, [1000, "oj: the network came back"]);
  assert.deepEqual(page.log, [["poll"], ["close", 0], ["reload"]]);
});

for (const hostname of ["localhost", "app.localhost", "127.0.0.1", "127.1.2.3", "[::1]"]) {
  test(`a dev server on this machine (${hostname}) keeps its socket when the network comes back`, async () => {
    const page = loadPage({ ping: 1000, hostname });
    page.ws().emit("open");
    page.fire("online");
    await page.settle();
    assert.deepEqual(page.log, []);
    assert.equal(page.sockets.length, 1);
  });
}

test("a dropped socket that opens late is not the page's: it neither counts as connected nor gets the queued frames", async () => {
  const page = loadPage({ ping: 1000 });
  // Stuck connecting past the deadline: dropped, a retry scheduled.
  page.advanceTo(3500);
  await page.settle();
  page.sockets[0].emit("open");
  page.timers.shift()();
  // The retry's socket closes: had the late open counted, this would reload.
  page.ws().emit("close");
  await page.settle();
  assert.deepEqual(page.log, [["close", 0]]);
  assert.equal(page.timers.length, 1, "it retries again instead");
});

test("the replacement socket after a stuck connect gets its own deadline", async () => {
  const page = loadPage({ ping: 1000 });
  page.advanceTo(3500);
  await page.settle();
  page.timers.shift()();
  page.ws().emit("open");
  page.advanceTo(6000);
  await page.settle();
  assert.deepEqual(page.log, [["close", 0]], "heard when it opened: not yet past its deadline");
  page.advanceTo(8000);
  await page.settle();
  assert.deepEqual(page.log, [["close", 0], ["poll"], ["close", 1], ["reload"]]);
});

test("the watchdog checks every half interval, and never more often than every 250ms", () => {
  assert.equal(loadPage({ ping: 15000 }).intervals[0].ms, 7500);
  assert.equal(loadPage({ ping: 300 }).intervals[0].ms, 250);
});

test("the host the socket dials decides, not the page's: a remote page whose socket dials localhost keeps it", async () => {
  const page = loadPage({ ping: 1000, hostname: "box.dev.example", hmrHost: "localhost" });
  assert.match(page.ws().url, /^ws:\/\/localhost\//);
  page.ws().emit("open");
  page.fire("online");
  await page.settle();
  assert.deepEqual(page.log, []);
});

test("a local page whose socket dials a remote host drops it when the network comes back", async () => {
  const page = loadPage({ ping: 1000, hostname: "localhost", hmrHost: "hmr.dev.example" });
  page.ws().emit("open");
  page.fire("online");
  await page.settle();
  assert.deepEqual(page.log, [["poll"], ["close", 0], ["reload"]]);
});

// Cases from Phoenix's socket tests (heartbeatTimeout, visibilitychange and
// resume) and engine.io's, applied to this client.

test("a dead socket is dropped once: every trigger after it starts no second recovery", async () => {
  const page = loadPage({ ping: 1000, hostname: "box.dev.example" });
  page.ws().emit("open");
  page.advanceTo(3500);
  page.clock.now = 3600;
  page.fireDoc("visibilitychange");
  page.fireDoc("resume");
  page.fire("online");
  page.advanceTo(10000);
  await page.settle();
  assert.deepEqual(page.log, [["poll"], ["close", 0], ["reload"]], "one close, one poll, one reload");
});

test("a socket the server closed is left to its own recovery: the watchdog starts no second one", async () => {
  // Phoenix: "does not let the scheduled reconnect replace a connection created by connect".
  const page = loadPage({ ping: 1000 });
  page.ws().emit("open");
  page.ws().emit("close");
  page.advanceTo(10000);
  await page.settle();
  assert.deepEqual(page.log, [["poll"], ["reload"]]);
});

test("a retry's socket stuck connecting is dropped too, and retried again", async () => {
  const page = loadPage({ ping: 1000 });
  page.ws().emit("close"); // never opened: the 1s retry
  page.timers.shift()();
  page.advanceTo(3500);
  await page.settle();
  assert.deepEqual(page.log, [["close", 1]]);
  assert.equal(page.timers.length, 1);
});

test("a worker importing the client, with no document, still loads it and gets the watchdog", async () => {
  const page = loadPage({ ping: 1000, worker: true });
  assert.equal(page.intervals.length, 1);
  page.ws().emit("open");
  page.advanceTo(3500);
  await page.settle();
  assert.deepEqual(page.log, [["poll"], ["close", 0], ["reload"]]);
});
