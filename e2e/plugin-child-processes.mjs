// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The child-process contract around the oj_deno_process fork: an IPC round
// trip works (the fork must SHARE upstream's ipc resource types — a second
// copy makes deno_node's ops fail with "Bad resource ID"), a detached child
// outlives restarts AND shutdown (never registered for the kill sweep), a
// plain child dies across the self-restart exec, SIGINT and SIGHUP are
// forwarded to own-group children (they no longer sit in the terminal's
// foreground group), and a SYNC spawn stays in the caller's group.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5327;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-childproc-"));
const p = (rel) => path.join(app, rel);
fs.writeFileSync(p("package.json"), '{"name":"childproc","private":true,"type":"module"}');
fs.writeFileSync(
  p("index.html"),
  '<!doctype html><html><body><script type="module" src="/main.js"></script></body></html>',
);
fs.writeFileSync(p("main.js"), "export const ok = 1;\n");
fs.writeFileSync(p(".env"), "VITE_FOO=1\n");
fs.writeFileSync(
  p("oj.plugins.mjs"),
  `import { execSync, spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
export default [{
  name: "test:children",
  configureServer() {
    // A sync spawn must STAY in the caller's process group (re-grouping a
    // blocking child risks SIGTTOU stops on an inherited terminal). The sh
    // child reports its own pgid + pid and the server's pgid.
    const [childPgid, childPid, serverPgid] = execSync(
      \`ps -o pgid= -p $$; echo $$; ps -o pgid= -p \${process.pid}\`,
    ).toString().split("\\n").map(Number);
    writeFileSync(new URL("./sync.json", import.meta.url), JSON.stringify({ childPgid, childPid, serverPgid }));

    // IPC round trip (node from PATH: process.execPath here is the oj binary).
    const ipc = spawn("node", ["-e", "process.on('message', (m) => { process.send({ echo: m }); process.exit(0); })"], {
      stdio: ["ignore", "ignore", "ignore", "ipc"],
    });
    ipc.on("message", (m) => writeFileSync(new URL("./ipc.json", import.meta.url), JSON.stringify(m)));
    ipc.on("exit", (c) => writeFileSync(new URL("./ipc-exit.txt", import.meta.url), String(c)));
    ipc.on("error", (e) => writeFileSync(new URL("./ipc-error.txt", import.meta.url), String(e)));
    // A string payload: numbers cross the engine's ipc as serde_json's
    // arbitrary_precision token (pre-existing upstream quirk, same on main).
    ipc.send({ hello: "world" });

    // A long-lived plain child (killed on restart/shutdown) and a DETACHED
    // one (must outlive both).
    const plain = spawn("sleep", ["300"], { stdio: "ignore" });
    writeFileSync(new URL("./plain.pid", import.meta.url), String(plain.pid));
    const detached = spawn("sleep", ["300"], { detached: true, stdio: "ignore" });
    writeFileSync(new URL("./detached.pid", import.meta.url), String(detached.pid));
    detached.unref();
  },
}];\n`,
);

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};
const reaped = (pid) => settles(() => !alive(pid), { timeoutMs: 15000 });

let failed = false;
let stderr = "";
let srv2;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "ignore", "pipe"] });
srv.stderr.on("data", (d) => (stderr += d));
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });

  // 1. IPC: the echo lands (fails with "Bad resource ID" when the fork
  //    carries its own ipc types).
  const gotIpc = await settles(() => fs.existsSync(p("ipc.json")));
  must(
    gotIpc,
    `no IPC echo: err=${fs.existsSync(p("ipc-error.txt")) ? fs.readFileSync(p("ipc-error.txt"), "utf8") : "-"} exit=${fs.existsSync(p("ipc-exit.txt")) ? fs.readFileSync(p("ipc-exit.txt"), "utf8") : "-"}\n${stderr.slice(-2000)}`,
  );
  const echoed = JSON.parse(fs.readFileSync(p("ipc.json"), "utf8"));
  must(echoed?.echo?.hello === "world", `IPC echo carried the wrong payload: ${JSON.stringify(echoed)}`);
  console.log("ipc round trip:     ok");

  // Sync spawns keep the caller's group: never a group leader of their own.
  const sync = JSON.parse(fs.readFileSync(p("sync.json"), "utf8"));
  must(
    sync.childPgid === sync.serverPgid && sync.childPgid !== sync.childPid,
    `sync spawn left the caller's process group: ${JSON.stringify(sync)}`,
  );
  console.log("sync spawn group:   kept");

  const plainPid = Number(fs.readFileSync(p("plain.pid"), "utf8"));
  const detachedPid = Number(fs.readFileSync(p("detached.pid"), "utf8"));
  must(alive(plainPid) && alive(detachedPid), "both children should be running");

  // 2. Self-restart: plain child dies, detached survives.
  fs.appendFileSync(p(".env"), "VITE_BAR=2\n");
  await settles(() => /restarting dev server/i.test(stderr));
  must(await reaped(plainPid), "plain child survived the self-restart");
  must(alive(detachedPid), "detached child must OUTLIVE the self-restart");
  console.log("restart contract:   plain killed, detached survived");

  // 3. Ctrl-C: forwarded to the new boot's plain child; detached still safe.
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  await settles(() => {
    try {
      return Number(fs.readFileSync(p("plain.pid"), "utf8")) !== plainPid;
    } catch {
      return false;
    }
  });
  const plain2 = Number(fs.readFileSync(p("plain.pid"), "utf8"));
  srv.kill("SIGINT");
  must(await reaped(plain2), "plain child survived Ctrl-C (signal not forwarded)");
  must(alive(detachedPid), "detached child must outlive Ctrl-C");
  console.log("sigint forwarding:  ok");

  // 4. SIGHUP: the same forwarding contract (own-group children left the
  //    terminal session, so nothing dies with it implicitly).
  try {
    process.kill(Number(fs.readFileSync(p("detached.pid"), "utf8")), "SIGKILL");
  } catch {}
  srv2 = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "ignore", "pipe"] });
  await waitUp(`http://localhost:${PORT}/`, { proc: srv2 });
  await settles(() => {
    try {
      const pid = Number(fs.readFileSync(p("plain.pid"), "utf8"));
      return pid !== plain2 && alive(pid);
    } catch {
      return false;
    }
  });
  const plain3 = Number(fs.readFileSync(p("plain.pid"), "utf8"));
  srv2.kill("SIGHUP");
  must(await reaped(plain3), "plain child survived SIGHUP (signal not forwarded)");
  console.log("sighup forwarding:  ok");

  try {
    process.kill(detachedPid, "SIGKILL");
  } catch {}
  console.log("\nPLUGIN CHILD PROCESSES VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  try {
    srv.kill("SIGKILL");
  } catch {}
  try {
    srv2?.kill("SIGKILL");
  } catch {}
  for (const f of ["plain.pid", "detached.pid"]) {
    try {
      process.kill(Number(fs.readFileSync(p(f), "utf8")), "SIGKILL");
    } catch {}
  }
  fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
