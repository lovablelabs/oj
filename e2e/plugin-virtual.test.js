// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const base = "http://localhost:5199";

(async () => {
  const demo = await (await fetch(`${base}/src/virtual-demo.tsx`)).text();
  const idUrl = demo.match(/\/@id\/[a-f0-9]+\?importer=[a-f0-9]+/)?.[0];
  if (!idUrl) throw new Error("plugin resolveId did not rewrite the import to /@id/:\n" + demo);
  const mod = await (await fetch(`${base}${idUrl}`)).text();
  if (!mod.includes("hello from plugin")) throw new Error("plugin load did not provide the module:\n" + mod);
  console.log("plugin resolveId+load: virtual:plugin-greeting ->", idUrl.split("?")[0]);
  console.log("PLUGIN resolveId/load HOOKS VERIFIED");
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
