// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("fs");

const APP = require("path").join(__dirname, "..", "playground") + "/src/App.tsx";
const URL = "http://localhost:5199/";

(async () => {
  const original = fs.readFileSync(APP, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  const logs = [];
  page.on("console", (m) => {
    logs.push(`[${m.type()}] ${m.text()}`);
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push(String(e)));
  process.on("exit", () => {
    if (process.exitCode) console.error("browser console:\n" + logs.join("\n"));
  });

  try {
    await page.goto(URL, { waitUntil: "networkidle" });
    await page.waitForSelector("h1:has-text('oj playground')", { timeout: 15000 });

    await page.evaluate(() => {
      window.__no_reload_marker = 42;
    });

    const button = page.locator("button");
    await button.click();
    await button.click();
    await button.click();
    if (!(await button.textContent()).includes("Clicks: 3")) {
      throw new Error("counter did not reach 3: " + (await button.textContent()));
    }
    await page.screenshot({ path: "before-edit.png" });

    fs.writeFileSync(APP, original.replace("oj playground", "oj playground — HOT"));
    await page.waitForSelector("h1:has-text('oj playground — HOT')", { timeout: 10000 });

    const afterText = await button.textContent();
    const marker = await page.evaluate(() => window.__no_reload_marker);
    await page.screenshot({ path: "after-edit.png" });

    console.log("heading updated:      yes");
    console.log("counter after edit:  ", afterText.trim());
    console.log("page reloaded:       ", marker === 42 ? "no (marker intact)" : "YES (state lost)");
    console.log("console errors:      ", errors.length ? errors.join("\n") : "none");

    if (!afterText.includes("Clicks: 3")) throw new Error("STATE LOST: " + afterText);
    if (marker !== 42) throw new Error("PAGE RELOADED instead of hot update");
    if (errors.length) throw new Error("console errors present");
    console.log("\nFAST REFRESH VERIFIED: state preserved through hot edit");
  } finally {
    fs.writeFileSync(APP, original);
    await browser.close();
  }
})().catch((err) => {
  console.error("FAIL:", err.message);
  process.exit(1);
});
