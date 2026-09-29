// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    await page.waitForSelector("[data-json]", { state: "attached" });
    const v = await page.getAttribute("[data-json]", "data-json");
    console.log("data-json:", v, "| errors:", errors.length ? errors : "none");
    if (v !== "oj playground|7") throw new Error("json import wrong: " + v);
    if (errors.length) throw new Error("console errors");
    console.log("JSON IMPORTS VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
