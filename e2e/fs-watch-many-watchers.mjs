// SPDX-License-Identifier: MIT
//
// A plugin that opens one fs.watch per file of a large tree (a build-output
// watcher does) while something keeps writing into that tree. Every watch is
// registered through the notify thread that also dispatches events; when each
// event was matched against every watcher by opening files, that thread never
// caught up under the writes, the next fs.watch blocked, and configureServer
// never returned, so the server never listened. Run with a built
// target/debug/oj (or OJ_BIN).
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 5497;
const DIRS = 200;
const FILES = 100;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-fs-watch-many-"));
const tree = path.join(app, "tree");
for (let d = 0; d < DIRS; d++) {
  fs.mkdirSync(path.join(tree, `d${d}`), { recursive: true });
  for (let f = 0; f < FILES; f++) fs.writeFileSync(path.join(tree, `d${d}`, `f${f}.js`), "x");
}
fs.mkdirSync(path.join(tree, "churn"));
fs.writeFileSync(path.join(tree, "churn", "out.js"), "0");
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "fs-watch-many", private: true, type: "module" }),
);
fs.writeFileSync(path.join(app, "index.html"), "<!doctype html><html><body>WATCHED</body></html>");
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `import fs from "node:fs";
import path from "node:path";
export default {
  plugins: [{
    name: "watch-every-file",
    configureServer() {
      const tree = path.resolve("tree");
      const started = Date.now();
      let files = 0;
      let delivered = false;
      for (const d of fs.readdirSync(tree)) {
        fs.watch(path.join(tree, d), () => {});
        for (const f of fs.readdirSync(path.join(tree, d))) {
          // The churn file's watcher proves events still reach their one
          // watcher among thousands (the dispatch, not just registration).
          const cb = d === "churn" && !delivered
            ? () => { if (!delivered) { delivered = true; console.error("CHURN EVENT DELIVERED"); } }
            : () => {};
          fs.watch(path.join(tree, d, f), cb);
          files++;
        }
      }
      console.error("WATCHED " + files + " files in " + (Date.now() - started) + "ms");
    },
  }],
};
`,
);

// The writer runs in its own process, about a write a millisecond, so the event stream does not depend on the host.
const churn = path.join(tree, "churn", "out.js");
const writer = spawn(
  process.execPath,
  [
    "-e",
    `const fs = require("fs"); let i = 0; setInterval(() => fs.writeFileSync(${JSON.stringify(churn)}, String(i++)), 1);`,
  ],
  { stdio: "ignore" },
);

let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { cwd: app, stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));

let failed = false;
try {
  const res = await waitUp(`http://localhost:${PORT}/`, { timeoutMs: 60000, proc: srv });
  const body = await res.text();
  if (!body.includes("WATCHED")) throw new Error(`GET / answered without the page:\n${body}`);
  if (!log.includes(`WATCHED ${DIRS * FILES + 1} files`))
    throw new Error(`configureServer did not watch every file:\n${log}`);
  // The writer keeps hitting the churn file; its watcher must fire.
  const deadline = Date.now() + 20000;
  while (!log.includes("CHURN EVENT DELIVERED") && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 200));
  }
  if (!log.includes("CHURN EVENT DELIVERED")) throw new Error(`no event reached the churn watcher:\n${log}`);
  console.log("FS-WATCH-MANY-WATCHERS E2E PASSED");
} catch (err) {
  failed = true;
  console.error(`FS-WATCH-MANY-WATCHERS E2E FAILED: ${err.message}\n--- server log ---\n${log}`);
} finally {
  writer.kill("SIGKILL");
  srv.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
