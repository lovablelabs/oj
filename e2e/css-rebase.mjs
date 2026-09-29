// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the production build rebases relative CSS url() refs to emitted,
// content-hashed assets (they'd otherwise 404 once CSS is concatenated into
// /assets/style-*.css). Standalone: runs `oj build` on a temp fixture.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cssrebase-"));

let failed = false;
try {
  fs.mkdirSync(path.join(app, "src", "img"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"cssrebase","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.tsx"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "main.tsx"), 'import "./style.css";\ndocument.body.dataset.ok = "1";\n');
  // A real referenced asset (font/image) plus a data: url that must be left alone.
  // Larger than build.assetsInlineLimit (4kB): a smaller file would be inlined as a data URL, as in Vite.
  fs.writeFileSync(
    path.join(app, "src", "img", "bg.png"),
    Buffer.concat([Buffer.from("PNGDATA-oj-cssrebase"), Buffer.alloc(5000, 1)]),
  );
  fs.writeFileSync(
    path.join(app, "src", "style.css"),
    ".hero{background:url(./img/bg.png)}.d{background:url(data:image/gif;base64,AA)}",
  );

  execFileSync(OJ, ["build", app], { stdio: "pipe" });

  const assetsDir = path.join(app, "dist", "assets");
  const files = fs.readdirSync(assetsDir);
  // cssCodeSplit names the stylesheet after its chunk (e.g. main-*.css).
  const styleFile = files.find((f) => /\.css$/.test(f));
  if (!styleFile) throw new Error("no .css emitted; got: " + files.join(", "));
  const css = fs.readFileSync(path.join(assetsDir, styleFile), "utf8");

  const rebased = css.match(/url\("\/assets\/bg-[0-9a-f]+\.png"\)/);
  if (!rebased) throw new Error("relative url() not rebased to a hashed asset:\n" + css);
  if (css.includes("./img/bg.png")) throw new Error("original relative url() still present");
  if (!css.includes("url(data:image/gif;base64,AA)")) throw new Error("data: url() should be left untouched");

  const emittedAsset = files.find((f) => /^bg-[0-9a-f]+\.png$/.test(f));
  if (!emittedAsset) throw new Error("referenced png not emitted into assets/; got: " + files.join(", "));
  const bytes = fs.readFileSync(path.join(assetsDir, emittedAsset), "utf8");
  if (!bytes.includes("PNGDATA-oj-cssrebase")) throw new Error("emitted png has wrong content");

  console.log("rebased url():     ", rebased[0]);
  console.log("asset emitted:     ", emittedAsset);
  console.log("data: url intact:   yes");
  console.log("\nCSS REBASE VERIFIED: relative url() -> hashed /assets, data: left alone");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
