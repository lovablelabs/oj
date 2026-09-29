// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("node:fs");
const path = require("node:path");
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    await page.waitForSelector("[data-plugin]", { state: "attached" });
    const v = await page.getAttribute("[data-plugin]", "data-plugin");
    const cfg = await page.getAttribute("[data-plugin-config]", "data-plugin-config");
    console.log("data-plugin:", v, "| data-plugin-config:", cfg, "| errors:", errors.length ? errors : "none");
    if (v !== "transformed-by-plugin") throw new Error("plugin transform did not run: " + v);
    if (cfg !== "development:oj-plugin") throw new Error("config/configResolved handshake failed: " + cfg);
    const injected = await page.locator('meta[name="oj-plugin-injected"]').getAttribute("content");
    const title = await page.title();
    if (injected !== "yes") throw new Error("transformIndexHtml did not inject the meta tag");
    if (!title.includes("(plugin)")) throw new Error("transformIndexHtml did not rewrite the title: " + title);
    const order = await page.getAttribute("[data-order]", "data-order");
    if (order !== "base-pre-post") throw new Error("enforce ordering wrong: " + order);
    const apply = await page.getAttribute("[data-apply]", "data-apply");
    if (apply !== "serve-only") throw new Error("apply gating wrong (expected serve-only): " + apply);
    const resolved = await page.getAttribute("[data-resolved]", "data-resolved");
    if (resolved !== "Counter.tsx") throw new Error("this.resolve wrong (expected Counter.tsx): " + resolved);
    const modinfo = await page.getAttribute("[data-modinfo]", "data-modinfo");
    if (modinfo !== "3:true") throw new Error("getModuleInfo/this.load wrong (expected 3:true): " + modinfo);
    const moduleids = await page.getAttribute("[data-moduleids]", "data-moduleids");
    if (moduleids !== "2") throw new Error("getModuleIds wrong (expected 2): " + moduleids);
    const marker = path.join(__dirname, "..", "playground", ".oj-cache", "plugin-buildstart");
    const buildStart = fs.existsSync(marker) ? fs.readFileSync(marker, "utf8").trim() : "MISSING";
    if (buildStart !== "serve") throw new Error("buildStart marker wrong (expected serve): " + buildStart);
    const envName = await page.getAttribute("[data-env-name]", "data-env-name");
    if (envName !== "client") throw new Error("this.environment.name wrong (expected client): " + envName);
    const envMode = await page.getAttribute("[data-env-mode]", "data-env-mode");
    if (envMode !== "dev") throw new Error("this.environment.mode wrong (expected dev, not development): " + envMode);
    // transformIndexHtml: default injectTo is head-prepend (before head-append tags),
    // and object-form hooks honor order:'pre'/'post'.
    const rawHtml = await (await page.request.get("http://localhost:5199/")).text();
    const headOpen = rawHtml.indexOf("<head>");
    const defaultAt = rawHtml.indexOf('name="oj-html-default"');
    const appendAt = rawHtml.indexOf('name="oj-plugin-injected"');
    if (defaultAt === -1 || headOpen === -1 || defaultAt < headOpen)
      throw new Error("head-prepend tag missing or before <head>");
    if (appendAt === -1 || defaultAt > appendAt)
      throw new Error("default injectTo should be head-prepend (before the head-append tag)");
    const seq = await page.locator('meta[name="oj-html-seq"]').getAttribute("content");
    if (seq !== "pre-post") throw new Error("transformIndexHtml order pre/post not honored: " + seq);
    const envClient = await page.getAttribute("[data-env-client]", "data-env-client");
    if (envClient !== "client-ran") throw new Error("applyToEnvironment client gate wrong: " + envClient);
    const envSsr = await page.getAttribute("[data-env-ssr]", "data-env-ssr");
    if (envSsr !== "__ENV_SSR__") throw new Error("applyToEnvironment ssr plugin should NOT run in client: " + envSsr);
    const define = await page.getAttribute("[data-define]", "data-define");
    if (define !== "global-define|client-define")
      throw new Error("per-environment define wrong (expected global-define|client-define): " + define);
    const health = await page.request.get("http://localhost:5199/__oj_health");
    const healthText = (await health.text()).trim();
    if (healthText !== "oj-plugin-mw-ok") throw new Error("configureServer middleware wrong: " + healthText);
    const parsed = (await (await page.request.get("http://localhost:5199/__oj_parsed")).text()).trim();
    if (!parsed.split(",").includes("App.tsx")) throw new Error("moduleParsed did not record App.tsx: " + parsed);
    if (errors.length) throw new Error("console errors");
    console.log(
      "PLUGIN transform + config + transformIndexHtml (head-prepend default + pre/post order) + enforce/apply + buildStart + this.resolve + getModuleInfo + getModuleIds + configureServer + Environment API (name+mode) + per-env define HOOKS VERIFIED",
    );
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
