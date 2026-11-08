// SPDX-License-Identifier: MIT
// Focused unit coverage for e2e/lib/solid-start-html.mjs: scripts are
// stripped case-insensitively from the WHOLE document BEFORE boundaries are
// located, so head-script fake <body>/<title> payloads cannot satisfy
// healthy-route assertions, and missing bodies fail loudly.

import { test } from "node:test";
import assert from "node:assert/strict";
import { bodyWithoutScripts, headOf, stripScripts } from "../lib/solid-start-html.mjs";

const doc = (head, body) =>
  `<!DOCTYPE html><html><head>${head}</head><body>${body}</body></html>`;

test("missing body boundaries fail (no whole-document fallback)", () => {
  assert.throws(() => bodyWithoutScripts("<html><head></head><p>no body</p></html>"), /missing <body>/);
  assert.throws(() => headOf("<html><body>no head close</body></html>"), /missing <\/head>/);
});

test("uppercase <SCRIPT> payloads are stripped, markers inside fail", () => {
  const html = doc("", `<SCRIPT>var x = "solid-start-home-marker";</SCRIPT><h1>real</h1>`);
  const body = bodyWithoutScripts(html);
  assert.ok(!body.includes("solid-start-home-marker"), "uppercase script payload must be stripped");
  assert.ok(body.includes("<h1>real</h1>"));
  assert.equal(stripScripts("<SCRIPT>hi</SCRIPT>left"), "left");
});

test("fake <body>/<title> inside a head script cannot satisfy", () => {
  const fake = doc(
    `<script>var f = "</title><body><h1>solid-start-home-marker</h1><title>solid-start-fixture-home";</script>`,
    `<h1>empty</h1>`,
  );
  const body = bodyWithoutScripts(fake);
  assert.ok(!body.includes("solid-start-home-marker"), "script-faked body marker must not satisfy");
  const head = headOf(fake);
  assert.ok(!head.includes("solid-start-fixture-home"), "script-faked title must not satisfy");
  // And the genuine markers still satisfy once really rendered:
  const genuine = doc(`<title>solid-start-fixture-home</title>`, `<h1>solid-start-home-marker</h1>`);
  assert.ok(bodyWithoutScripts(genuine).includes("solid-start-home-marker"));
  assert.ok(headOf(genuine).includes("solid-start-fixture-home"));
});

test("a lone fake <body> inside a closed script still throws (no fallback)", () => {
  // No real body tags at all — only a fake pair inside a stripped script.
  // After whole-document stripping there are no boundaries, so it throws
  // instead of falling back to the whole document.
  const html = `<!DOCTYPE html><html><head><script>var f = "<body><h1>solid-start-home-marker</h1></body>";</script></head><p>no real body</p></html>`;
  assert.throws(() => bodyWithoutScripts(html), /missing <body>/);
});
