// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// oxlint's type-aware rules on tsgolint against the same rules on oj
// (`oj lint`): wall-clock time and peak memory of a whole `oxlint --type-aware`
// run over a project, medians of alternating runs.
//
//   node bench/lint.mjs <project> [runs]
//
//   OJ_BIN           the oj binary (default: target/release/oj)
//   BENCH_OXLINT     the oxlint to run (default: <project>/node_modules/.bin/oxlint)
//   BENCH_TSGOLINT   the tsgolint binary (default: the one oxlint finds in the
//                    project, its `oxlint-tsgolint` package)
//   BENCH_LINT_ARGS  extra oxlint arguments, space separated
//
// Both tools run the project's own oxlint and config; only the backend
// behind the type-aware rules differs (OXLINT_TSGOLINT_PATH). Nothing is
// written to the project: no `--fix`. Peak memory is the largest process of
// the run, which is the backend (`/usr/bin/time`'s maximum resident set size).

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const project = path.resolve(process.argv[2] ?? ".");
const runs = parseInt(process.argv[3] ?? "5", 10);
const oj = path.resolve(process.env.OJ_BIN ?? path.join(here, "..", "target", "release", "oj"));
const oxlint = path.resolve(process.env.BENCH_OXLINT ?? path.join(project, "node_modules", ".bin", "oxlint"));
const extra = (process.env.BENCH_LINT_ARGS ?? "").split(" ").filter(Boolean);
for (const [what, file] of [
  ["oj", oj],
  ["oxlint", oxlint],
]) {
  if (!fs.existsSync(file)) {
    console.error(`bench/lint: no ${what} at ${file}`);
    process.exit(1);
  }
}

const mac = process.platform === "darwin";
const TOOLS = {
  tsgolint: { OXLINT_TSGOLINT_PATH: process.env.BENCH_TSGOLINT },
  oj: { OXLINT_TSGOLINT_PATH: oj },
};

// One `oxlint --type-aware` run under /usr/bin/time: seconds, peak bytes, and
// the diagnostics as "file:line:column rule" keys.
function run(tool) {
  const env = { ...process.env, ...TOOLS[tool] };
  if (env.OXLINT_TSGOLINT_PATH === undefined) delete env.OXLINT_TSGOLINT_PATH;
  const started = process.hrtime.bigint();
  const out = spawnSync("/usr/bin/time", [mac ? "-l" : "-v", oxlint, "--type-aware", "--format", "json", ...extra], {
    cwd: project,
    env,
    encoding: "utf8",
    maxBuffer: 1 << 30,
  });
  const seconds = Number(process.hrtime.bigint() - started) / 1e9;
  const rss = mac
    ? /(\d+)\s+maximum resident set size/.exec(out.stderr)
    : /Maximum resident set size \(kbytes\): (\d+)/.exec(out.stderr);
  const bytes = rss ? Number(rss[1]) * (mac ? 1 : 1024) : NaN;
  let diagnostics = null;
  try {
    const report = JSON.parse(out.stdout.slice(out.stdout.indexOf("{")));
    diagnostics = report.diagnostics.map((d) => {
      const span = d.labels?.[0]?.span ?? {};
      return `${d.filename}:${span.line ?? 0}:${span.column ?? 0} ${d.code}`;
    });
  } catch {
    // Not a report: the run failed before linting.
  }
  if (diagnostics === null) {
    console.error(`bench/lint: ${tool} printed no report (exit ${out.status}):`);
    console.error((out.stderr || out.stdout).slice(0, 2000));
    process.exit(1);
  }
  return { seconds, bytes, diagnostics };
}

const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];
const gb = (bytes) => `${(bytes / 2 ** 30).toFixed(2)} GB`;
const less = (a, b) => `${Math.round((1 - b / a) * 100)}% less`;

const samples = { tsgolint: [], oj: [] };
for (let i = 0; i < runs; i++) {
  for (const tool of Object.keys(TOOLS)) {
    const sample = run(tool);
    samples[tool].push(sample);
    console.log(
      `run ${i + 1}/${runs} ${tool.padEnd(8)} ${sample.seconds.toFixed(1)}s  ${gb(sample.bytes)}  ${sample.diagnostics.length} diagnostics`,
    );
  }
}

const stat = (tool) => ({
  seconds: median(samples[tool].map((s) => s.seconds)),
  bytes: median(samples[tool].map((s) => s.bytes)),
});
const [a, b] = [stat("tsgolint"), stat("oj")];
console.log(`\nmedians of ${runs} alternating runs, ${os.cpus()[0].model}, ${os.cpus().length} threads`);
console.log(
  `wall clock   tsgolint ${a.seconds.toFixed(1)}s   oj ${b.seconds.toFixed(1)}s   (${less(a.seconds, b.seconds)})`,
);
console.log(`peak memory  tsgolint ${gb(a.bytes)}   oj ${gb(b.bytes)}   (${less(a.bytes, b.bytes)})`);

// The same diagnostics on both? Counted from each tool's first run.
const [first, second] = [samples.tsgolint[0].diagnostics, samples.oj[0].diagnostics];
const only = (xs, ys) => {
  const seen = new Set(ys);
  return xs.filter((x) => !seen.has(x));
};
const [onlyTsgolint, onlyOj] = [only(first, second), only(second, first)];
console.log(
  `diagnostics  tsgolint ${first.length}   oj ${second.length}   only tsgolint ${onlyTsgolint.length}   only oj ${onlyOj.length}`,
);
const byRule = (keys) => {
  const counts = {};
  for (const key of keys) {
    const rule = key.slice(key.indexOf(" ") + 1);
    counts[rule] = (counts[rule] ?? 0) + 1;
  }
  return Object.entries(counts)
    .sort((x, y) => y[1] - x[1])
    .map(([rule, n]) => `${rule} ${n}`)
    .join(", ");
};
if (onlyTsgolint.length) console.log(`  only tsgolint, by rule: ${byRule(onlyTsgolint)}`);
if (onlyOj.length) console.log(`  only oj, by rule: ${byRule(onlyOj)}`);
