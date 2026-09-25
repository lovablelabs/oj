// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("fs");
const path = require("path");
const APP = path.join(__dirname, "..", "playground", "src", "App.tsx");

(async () => {
  const original = fs.readFileSync(APP, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    await page.waitForSelector("h1");

    const echoed = await page.evaluate(async () => {
      const { createHotContext } = await import("/@oj/client.js");
      const hot = createHotContext("/__hot_api_test");
      return await new Promise((resolve) => {
        hot.on("oj:echo", (d) => resolve(d));
        setTimeout(() => hot.send("oj:echo", { n: 7 }), 50);
      });
    });
    console.log("custom round-trip:", JSON.stringify(echoed));
    if (!echoed || echoed.n !== 7) throw new Error("send/on round-trip failed");

    const afterUpdate = page.evaluate(
      () =>
        new Promise((resolve) =>
          import("/@oj/client.js").then(({ createHotContext }) => {
            createHotContext("/__hot_api_test2").on("vite:afterUpdate", () => resolve(true));
          })
        )
    );
    await page.waitForTimeout(100);
    fs.writeFileSync(APP, original.replace("oj playground", "oj playground HOTAPI"));
    const fired = await Promise.race([
      afterUpdate,
      new Promise((r) => setTimeout(() => r(false), 8000)),
    ]);
    console.log("vite:afterUpdate fired:", fired);
    if (!fired) throw new Error("vite:afterUpdate did not fire");
    console.log("HOT API VERIFIED");
  } finally {
    fs.writeFileSync(APP, original);
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
