// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");

(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "domcontentloaded" });
    const res = await page.evaluate(async () => {
      const raw = (await import("/src/theme.scss?raw")).default;
      const inline = (await import("/src/data.json?inline")).default;
      return { raw, inline };
    });
    console.log("?raw starts:", JSON.stringify(res.raw.slice(0, 14)));
    console.log("?inline starts:", JSON.stringify(res.inline.slice(0, 24)));
    if (typeof res.raw !== "string" || !res.raw.includes("$brand"))
      throw new Error("?raw wrong: " + res.raw.slice(0, 40));
    if (!res.inline.startsWith("data:application/json;base64,"))
      throw new Error("?inline wrong: " + res.inline.slice(0, 40));
    console.log("RAW/INLINE VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
