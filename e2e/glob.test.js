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
    await page.waitForSelector("[data-pages]", { state: "attached" });
    const count = await page.getAttribute("[data-pages]", "data-pages");
    console.log("glob matched pages:", count, "| errors:", errors.length ? errors : "none");
    if (count !== "2") throw new Error("expected 2 glob matches, got " + count);
    if (errors.length) throw new Error("console errors");
    console.log("IMPORT.META.GLOB VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
