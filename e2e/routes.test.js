// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");

(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e.message || e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "domcontentloaded" });
    const paths = await page.evaluate(async () => (await import("/src/route-demo.tsx")).paths);
    console.log("virtual:oj-routes paths:", paths, "| errors:", errors.length ? errors : "none");
    const expected = "/,/about,/boom,/crash,/deep,/users/:id";
    if (paths !== expected) throw new Error(`route manifest wrong:\n  got:      ${paths}\n  expected: ${expected}`);
    if (errors.length) throw new Error("console errors");
    console.log("FILE-BASED ROUTE MANIFEST VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
