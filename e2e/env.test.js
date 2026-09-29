// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    await page.waitForSelector("[data-env]", { state: "attached" });
    const env = await page.getAttribute("[data-env]", "data-env");
    console.log("data-env:", env);
    const [greeting, mode, secret] = env.split("|");
    if (greeting !== "hello-from-env") throw new Error("VITE_ var missing: " + greeting);
    if (mode !== "development") throw new Error("MODE wrong: " + mode);
    if (secret !== "none") throw new Error("SECRET LEAKED: " + secret);
    console.log("ENV LOADING VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
