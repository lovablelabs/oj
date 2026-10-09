// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Shared e2e wait helpers: deadline-based, never index-counted.

import fs from "node:fs";

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Teardown rm for app dirs a killed server's cache writes can still land in
// for a beat. The async rm, not rmSync: on ENOTEMPTY, sync rimraf clears the
// children once and then only retries the bare rmdir, so a single straggler
// file defeats every retry; the async retry re-runs the whole traversal per
// attempt, removing stragglers as it goes.
export const rmrf = (dir) => fs.promises.rm(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });

// Poll `cond` (sync or async) until truthy, against a wall-clock deadline.
// `touch` re-fires a probe write on the way: oj answers HTTP before its
// watcher thread has registered every watch, so a single write can predate
// registration and never be seen; rewriting the same bytes bumps the mtime
// and re-fires it (Vite's chokidar scan has the same race). Resolves false
// on timeout so callers keep their own failure message.
export async function settles(cond, { timeoutMs = 45000, pollMs = 100, touch, touchEveryMs = 3000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let touched = Date.now();
  while (!(await cond())) {
    if (Date.now() >= deadline) return false;
    if (touch && Date.now() - touched >= touchEveryMs) {
      touch();
      touched = Date.now();
    }
    await sleep(pollMs);
  }
  return true;
}

// Wait until `url` answers: `res.ok` by default, or a custom `until(res)`
// (which may read the body). Throws on timeout with the last failure, and
// fails fast when `proc` has already exited. Returns the accepted response.
// `init` may be a function, evaluated per attempt (a fresh AbortSignal each
// try; a single one would abort every retry after its first deadline).
export async function waitUp(url, { timeoutMs = 120000, pollMs = 200, until, proc, init } = {}) {
  const deadline = Date.now() + timeoutMs;
  let last = "no response";
  for (;;) {
    if (proc && proc.exitCode !== null) {
      throw new Error(`server exited with ${proc.exitCode} before ${url} answered`);
    }
    try {
      const res = await fetch(url, typeof init === "function" ? init() : init);
      if (until ? await until(res) : res.ok) return res;
      last = `status ${res.status}`;
    } catch (e) {
      last = e?.cause?.code ?? e?.message ?? String(e);
    }
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${url} (${last})`);
    await sleep(pollMs);
  }
}
