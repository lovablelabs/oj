// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("fs");
const CSS = require("path").join(__dirname, "..", "playground") + "/src/Counter.module.css";
(async () => {
  const original = fs.readFileSync(CSS, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    const btn = page.locator("button");
    await btn.waitFor();
    const cls = await btn.getAttribute("class");
    if (!cls || !cls.includes("button_")) throw new Error("scoped class missing: " + cls);
    const bg0 = await btn.evaluate((el) => getComputedStyle(el).backgroundColor);
    if (bg0 !== "rgb(230, 240, 255)") throw new Error("module css not applied: " + bg0);
    await btn.click();
    await btn.click();
    await btn.click();

    fs.writeFileSync(CSS, original.replace("rgb(230, 240, 255)", "rgb(255, 235, 205)"));
    await page.waitForFunction(
      () => getComputedStyle(document.querySelector("button")).backgroundColor === "rgb(255, 235, 205)",
      { timeout: 8000 },
    );
    const text = await btn.textContent();
    console.log(
      "scoped class:",
      cls.trim(),
      "| after css edit:",
      text.trim(),
      "| errors:",
      errors.length ? errors : "none",
    );
    if (!text.includes("Clicks: 3")) throw new Error("STATE LOST");
    if (errors.length) throw new Error("console errors");
    console.log("CSS MODULES + HMR VERIFIED");
  } finally {
    fs.writeFileSync(CSS, original);
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
