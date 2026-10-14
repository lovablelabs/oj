// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// css.preprocessorOptions under dev SSR: a Sass CSS module that uses a
// variable from `scss.additionalData` must compile on the server as it does
// for the client (Vite's compileCSS applies the same preprocessor options to
// every consumer), and both must name the same scoped class. The server once
// compiled Sass without the options, so the variable was undefined there.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const playground = path.join(repo, "playground");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5398);

if (!fs.existsSync(path.join(playground, "node_modules", "react"))) {
  console.log("SKIP ssr css preprocessor options: playground deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const counterFile = path.join(playground, "src", "Counter.tsx");
const originalCounter = fs.readFileSync(counterFile, "utf8");
const configFile = path.join(playground, "oj.config.ts");
const originalConfig = fs.readFileSync(configFile, "utf8");
const toneFile = path.join(playground, "src", "Tone.module.scss");
let srv;
let log = "";
try {
  fs.writeFileSync(toneFile, `.tone { color: $tone; }\n`);
  fs.writeFileSync(
    configFile,
    originalConfig.replace(
      "export default defineConfig({",
      `export default defineConfig({\n  css: { preprocessorOptions: { scss: { additionalData: "$tone: rgb(31, 32, 33);" } } },`,
    ),
  );
  fs.writeFileSync(
    counterFile,
    originalCounter
      .replace(
        'import styles from "./Counter.module.css";',
        'import styles from "./Counter.module.css";\nimport tone from "./Tone.module.scss";',
      )
      .replace("{label}: {count}", '{label}: {count}<span className={tone.tone} data-tone="x">tone</span>'),
  );
  must(
    fs.readFileSync(counterFile, "utf8").includes("tone.tone") && fs.readFileSync(configFile, "utf8").includes("$tone"),
    "playground patch did not apply (fixture drifted)",
  );
  fs.rmSync(path.join(playground, ".oj-cache", "v1", "ssr"), { recursive: true, force: true });

  srv = spawn(oj, ["dev", playground, "--ssr", "src/entry-server.tsx", "--port", String(PORT)], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));

  let body = "";
  await settles(async () => {
    try {
      body = await (await fetch(`http://localhost:${PORT}/`)).text();
      return body.includes('data-tone="x"') || body.includes("[oj ssr]");
    } catch {
      return false;
    }
  });
  const ssrClass = /<span class="([^"]*)" data-tone="x">/.exec(body)?.[1] ?? "";
  must(
    ssrClass.includes("tone") && ssrClass !== "tone",
    `SSR rendered no scoped class: "${ssrClass}"\n${body.slice(0, 1200)}\n---\n${log.slice(-1500)}`,
  );

  // The client's module for the same file: the same class, the compiled color.
  const client = await (await fetch(`http://localhost:${PORT}/src/Tone.module.scss`)).text();
  must(
    client.includes(JSON.stringify(ssrClass)),
    `client module does not name "${ssrClass}":\n${client.slice(0, 800)}`,
  );
  must(
    /rgb\(31, 32, 33\)|#1f2021/.test(client) && !client.includes("$tone"),
    `client sheet not compiled:\n${client.slice(0, 800)}`,
  );
  console.log(`ssr dev: ${ssrClass} on server and client, additionalData applied`);
  console.log("\nSSR CSS PREPROCESSOR OPTIONS PASSED");
} catch (e) {
  console.error("\nSSR CSS PREPROCESSOR OPTIONS FAILED:", e.message);
  process.exitCode = 1;
} finally {
  fs.writeFileSync(counterFile, originalCounter);
  fs.writeFileSync(configFile, originalConfig);
  fs.rmSync(toneFile, { force: true });
  srv?.kill("SIGKILL");
}
