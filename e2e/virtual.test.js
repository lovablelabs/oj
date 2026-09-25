// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "domcontentloaded" });
    const res = await page.evaluate(async () => {
      const m = await import("/@virtual/virtual:oj-info");
      return { tool: m.tool, version: m.default.version };
    });
    console.log("virtual module:", JSON.stringify(res));
    if (res.tool !== "oj" || res.version !== 9) throw new Error("virtual module wrong: " + JSON.stringify(res));
    console.log("VIRTUAL MODULES VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
