// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The scan fairness queue (optimize-deps.mjs makeScanGate): units run one at
// a time in arrival order, the time budget is enforced BETWEEN units (an
// entry-time check is worthless: a flood of callers all passes it before any
// work runs), an over-budget queue parks on a real timer so other queued
// macrotasks (standing in for queued plugin-hook calls) get a turn, and a
// rejected unit neither breaks the queue nor loses its rejection. Injectable
// clock; the timers are real.

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const sidecar = path.join(here, "..", "..", "crates/oj_server/src/assets/optimize-deps.mjs");
const { makeScanGate } = await import(pathToFileURL(sidecar).href);

test("under budget a unit runs without yielding to the timer phase", async () => {
  let t = 0;
  const gate = makeScanGate(8, () => t);
  let macrotaskRan = false;
  setTimeout(() => (macrotaskRan = true), 0);
  t = 7;
  const out = await gate(() => "ran");
  assert.equal(out, "ran");
  assert.equal(macrotaskRan, false, "an under-budget unit must not wait on the timer phase");
});

test("work consumes the slice and the next unit parks behind queued macrotasks", async () => {
  let t = 0;
  const gate = makeScanGate(8, () => t);
  const order = [];
  setTimeout(() => order.push("queued-hook"), 0);
  // Each unit "burns" 6ms of fake time: the second one starts over budget.
  const burn = (name) => () => {
    order.push(name);
    t += 6;
  };
  await Promise.all([gate(burn("a")), gate(burn("b")), gate(burn("c"))]);
  // a and b fit the 8ms slice; c parks, letting the queued macrotask run first.
  assert.deepEqual(order, ["a", "b", "queued-hook", "c"]);
});

test("the park renews the slice", async () => {
  let t = 0;
  const gate = makeScanGate(8, () => t);
  await gate(() => {
    t += 20;
  });
  await gate(() => {}); // parks (20 >= 8), slice renews at resume
  let macrotaskRan = false;
  setTimeout(() => (macrotaskRan = true), 0);
  await gate(() => {}); // fresh slice: no park
  assert.equal(macrotaskRan, false, "the renewed slice must admit the next unit without parking");
});

test("idle time between units does not spend the slice", async () => {
  let t = 0;
  const gate = makeScanGate(8, () => t);
  await gate(() => {
    t += 7;
  });
  t += 10000; // the crawl sat in native code; the isolate was idle
  let macrotaskRan = false;
  setTimeout(() => (macrotaskRan = true), 0);
  await gate(() => {});
  assert.equal(macrotaskRan, false, "an idle gap must not force a park");
});

test("units run one at a time, in arrival order", async () => {
  const gate = makeScanGate(1000);
  let running = 0;
  const order = [];
  const unit = (name) => async () => {
    running++;
    assert.equal(running, 1, "two units overlapped");
    order.push(name);
    await null; // suspend mid-unit: the queue must still not admit the next
    running--;
  };
  await Promise.all([gate(unit("a")), gate(unit("b")), gate(unit("c"))]);
  assert.deepEqual(order, ["a", "b", "c"]);
});

test("a rejected unit reaches its caller and the queue keeps running", async () => {
  const gate = makeScanGate(1000);
  const boom = gate(() => {
    throw new Error("boom");
  });
  await assert.rejects(boom, /boom/);
  assert.equal(await gate(() => "after"), "after");
});
