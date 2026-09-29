// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const http = require("node:http");

(async () => {
  const backend = http.createServer((req, res) => {
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify({ ok: true, path: req.url }));
  });
  await new Promise((r) => backend.listen(8899, r));

  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "domcontentloaded" });
    const body = await page.evaluate(async () => {
      const res = await fetch("/api/ping");
      return res.json();
    });
    console.log("proxied response:", JSON.stringify(body));
    if (!body.ok) throw new Error("proxy did not reach backend");
    if (body.path !== "/ping") throw new Error("rewrite ^/api failed, got: " + body.path);
    console.log("PROXY VERIFIED");
  } finally {
    await browser.close();
    backend.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
