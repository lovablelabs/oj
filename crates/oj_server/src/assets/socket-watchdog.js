// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The dev socket watchdog oj's dev clients share: the SPA client
// (/@oj/client.js) and the Start live-reload client. Kept free of either
// client's state: it asks for the page's current socket and hands a dead one
// back to `recover`, which detaches it and does whatever that client does
// after losing its socket.
//
// Liveness. The server sends every open socket a frame at least every
// `pingMs` (a ping when it has nothing else), and any frame shows the socket
// alive (`heard`). A connection dropped on the way without a close (a proxy
// or load balancer, a sleeping laptop) never fires `onclose`, so a socket
// that stays silent for `deadMs` is presumed dead and replaced, as Phoenix's
// heartbeat and Socket.IO's pingInterval + pingTimeout do. (The server's own
// deadline runs on the protocol's ping, which the browser answers without
// this script, so a page paused at a breakpoint keeps its socket.) 0: no
// keepalive, no watchdog.
function ojSocketWatchdog({ pingMs, socket, recover }) {
  const on = pingMs > 0;
  const deadMs = pingMs * 2 + Math.min(pingMs, 10000);
  let heardAt = Date.now();
  // Whether the socket already had its grace since it was last heard from
  // (see check).
  let graced = false;
  const heard = () => {
    heardAt = Date.now();
    graced = false;
  };
  // Drops the socket, open or still connecting. `recover` detaches its
  // handlers first: a close that arrives late must not start a second
  // reconnect. Said in the console and in the close, as Phoenix's "heartbeat
  // timeout": a page that stopped updating leaves a trace. The log comes
  // after `recover`: a client that forwards console lines over its socket
  // checks the deadline before sending, and must find the dead one gone.
  const replace = (why) => {
    const dead = socket();
    if (!dead || dead.readyState > 1) return;
    recover(dead);
    console.log("[oj] " + why + "; reconnecting");
    try {
      dead.close(1000, "oj: " + why);
    } catch {}
  };
  // The last time the watchdog ran. A run long after the one before means
  // this page, not its socket, was stopped (a breakpoint, an alert(), a
  // frozen or throttled tab, a sleeping laptop): frames may be waiting to be
  // delivered, and replacing the socket now would drop them. So the socket
  // gets one more ping's time (and a second) to be heard from, once; a
  // still-silent socket is replaced at the next check after that.
  let checkedAt = Date.now();
  const check = () => {
    if (!on) return;
    const now = Date.now();
    const stopped = now - checkedAt > deadMs;
    checkedAt = now;
    if (now - heardAt <= deadMs) return;
    if (stopped && !graced) {
      graced = true;
      heardAt = now - deadMs + pingMs + 1000;
      return;
    }
    replace("no word from the dev server in " + deadMs + "ms");
  };
  if (on) {
    setInterval(check, Math.max(250, Math.floor(pingMs / 2)));
    // A tab coming back (timers throttled while hidden): check now rather
    // than at the next tick. `resume` too, as Phoenix does: Chrome does not
    // reliably fire visibilitychange after a frozen page resumes
    // (https://issues.chromium.org/issues/547062449).
    // A worker that imports the client (Vite injects it wherever
    // `import.meta.hot` is used) has no document.
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", () => {
        if (document.visibilityState === "visible") check();
      });
      document.addEventListener("resume", check);
    }
    // The network came back: a socket from before it went (another network,
    // a new address) is dead whether or not it has been silent long enough
    // to tell. Socket.IO closes on "offline" for the same reason, and skips
    // a dev server on this machine, whose loopback socket survives.
    // Socket.IO asks the host its socket dials (`hostname !== "localhost"`),
    // not the page's: `server.hmr.host` can point the socket elsewhere.
    // This machine here is also localhost's subdomains (which browsers
    // resolve to loopback), 127.0.0.0/8, ::1, 127.0.0.0/8 mapped into IPv6
    // (which URL writes as ::ffff:7fXX:XXXX), and the unspecified addresses
    // 0.0.0.0 and :: (which browsers connect to this machine).
    const dials = () => {
      try {
        return new URL(socket().url).hostname;
      } catch {
        return location.hostname || "";
      }
    };
    const loopback = () =>
      /^(localhost|.+\.localhost|127(\.\d+){3}|0\.0\.0\.0|\[::1?\]|\[::ffff:7f[0-9a-f]{2}:[0-9a-f]{1,4}\])$/i.test(
        dials(),
      );
    addEventListener("online", () => (loopback() ? check() : replace("the network came back")));
  }
  // `check` before a send: never into a socket already past its deadline, as
  // engine.io checks its ping timeout before a write (a throttled timer may
  // not have fired yet).
  return { heard, check };
}
